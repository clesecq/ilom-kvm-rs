//! Blocking operations on the shared ILOM console session.
//!
//! The session core is thread based, so every method here blocks; the MCP
//! tools call them through `spawn_blocking`.

use std::{
    io::Cursor,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use ilom_kvm_core::{
    hid,
    keymap::{self, Layout},
    scsi::MediaKind,
    session::{ConnectionState, DecodedFrame, ViewerCommand, ViewerStatus},
};
use ilom_vnc::hub::{Attachment, Hub};
use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};

/// The first picture takes about 10 s after the web login.
const READY_TIMEOUT: Duration = Duration::from_secs(60);
const MEDIA_TIMEOUT: Duration = Duration::from_secs(20);
const CLICK_HOLD: Duration = Duration::from_millis(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    fn bit(self) -> u8 {
        match self {
            Self::Left => hid::BUTTON_LEFT,
            Self::Right => hid::BUTTON_RIGHT,
            Self::Middle => hid::BUTTON_MIDDLE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pointer {
    Move,
    Click,
    Double,
}

/// A screenshot: PNG bytes, the frame size in pixels and the connection
/// state it was taken in.
pub struct Screenshot {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub state: ConnectionState,
    pub message: String,
}

impl Screenshot {
    /// The picture may be old: the session is not connected right now.
    pub fn stale(&self) -> bool {
        self.state != ConnectionState::Connected
    }
}

/// The latest frame and the connection state it was seen in.
pub struct Capture {
    pub frame: Arc<DecodedFrame>,
    pub state: ConnectionState,
    pub message: String,
}

impl Capture {
    /// The picture may be old: the session is not connected right now.
    pub fn stale(&self) -> bool {
        self.state != ConnectionState::Connected
    }
}

/// Where `mount_media` may read from, and whether images may be written.
#[derive(Debug, Clone, Default)]
pub struct MediaPolicy {
    /// Only files below this directory can be mounted. `None` refuses all.
    pub dir: Option<PathBuf>,
    pub allow_writable: bool,
}

impl MediaPolicy {
    /// Resolves `path` (relative paths start at the media directory) and
    /// checks it stays inside the directory once symlinks are followed.
    pub fn resolve(&self, path: &Path, writable: bool) -> Result<PathBuf, String> {
        let dir = self.dir.as_ref().ok_or(
            "mounting is disabled for this host: set media_dir in the hosts file, or start ilom-mcp \
             with --media-dir (ILOM_MCP_MEDIA_DIR)",
        )?;
        if writable && !self.allow_writable {
            return Err(
                "writable media is disabled for this host: set allow_writable_media in the hosts file, \
                 or start ilom-mcp with --allow-writable-media"
                .into(),
            );
        }
        let root = dir
            .canonicalize()
            .map_err(|error| format!("media directory {}: {error}", dir.display()))?;
        let resolved = root
            .join(path)
            .canonicalize()
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if !resolved.starts_with(&root) {
            return Err(format!(
                "{} is outside the media directory {}",
                path.display(),
                root.display()
            ));
        }
        if !resolved.is_file() {
            return Err(format!("{} is not a file", resolved.display()));
        }
        Ok(resolved)
    }
}

pub struct Console {
    hub: Arc<Hub>,
    layout: Layout,
    policy: MediaPolicy,
    /// One input tool at a time: they wait on shared packet counters and on
    /// the paste in progress.
    input: Mutex<()>,
    /// Keeps the session open while media is mounted: the mounts live and
    /// die with the session, which the hub closes when nobody is attached.
    pin: Mutex<Option<Attachment>>,
}

impl Console {
    pub fn new(hub: Arc<Hub>, layout: Layout, policy: MediaPolicy) -> Self {
        Self {
            hub,
            layout,
            policy,
            input: Mutex::new(()),
            pin: Mutex::new(None),
        }
    }

    /// Connection state and message of the running session, without
    /// starting one.
    pub fn peek(&self) -> Option<(ConnectionState, String)> {
        let shared = self.hub.peek()?;
        let status = shared.status.lock().ok()?;
        Some((status.state, status.message.clone()))
    }

    fn input(&self) -> MutexGuard<'_, ()> {
        self.input
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Attaches to the session (starting it if needed) and waits for the
    /// first picture.
    fn attach(&self) -> Result<Attachment, String> {
        self.release_dead_pin();
        let attachment = self.hub.attach()?;
        attachment.wait_ready(READY_TIMEOUT)?;
        Ok(attachment)
    }

    /// Drops the pin once its session ended or holds no media any more, so
    /// the hub's idle timer can close the session.
    fn release_dead_pin(&self) {
        let released = {
            let mut pin = self.pin();
            if pin.as_ref().is_some_and(|pin| !holds_media(pin)) {
                pin.take()
            } else {
                None
            }
        };
        // Dropping may close the session, which joins threads: no lock held.
        drop(released);
    }

    /// Keeps `attachment` as the pin while any drive has an image, else
    /// releases the pin.
    fn settle_pin(&self, attachment: Attachment) {
        let old = {
            let mut pin = self.pin();
            if holds_media(&attachment) {
                pin.replace(attachment)
            } else {
                pin.take()
            }
        };
        drop(old);
    }

    /// The latest frame, or `None` while the host sends no picture.
    pub fn capture(&self) -> Result<Option<Capture>, String> {
        let attachment = self.attach()?;
        let Some(frame) = latest_frame(&attachment) else {
            return Ok(None);
        };
        let status = status(&attachment);
        Ok(Some(Capture {
            frame,
            state: status.state,
            message: status.message,
        }))
    }

    pub fn screenshot(&self) -> Result<Option<Screenshot>, String> {
        let Some(capture) = self.capture()? else {
            return Ok(None);
        };
        Ok(Some(Screenshot {
            png: encode_png(&capture.frame)?,
            width: capture.frame.width,
            height: capture.frame.height,
            state: capture.state,
            message: capture.message,
        }))
    }

    /// Types `text` through the host layout. Returns the characters the
    /// layout cannot type.
    pub fn type_text(&self, text: &str) -> Result<(usize, Vec<char>), String> {
        let (strokes, skipped) = keymap::text_to_strokes(self.layout, text);
        let count = strokes.len();
        if count == 0 {
            return Ok((0, skipped));
        }
        let _input = self.input();
        let attachment = self.attach()?;
        let sent = &attachment.shared.keyboard_packets_sent;
        let before = sent.load(Ordering::Relaxed);
        attachment.send(ViewerCommand::TypeStrokes(strokes));
        // Each stroke takes about 40 ms on the wire.
        let timeout = Duration::from_millis(count as u64 * 100 + 5000);
        if wait_counter(sent, before + count as u64, timeout).is_err() {
            // Stop the paste so a retry does not type the text twice.
            attachment
                .shared
                .cancel_typing
                .store(true, Ordering::SeqCst);
            return Err("typing did not finish in time and was cancelled".into());
        }
        Ok((count, skipped))
    }

    /// Presses each of `usages` in turn with `modifiers` held.
    pub fn send_keys(&self, modifiers: u8, usages: &[u8]) -> Result<(), String> {
        let _input = self.input();
        let attachment = self.attach()?;
        let sent = &attachment.shared.keyboard_packets_sent;
        let before = sent.load(Ordering::Relaxed);
        for &usage in usages {
            attachment.send(ViewerCommand::Keystroke { modifiers, usage });
        }
        wait_counter(sent, before + usages.len() as u64, Duration::from_secs(10))
            .map_err(|()| "the keys were not sent in time".to_string())
    }

    /// Moves, clicks or double-clicks; returns the frame size the
    /// coordinates were checked against. `expected` is the screenshot size
    /// the caller worked from.
    pub fn mouse(
        &self,
        action: Pointer,
        x: u32,
        y: u32,
        button: Button,
        expected: (Option<u32>, Option<u32>),
    ) -> Result<(u32, u32), String> {
        let _input = self.input();
        let attachment = self.attach()?;
        if !status(&attachment).absolute_mouse {
            return Err(
                "the SP uses relative mouse mode, which this server does not support".into(),
            );
        }
        let frame = latest_frame(&attachment).ok_or("the host shows no picture yet")?;
        let (width, height) = (frame.width, frame.height);
        check_frame(width, height, expected, x, y)?;
        let send = |buttons: u8| {
            attachment.send(ViewerCommand::MouseAbsolute {
                buttons,
                x,
                y,
                width,
                height,
            });
        };
        let clicks = match action {
            Pointer::Move => 0,
            Pointer::Click => 1,
            Pointer::Double => 2,
        };
        let sent = &attachment.shared.mouse_packets_sent;
        let before = sent.load(Ordering::Relaxed);
        send(0);
        for _ in 0..clicks {
            thread::sleep(CLICK_HOLD);
            send(button.bit());
            thread::sleep(CLICK_HOLD);
            send(0);
        }
        wait_counter(sent, before + 1 + 2 * clicks, Duration::from_secs(10))
            .map_err(|()| "the mouse events were not sent in time".to_string())?;
        Ok((width, height))
    }

    pub fn status(&self) -> Result<ViewerStatus, String> {
        let attachment = self.attach()?;
        let mut status = status(&attachment);
        if let Some(frame) = latest_frame(&attachment) {
            status.message = format!("{} ({}x{})", status.message, frame.width, frame.height);
        }
        Ok(status)
    }

    /// Redirects an image file to a virtual drive and waits for the SP to
    /// accept it. The session stays open until every drive is empty again.
    pub fn mount(&self, kind: MediaKind, path: &Path, writable: bool) -> Result<String, String> {
        let path = self.policy.resolve(path, writable)?;
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let attachment = self.attach()?;
        attachment.send(ViewerCommand::Mount {
            kind,
            path: path.clone(),
            writable,
        });
        // The status still shows the previous image until the session
        // thread handles the command: wait for the new name first.
        let outcome = wait_until(MEDIA_TIMEOUT, || {
            let status = status(&attachment);
            let media = status.media(kind);
            if media.image.as_deref() != Some(name.as_str()) {
                return None;
            }
            match (&media.error, media.active) {
                (Some(error), _) => Some(Err(error.clone())),
                (None, true) => Some(Ok(())),
                _ => None,
            }
        });
        match outcome {
            Some(Ok(())) => {
                self.settle_pin(attachment);
                Ok(format!(
                    "{} mounted on the {}",
                    path.display(),
                    kind.label()
                ))
            }
            Some(Err(message)) => {
                release_drive(&attachment, kind);
                self.settle_pin(attachment);
                Err(message)
            }
            None => {
                // The SP may still accept it: keep the session open so
                // `status` and `unmount_media` see the real state.
                self.settle_pin(attachment);
                Err(format!(
                    "the SP has not accepted the {} image yet; it stays requested (check status, or unmount_media)",
                    kind.label()
                ))
            }
        }
    }

    pub fn unmount(&self, kind: MediaKind) -> Result<String, String> {
        if self.pin().is_none() {
            return Ok("no media is mounted".into());
        }
        let attachment = self.attach()?;
        let was_empty = status(&attachment).media(kind).image.is_none();
        if !was_empty {
            release_drive(&attachment, kind);
        }
        let empty = status(&attachment).media(kind).image.is_none();
        self.settle_pin(attachment);
        match (was_empty, empty) {
            (true, _) => Ok(format!("the {} was already empty", kind.label())),
            (false, true) => Ok(format!("{} unmounted", kind.label())),
            (false, false) => Err(format!("the {} was not released in time", kind.label())),
        }
    }

    fn pin(&self) -> MutexGuard<'_, Option<Attachment>> {
        self.pin.lock().unwrap_or_else(|poison| poison.into_inner())
    }
}

/// The session behind `attachment` still runs and has an image on a drive.
fn holds_media(attachment: &Attachment) -> bool {
    let status = status(attachment);
    let running = !matches!(
        status.state,
        ConnectionState::Stopped | ConnectionState::Error | ConnectionState::Rejected { .. }
    );
    running && (status.cdrom.image.is_some() || status.floppy.image.is_some())
}

/// Sends an unmount and waits until the drive shows empty.
fn release_drive(attachment: &Attachment, kind: MediaKind) {
    attachment.send(ViewerCommand::Unmount(kind));
    let _ = wait_until(Duration::from_secs(5), || {
        status(attachment).media(kind).image.is_none().then_some(())
    });
}

/// Checks a point against the frame and the size the caller expected.
fn check_frame(
    width: u32,
    height: u32,
    expected: (Option<u32>, Option<u32>),
    x: u32,
    y: u32,
) -> Result<(), String> {
    let (expected_width, expected_height) = expected;
    if expected_width.is_some_and(|value| value != width)
        || expected_height.is_some_and(|value| value != height)
    {
        return Err(format!(
            "the screen is now {width}x{height}, not the size you expected; take a new screenshot"
        ));
    }
    if x >= width || y >= height {
        return Err(format!("({x}, {y}) is outside the {width}x{height} screen"));
    }
    Ok(())
}

fn status(attachment: &Attachment) -> ViewerStatus {
    attachment.shared.status.lock().map_or_else(
        |poison| poison.into_inner().clone(),
        |status| status.clone(),
    )
}

fn latest_frame(attachment: &Attachment) -> Option<Arc<DecodedFrame>> {
    attachment.shared.latest_frame.lock().ok()?.clone()
}

/// Polls `check` until it returns `Some` or `timeout` passes.
fn wait_until<T>(timeout: Duration, mut check: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = check() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_counter(counter: &AtomicU64, target: u64, timeout: Duration) -> Result<(), ()> {
    wait_until(timeout, || {
        (counter.load(Ordering::Relaxed) >= target).then_some(())
    })
    .ok_or(())
}

/// The frame's pixels as packed RGB, without the alpha channel.
pub fn rgb_bytes(frame: &DecodedFrame) -> Vec<u8> {
    frame
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|pixel| [pixel[0], pixel[1], pixel[2]])
        .collect()
}

/// Encodes a frame as an opaque RGB PNG.
pub fn encode_png(frame: &DecodedFrame) -> Result<Vec<u8>, String> {
    let rgb = rgb_bytes(frame);
    let mut png = Cursor::new(Vec::new());
    PngEncoder::new(&mut png)
        .write_image(&rgb, frame.width, frame.height, ExtendedColorType::Rgb8)
        .map_err(|error| format!("PNG encoding failed: {error}"))?;
    Ok(png.into_inner())
}

/// One-line description of a connection state.
pub fn state_label(state: ConnectionState) -> &'static str {
    match state {
        ConnectionState::Connecting => "connecting",
        ConnectionState::Connected => "connected",
        ConnectionState::Reconnecting => "reconnecting",
        ConnectionState::Error => "error",
        ConnectionState::Rejected { .. } => "rejected",
        ConnectionState::Stopped => "stopped",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_of_a_synthetic_frame() {
        let frame = DecodedFrame {
            width: 2,
            height: 2,
            sequence: 1,
            rgba: vec![
                255, 0, 0, 255, //
                0, 255, 0, 255, //
                0, 0, 255, 255, //
                10, 20, 30, 255,
            ],
        };
        let png = encode_png(&frame).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn frame_size_checks() {
        assert!(check_frame(100, 50, (None, None), 99, 49).is_ok());
        assert!(check_frame(100, 50, (Some(100), Some(50)), 0, 0).is_ok());
        assert!(check_frame(100, 50, (None, None), 100, 0).is_err());
        assert!(check_frame(100, 50, (Some(80), None), 0, 0).is_err());
        assert!(check_frame(100, 50, (None, Some(60)), 0, 0).is_err());
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ilom-mcp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn media_policy() {
        let root = scratch("policy");
        let outside = scratch("policy-outside");
        std::fs::write(root.join("a.iso"), b"x").unwrap();
        std::fs::write(outside.join("b.iso"), b"x").unwrap();
        let policy = MediaPolicy {
            dir: Some(root.clone()),
            allow_writable: false,
        };
        assert!(policy.resolve(Path::new("a.iso"), false).is_ok());
        assert!(policy.resolve(&root.join("a.iso"), false).is_ok());
        assert!(policy.resolve(Path::new("missing.iso"), false).is_err());
        assert!(policy.resolve(&outside.join("b.iso"), false).is_err());
        assert!(policy.resolve(&root, false).is_err());
        assert!(policy.resolve(Path::new("a.iso"), true).is_err());
        let writable = MediaPolicy {
            allow_writable: true,
            ..policy.clone()
        };
        assert!(writable.resolve(Path::new("a.iso"), true).is_ok());
        assert!(
            MediaPolicy::default()
                .resolve(Path::new("a.iso"), false)
                .is_err()
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.join("b.iso"), root.join("link.iso")).unwrap();
            assert!(policy.resolve(Path::new("link.iso"), false).is_err());
        }
        assert!(policy.resolve(Path::new("../x"), false).is_err());
    }
}
