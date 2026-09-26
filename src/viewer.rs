//! Background session that feeds the GUI: video decoding on one thread,
//! keyboard/mouse commands on another.

use std::{
    net::{Shutdown, TcpStream},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::{
    codec::AspeedCodec,
    hid::HidSession,
    jnlp::{self, ConsoleArgs},
    tls::CertPolicy,
    tokend::Tokend,
    video::{VideoEvent, VideoSession},
    web::WebSession,
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
            } => {
                let web = WebSession::login(host, CertPolicy::Insecure, username, password)?;
                let args = web.console_args();
                if let Err(error) = web.logout() {
                    warn!(%error, "ILOM web logout failed");
                }
                args
            }
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

#[derive(Debug, Clone)]
pub struct ViewerStatus {
    pub state: ConnectionState,
    pub message: String,
    pub keyboard: bool,
    pub absolute_mouse: bool,
}

impl Default for ViewerStatus {
    fn default() -> Self {
        Self {
            state: ConnectionState::Connecting,
            message: "Starting".into(),
            keyboard: false,
            absolute_mouse: true,
        }
    }
}

#[derive(Default)]
pub struct ViewerShared {
    pub latest_frame: Mutex<Option<Arc<DecodedFrame>>>,
    pub status: Mutex<ViewerStatus>,
    pub keyboard_packets_sent: AtomicU64,
    pub mouse_packets_sent: AtomicU64,
}

#[derive(Debug)]
pub enum ViewerCommand {
    Keyboard { modifiers: u8, usages: Vec<u8> },
    Keystroke { modifiers: u8, usage: u8 },
    MouseAbsolute { buttons: u8, x: u32, y: u32, width: u32, height: u32 },
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
    let input_thread = {
        let hid = hid.clone();
        let shared = shared.clone();
        thread::Builder::new()
            .name("ilom-input".into())
            .spawn(move || input_loop(commands, hid, shared))
            .expect("spawn input thread")
    };

    let mut attempt = 0_u32;
    while !stop.load(Ordering::SeqCst) {
        let result = session(&source, &shared, &stop, &sockets, &hid, &repaint);
        if let Some(session) = hid.lock().ok().and_then(|mut hid| hid.take()) {
            session.close();
        }
        sockets.lock().map(|mut s| s.clear()).ok();
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
        attempt += 1;
        let delay = Duration::from_secs(u64::from(attempt.min(6)) * 5);
        set_status(&shared, &repaint, |status| {
            status.state = ConnectionState::Reconnecting;
            status.message = format!("{message}; reconnecting in {}s", delay.as_secs());
        });
        let deadline = std::time::Instant::now() + delay;
        while std::time::Instant::now() < deadline && !stop.load(Ordering::SeqCst) {
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
    shared: &ViewerShared,
    stop: &AtomicBool,
    sockets: &Mutex<Vec<TcpStream>>,
    hid_slot: &Mutex<Option<HidSession>>,
    repaint: &Repaint,
) -> Result<()> {
    set_status(shared, repaint, |status| {
        status.state = ConnectionState::Connecting;
        status.message = "Logging in".into();
    });
    let console = source.console_args()?;
    let policy = cert_policy(&console);
    let mut tokend = Tokend::connect(&console.host, policy, &console.username, &console.secret)?;
    set_status(shared, repaint, |status| status.message = "Starting video".into());
    let mut video = VideoSession::connect(&console.host, policy, &console.username, &mut tokend)?;
    sockets.lock().unwrap().push(video.try_clone_stream()?);

    match HidSession::connect(&console.host, &console.username, &mut tokend) {
        Ok(hid) => {
            sockets.lock().unwrap().push(hid.try_clone_stream()?);
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
    tokend.close();

    set_status(shared, repaint, |status| {
        status.state = ConnectionState::Connected;
        status.message = format!("Connected to {}", console.host);
    });
    info!(host = %console.host, "console session established");

    let mut codec = AspeedCodec::new()?;
    let mut sequence = 0_u64;
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

fn input_loop(
    commands: mpsc::Receiver<ViewerCommand>,
    hid: Arc<Mutex<Option<HidSession>>>,
    shared: Arc<ViewerShared>,
) {
    for command in commands {
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
            ViewerCommand::MouseAbsolute {
                buttons,
                x,
                y,
                width,
                height,
            } => session
                .send_absolute_mouse(buttons, x, y, width, height)
                .map(|_| shared.mouse_packets_sent.fetch_add(1, Ordering::Relaxed)),
            ViewerCommand::Stop => break,
        };
        if let Err(error) = result {
            warn!(%error, "HID send failed");
        }
    }
}
