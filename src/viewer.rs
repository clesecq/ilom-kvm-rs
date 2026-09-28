//! Background session that feeds the GUI: video decoding on one thread,
//! keyboard/mouse commands on another, one more per mounted media image.

use std::{
    net::{Shutdown, TcpStream},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::{
    codec::AspeedCodec,
    hid::{self, HidSession, HidStatus},
    jnlp::{self, ConsoleArgs},
    keymap,
    scsi::{MediaImage, MediaKind},
    tls::CertPolicy,
    tokend::Tokend,
    video::{VideoEvent, VideoSession},
    vmedia::{MediaChannel, MediaStats},
    web,
};

/// Where launch parameters come from.
#[derive(Debug, Clone)]
pub enum Source {
    /// Log into the ILOM web UI and mint a fresh JNLP (supports reconnects).
    Web {
        host: String,
        username: String,
        password: String,
    },
    /// A downloaded JNLP file; its secret works for a single connection.
    Jnlp(PathBuf),
}

impl Source {
    pub fn console_args(&self) -> Result<ConsoleArgs> {
        match self {
            Self::Web {
                host,
                username,
                password,
            } => web::fetch_console_args(host, username, password),
            Self::Jnlp(path) => {
                let xml = std::fs::read_to_string(path)
                    .with_context(|| format!("read {}", path.display()))?;
                jnlp::parse(&xml)
            }
        }
    }

    fn can_reconnect(&self) -> bool {
        matches!(self, Self::Web { .. })
    }
}

pub fn cert_policy(args: &ConsoleArgs) -> CertPolicy {
    match args.fingerprint {
        Some(fingerprint) => CertPolicy::Pinned(fingerprint),
        None => {
            warn!("JNLP has no certificate fingerprint; TLS peer is not verified");
            CertPolicy::Insecure
        }
    }
}

#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub sequence: u64,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    Connected,
    Reconnecting,
    Error,
    Stopped,
}

/// State of one virtual media drive.
#[derive(Debug, Clone, Default)]
pub struct MediaStatus {
    /// File name of the image the user chose, kept across reconnects.
    pub image: Option<String>,
    pub writable: bool,
    /// The SP accepted the redirection and forwards host commands.
    pub active: bool,
    pub stats: MediaStats,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ViewerStatus {
    pub state: ConnectionState,
    pub message: String,
    pub keyboard: bool,
    pub absolute_mouse: bool,
    /// Host keyboard LED bitmap (`hid::LED_*`), once the host reports it.
    pub leds: Option<u8>,
    pub cdrom: MediaStatus,
    pub floppy: MediaStatus,
}

impl ViewerStatus {
    pub fn media(&self, kind: MediaKind) -> &MediaStatus {
        match kind {
            MediaKind::Cdrom => &self.cdrom,
            MediaKind::Floppy => &self.floppy,
        }
    }

    fn media_mut(&mut self, kind: MediaKind) -> &mut MediaStatus {
        match kind {
            MediaKind::Cdrom => &mut self.cdrom,
            MediaKind::Floppy => &mut self.floppy,
        }
    }
}

impl Default for ViewerStatus {
    fn default() -> Self {
        Self {
            state: ConnectionState::Connecting,
            message: "Starting".into(),
            keyboard: false,
            absolute_mouse: true,
            leds: None,
            cdrom: MediaStatus::default(),
            floppy: MediaStatus::default(),
        }
    }
}

#[derive(Default)]
pub struct ViewerShared {
    pub latest_frame: Mutex<Option<Arc<DecodedFrame>>>,
    pub status: Mutex<ViewerStatus>,
    pub keyboard_packets_sent: AtomicU64,
    pub mouse_packets_sent: AtomicU64,
    /// Set to abort a paste that is still being typed.
    pub cancel_typing: AtomicBool,
    /// Keystrokes of the paste being typed (0 when idle) and those sent so far.
    pub typing_total: AtomicUsize,
    pub typing_done: AtomicUsize,
    /// Set to skip the wait before the next reconnect attempt.
    pub reconnect_now: AtomicBool,
}

#[derive(Debug)]
pub enum ViewerCommand {
    Keyboard {
        modifiers: u8,
        usages: Vec<u8>,
    },
    Keystroke {
        modifiers: u8,
        usage: u8,
    },
    /// Pointer motion for SPs in relative mouse mode.
    MouseRelative {
        buttons: u8,
        dx: i8,
        dy: i8,
    },
    MouseAbsolute {
        buttons: u8,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    },
    /// Type a sequence of `(modifiers, usage)` keystrokes (clipboard paste).
    TypeStrokes(Vec<keymap::Stroke>),
    /// Redirect an image file to the host's virtual CD-ROM or floppy drive.
    Mount {
        kind: MediaKind,
        path: PathBuf,
        writable: bool,
    },
    Unmount(MediaKind),
    Stop,
}

pub struct ViewerHandle {
    pub shared: Arc<ViewerShared>,
    pub commands: mpsc::Sender<ViewerCommand>,
    stop: Arc<AtomicBool>,
    sockets: Arc<Mutex<Vec<TcpStream>>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl ViewerHandle {
    pub fn stop_and_wait(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.shared.cancel_typing.store(true, Ordering::SeqCst);
        let _ = self.commands.send(ViewerCommand::Stop);
        // Unblock threads waiting in socket reads.
        if let Ok(sockets) = self.sockets.lock() {
            for socket in sockets.iter() {
                let _ = socket.shutdown(Shutdown::Both);
            }
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for ViewerHandle {
    fn drop(&mut self) {
        self.stop_and_wait();
    }
}

type Repaint = Arc<dyn Fn() + Send + Sync>;

pub fn spawn_viewer(source: Source, repaint: impl Fn() + Send + Sync + 'static) -> ViewerHandle {
    let shared = Arc::new(ViewerShared::default());
    let (commands, receiver) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let sockets = Arc::new(Mutex::new(Vec::new()));
    let repaint: Repaint = Arc::new(repaint);
    let worker = {
        let shared = shared.clone();
        let stop = stop.clone();
        let sockets = sockets.clone();
        thread::Builder::new()
            .name("ilom-session".into())
            .spawn(move || run(source, receiver, shared, stop, sockets, repaint))
            .expect("spawn session thread")
    };
    ViewerHandle {
        shared,
        commands,
        stop,
        sockets,
        worker: Some(worker),
    }
}

fn set_status(shared: &ViewerShared, repaint: &Repaint, update: impl FnOnce(&mut ViewerStatus)) {
    if let Ok(mut status) = shared.status.lock() {
        update(&mut status);
    }
    repaint();
}

fn run(
    source: Source,
    commands: mpsc::Receiver<ViewerCommand>,
    shared: Arc<ViewerShared>,
    stop: Arc<AtomicBool>,
    sockets: Arc<Mutex<Vec<TcpStream>>>,
    repaint: Repaint,
) {
    // Keyboard/mouse commands are routed to whichever HID session is current.
    let hid: Arc<Mutex<Option<HidSession>>> = Arc::new(Mutex::new(None));
    let media = Arc::new(Media::new(shared.clone(), repaint.clone()));
    let input_thread = {
        let hid = hid.clone();
        let shared = shared.clone();
        let media = media.clone();
        thread::Builder::new()
            .name("ilom-input".into())
            .spawn(move || input_loop(commands, hid, media, shared))
            .expect("spawn input thread")
    };

    let mut attempt = 0_u32;
    while !stop.load(Ordering::SeqCst) {
        let result = session(&source, &shared, &stop, &sockets, &hid, &media, &repaint);
        let was_connected = shared
            .status
            .lock()
            .is_ok_and(|status| status.state == ConnectionState::Connected);
        media.link_down();
        if let Some(session) = hid.lock().ok().and_then(|mut hid| hid.take()) {
            session.close();
        }
        sockets.lock().map(|mut s| s.clear()).ok();
        set_status(&shared, &repaint, |status| {
            status.keyboard = false;
            status.leds = None;
        });
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let message = match &result {
            Ok(()) => "Connection closed by the SP".to_string(),
            Err(error) => format!("{error:#}"),
        };
        warn!(%message, "session ended");
        if !source.can_reconnect() {
            set_status(&shared, &repaint, |status| {
                status.state = ConnectionState::Error;
                status.message = format!("{message} (open a fresh JNLP to reconnect)");
            });
            break;
        }
        // Back off only while attempts keep failing, not after a session
        // that ran and then dropped.
        attempt = if was_connected { 1 } else { attempt + 1 };
        let delay = Duration::from_secs(u64::from(attempt.min(6)) * 5);
        let deadline = std::time::Instant::now() + delay;
        shared.reconnect_now.store(false, Ordering::SeqCst);
        let mut shown = None;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero()
                || stop.load(Ordering::SeqCst)
                || shared.reconnect_now.swap(false, Ordering::SeqCst)
            {
                break;
            }
            let seconds = left.as_secs() + 1;
            if shown != Some(seconds) {
                shown = Some(seconds);
                set_status(&shared, &repaint, |status| {
                    status.state = ConnectionState::Reconnecting;
                    status.message = format!("{message}; reconnecting in {seconds}s");
                });
            }
            thread::sleep(Duration::from_millis(200));
        }
    }
    set_status(&shared, &repaint, |status| {
        if status.state != ConnectionState::Error {
            status.state = ConnectionState::Stopped;
            status.message = "Disconnected".into();
        }
    });
    let _ = input_thread.join();
}

fn session(
    source: &Source,
    shared: &Arc<ViewerShared>,
    stop: &AtomicBool,
    sockets: &Mutex<Vec<TcpStream>>,
    hid_slot: &Mutex<Option<HidSession>>,
    media: &Arc<Media>,
    repaint: &Repaint,
) -> Result<()> {
    set_status(shared, repaint, |status| {
        status.state = ConnectionState::Connecting;
        status.message = "Logging in".into();
    });
    let console = source.console_args()?;
    let policy = cert_policy(&console);
    let mut tokend = Tokend::connect(&console.host, policy, &console.username, &console.secret)?;
    set_status(shared, repaint, |status| {
        status.message = "Starting video".into()
    });
    let mut video = VideoSession::connect(&console.host, policy, &console.username, &mut tokend)?;
    sockets.lock().unwrap().push(video.try_clone_stream()?);

    match HidSession::connect(&console.host, &console.username, &mut tokend) {
        Ok(mut hid) => {
            let stream = hid.try_clone_stream()?;
            sockets.lock().unwrap().push(stream.try_clone()?);
            spawn_status_reader(stream, shared.clone(), repaint.clone());
            // The vendor client toggles NumLock twice after connecting,
            // which makes the host report its LED state.
            if let Err(error) = hid.nudge_leds() {
                warn!(%error, "LED nudge failed");
            }
            let absolute = hid.absolute;
            *hid_slot.lock().unwrap() = Some(hid);
            set_status(shared, repaint, |status| {
                status.keyboard = true;
                status.absolute_mouse = absolute;
            });
        }
        Err(error) => {
            warn!(error = %format!("{error:#}"), "keyboard/mouse unavailable");
            set_status(shared, repaint, |status| status.keyboard = false);
        }
    }
    // Media channels ask tokend for tokens whenever an image is mounted.
    media.link_up(Link {
        host: console.host.clone(),
        tokend,
    });

    set_status(shared, repaint, |status| {
        status.state = ConnectionState::Connected;
        status.message = format!("Connected to {}", console.host);
    });
    info!(host = %console.host, "console session established");

    let mut codec = AspeedCodec::new()?;
    let mut sequence = 0_u64;
    let mut blank = false;
    let result = loop {
        if stop.load(Ordering::SeqCst) {
            break Ok(());
        }
        match video.next_event() {
            Ok(VideoEvent::Frame(frame)) => {
                let width = u32::from(frame.header.source_width);
                let height = u32::from(frame.header.source_height);
                match codec.decode(&frame) {
                    Ok(rgba) => {
                        sequence += 1;
                        if std::mem::take(&mut blank) {
                            set_status(shared, repaint, |status| {
                                status.message = format!("Connected to {}", console.host);
                            });
                        }
                        if let Ok(mut latest) = shared.latest_frame.lock() {
                            *latest = Some(Arc::new(DecodedFrame {
                                width,
                                height,
                                sequence,
                                rgba,
                            }));
                        }
                        repaint();
                    }
                    Err(error) => warn!(%error, "frame decode failed"),
                }
            }
            Ok(VideoEvent::BlankScreen) => {
                blank = true;
                set_status(shared, repaint, |status| {
                    status.message = "Host video is blank (no signal)".into();
                });
            }
            Ok(event) => tracing::debug!(?event, "video event"),
            Err(error) if stop.load(Ordering::SeqCst) => {
                tracing::debug!(%error, "video read ended by stop");
                break Ok(());
            }
            Err(error) => break Err(error),
        }
    };
    video.stop();
    result
}

/// Reads LED and status messages from the HID socket until it closes.
fn spawn_status_reader(mut stream: TcpStream, shared: Arc<ViewerShared>, repaint: Repaint) {
    let _ = stream.set_read_timeout(None);
    let spawned = thread::Builder::new()
        .name("ilom-hid-status".into())
        .spawn(move || {
            loop {
                match hid::read_status(&mut stream) {
                    Ok(HidStatus::Leds(leds)) => {
                        set_status(&shared, &repaint, |status| status.leds = Some(leds));
                    }
                    Ok(other) => tracing::debug!(?other, "HID status"),
                    Err(error) => {
                        tracing::debug!(%error, "HID status reader ended");
                        break;
                    }
                }
            }
        });
    if let Err(error) = spawned {
        warn!(%error, "could not start HID status reader");
    }
}

/// Delay between reports while typing; the SP drops keys sent back to back.
const TYPE_DELAY: Duration = Duration::from_millis(12);

fn type_strokes(
    session: &mut HidSession,
    strokes: &[keymap::Stroke],
    shared: &ViewerShared,
) -> Result<u64> {
    shared.cancel_typing.store(false, Ordering::SeqCst);
    shared.typing_done.store(0, Ordering::SeqCst);
    shared.typing_total.store(strokes.len(), Ordering::SeqCst);
    let result = type_each(session, strokes, shared);
    shared.typing_total.store(0, Ordering::SeqCst);
    result
}

fn type_each(
    session: &mut HidSession,
    strokes: &[keymap::Stroke],
    shared: &ViewerShared,
) -> Result<u64> {
    for &(modifiers, usage) in strokes {
        if shared.cancel_typing.load(Ordering::SeqCst) {
            session.send_keyboard(0, &[])?;
            break;
        }
        // Press the modifiers alone first so the host sees them before the key.
        if modifiers != 0 {
            session.send_keyboard(modifiers, &[])?;
            thread::sleep(TYPE_DELAY);
        }
        session.send_keyboard(modifiers, &[usage])?;
        thread::sleep(TYPE_DELAY);
        session.send_keyboard(0, &[])?;
        thread::sleep(TYPE_DELAY);
        shared.keyboard_packets_sent.fetch_add(1, Ordering::Relaxed);
        shared.typing_done.fetch_add(1, Ordering::SeqCst);
    }
    Ok(strokes.len() as u64)
}

fn input_loop(
    commands: mpsc::Receiver<ViewerCommand>,
    hid: Arc<Mutex<Option<HidSession>>>,
    media: Arc<Media>,
    shared: Arc<ViewerShared>,
) {
    for command in commands {
        let command = match command {
            ViewerCommand::Mount {
                kind,
                path,
                writable,
            } => {
                media.mount(kind, path, writable);
                continue;
            }
            ViewerCommand::Unmount(kind) => {
                media.unmount(kind);
                continue;
            }
            command => command,
        };
        let mut guard = match hid.lock() {
            Ok(guard) => guard,
            Err(_) => break,
        };
        let Some(session) = guard.as_mut() else {
            if matches!(command, ViewerCommand::Stop) {
                break;
            }
            continue;
        };
        let result = match command {
            ViewerCommand::Keyboard { modifiers, usages } => session
                .send_keyboard(modifiers, &usages)
                .map(|_| shared.keyboard_packets_sent.fetch_add(1, Ordering::Relaxed)),
            ViewerCommand::Keystroke { modifiers, usage } => session
                .send_keystroke(modifiers, usage)
                .map(|_| shared.keyboard_packets_sent.fetch_add(1, Ordering::Relaxed)),
            ViewerCommand::MouseRelative { buttons, dx, dy } => session
                .send_relative_mouse(buttons, dx, dy)
                .map(|_| shared.mouse_packets_sent.fetch_add(1, Ordering::Relaxed)),
            ViewerCommand::MouseAbsolute {
                buttons,
                x,
                y,
                width,
                height,
            } => session
                .send_absolute_mouse(buttons, x, y, width, height)
                .map(|_| shared.mouse_packets_sent.fetch_add(1, Ordering::Relaxed)),
            ViewerCommand::TypeStrokes(strokes) => type_strokes(session, &strokes, &shared),
            ViewerCommand::Mount { .. } | ViewerCommand::Unmount(_) => unreachable!(),
            ViewerCommand::Stop => break,
        };
        if let Err(error) = result {
            warn!(%error, "HID send failed");
        }
    }
}

/// What media channels need from the current console session.
struct Link {
    host: String,
    tokend: Tokend,
}

/// An image the user mounted. It stays wanted across reconnects until the
/// user unmounts it.
struct Mount {
    kind: MediaKind,
    path: PathBuf,
    writable: bool,
    /// Bumped on every connection attempt so stale threads stay quiet.
    generation: u64,
    stream: Option<TcpStream>,
}

/// Mounted media images and their redirection threads.
struct Media {
    link: Mutex<Option<Link>>,
    mounts: Mutex<Vec<Mount>>,
    shared: Arc<ViewerShared>,
    repaint: Repaint,
}

/// Minimum interval between status refreshes while serving commands.
const MEDIA_STATUS_INTERVAL: Duration = Duration::from_millis(250);

impl Media {
    fn new(shared: Arc<ViewerShared>, repaint: Repaint) -> Self {
        Self {
            link: Mutex::new(None),
            mounts: Mutex::new(Vec::new()),
            shared,
            repaint,
        }
    }

    fn set(&self, kind: MediaKind, update: impl FnOnce(&mut MediaStatus)) {
        set_status(&self.shared, &self.repaint, |status| {
            update(status.media_mut(kind))
        });
    }

    fn mount(self: &Arc<Self>, kind: MediaKind, path: PathBuf, writable: bool) {
        self.unmount(kind);
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let writable = writable && kind == MediaKind::Floppy;
        self.set(kind, |media| {
            *media = MediaStatus {
                image: Some(name),
                writable,
                ..Default::default()
            }
        });
        self.mounts.lock().unwrap().push(Mount {
            kind,
            path,
            writable,
            generation: 0,
            stream: None,
        });
        self.start(kind);
    }

    fn unmount(&self, kind: MediaKind) {
        let removed = {
            let mut mounts = self.mounts.lock().unwrap();
            let index = mounts.iter().position(|mount| mount.kind == kind);
            index.map(|index| mounts.remove(index))
        };
        if let Some(stream) = removed.and_then(|mount| mount.stream) {
            let _ = stream.shutdown(Shutdown::Both);
            info!(kind = kind.label(), "media unmounted");
        }
        self.set(kind, |media| *media = MediaStatus::default());
    }

    /// A console session is up: (re)connect every wanted image.
    fn link_up(self: &Arc<Self>, link: Link) {
        *self.link.lock().unwrap() = Some(link);
        let kinds: Vec<MediaKind> = self
            .mounts
            .lock()
            .unwrap()
            .iter()
            .map(|mount| mount.kind)
            .collect();
        for kind in kinds {
            self.start(kind);
        }
    }

    /// The console session ended: close tokend and the media connections,
    /// keeping the mounts for the next session.
    fn link_down(&self) {
        if let Some(link) = self.link.lock().unwrap().take() {
            link.tokend.close();
        }
        for mount in self.mounts.lock().unwrap().iter_mut() {
            if let Some(stream) = mount.stream.take() {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
    }

    fn start(self: &Arc<Self>, kind: MediaKind) {
        let started = {
            let mut mounts = self.mounts.lock().unwrap();
            mounts
                .iter_mut()
                .find(|mount| mount.kind == kind)
                .map(|mount| {
                    mount.generation += 1;
                    (mount.path.clone(), mount.writable, mount.generation)
                })
        };
        let Some((path, writable, generation)) = started else {
            return;
        };
        if self.link.lock().unwrap().is_none() {
            return;
        }
        let media = self.clone();
        let spawned = thread::Builder::new()
            .name(format!("ilom-{}", kind.label()))
            .spawn(move || {
                let result = media.run_mount(kind, &path, writable, generation);
                media.finished(kind, generation, result);
            });
        if let Err(error) = spawned {
            self.set(kind, |media| media.error = Some(format!("{error}")));
        }
    }

    fn is_current(&self, kind: MediaKind, generation: u64) -> bool {
        self.mounts
            .lock()
            .unwrap()
            .iter()
            .any(|mount| mount.kind == kind && mount.generation == generation)
    }

    fn run_mount(
        &self,
        kind: MediaKind,
        path: &std::path::Path,
        writable: bool,
        generation: u64,
    ) -> Result<()> {
        let mut image = MediaImage::open(path, kind, writable)?;
        let (host, token) = {
            let mut link = self.link.lock().unwrap();
            let link = link.as_mut().context("not connected")?;
            let token = link
                .tokend
                .redirection_token()
                .context("request a media token from tokend")?;
            (link.host.clone(), token)
        };
        let mut channel = MediaChannel::connect(&host, kind, &token)?;
        {
            let mut mounts = self.mounts.lock().unwrap();
            match mounts
                .iter_mut()
                .find(|mount| mount.kind == kind && mount.generation == generation)
            {
                Some(mount) => mount.stream = Some(channel.try_clone_stream()?),
                // Unmounted while connecting.
                None => {
                    channel.close();
                    return Ok(());
                }
            }
        }
        self.set(kind, |media| {
            media.active = true;
            media.error = None;
        });
        let mut last = Instant::now();
        channel.serve(&mut image, |stats| {
            if last.elapsed() >= MEDIA_STATUS_INTERVAL && self.is_current(kind, generation) {
                last = Instant::now();
                let stats = *stats;
                self.set(kind, |media| media.stats = stats);
            }
        })
    }

    fn finished(&self, kind: MediaKind, generation: u64, result: Result<()>) {
        let still_wanted = {
            let mut mounts = self.mounts.lock().unwrap();
            match mounts
                .iter_mut()
                .find(|mount| mount.kind == kind && mount.generation == generation)
            {
                Some(mount) => {
                    mount.stream = None;
                    true
                }
                None => false,
            }
        };
        if !still_wanted {
            return;
        }
        let connected = self.link.lock().unwrap().is_some();
        let message = match result {
            Ok(()) if !connected => "waiting for the console to reconnect".to_string(),
            Ok(()) => "the SP closed the media connection".to_string(),
            Err(error) => format!("{error:#}"),
        };
        warn!(kind = kind.label(), %message, "media redirection ended");
        self.set(kind, |media| {
            media.active = false;
            media.error = Some(message);
        });
    }
}
