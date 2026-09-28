//! One VNC client: handshake, then a reader thread for input and a writer
//! loop that answers framebuffer update requests.

use std::{
    io::{BufWriter, Read, Write},
    net::{Shutdown, TcpStream},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use ilom_kvm_core::{
    hid,
    keymap::Layout,
    session::{DecodedFrame, HostCursor, ViewerCommand},
};
use tracing::{debug, info};

use crate::{
    hub::{Attachment, Hub},
    keys::Keyboard,
    rfb::{self, ClientMessage, PixelFormat, Rect, Update, Zrle},
    vncauth,
};

pub struct Options {
    /// VNC Authentication password; `None` offers no authentication.
    pub password: Option<Vec<u8>>,
    /// Keyboard layout configured on the host.
    pub layout: Layout,
    /// Desktop name shown by clients.
    pub name: String,
}

/// Framebuffer size while the host sends no picture.
const PLACEHOLDER: (u16, u16) = (1024, 768);
/// How long a new client may wait for the ILOM console to come up.
const READY_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Version {
    V3,
    V7,
    V8,
}

pub fn serve(stream: TcpStream, hub: &Arc<Hub>, options: &Options) -> Result<()> {
    stream.set_nodelay(true)?;
    // No client may hold the handshake open for ever.
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = stream.try_clone()?;
    let mut writer = BufWriter::new(stream.try_clone()?);

    writer.write_all(b"RFB 003.008\n")?;
    writer.flush()?;
    let mut version = [0_u8; 12];
    reader.read_exact(&mut version)?;
    let version = parse_version(&version)?;
    debug!(?version, "client version");

    let attachment = match &options.password {
        None => {
            // Nothing to authenticate: connect first, so a failure can be
            // reported to every protocol version.
            let attachment = hub.attach().and_then(|attachment| {
                attachment.wait_ready(READY_TIMEOUT)?;
                Ok(attachment)
            });
            match attachment {
                Ok(attachment) => {
                    match version {
                        Version::V3 => writer.write_all(&1_u32.to_be_bytes())?,
                        _ => {
                            writer.write_all(&[1, rfb::SECURITY_NONE])?;
                            writer.flush()?;
                            choose(&mut reader, rfb::SECURITY_NONE)?;
                        }
                    }
                    if version == Version::V8 {
                        writer.write_all(&0_u32.to_be_bytes())?;
                    }
                    writer.flush()?;
                    attachment
                }
                Err(reason) => {
                    // No security types: the connection failed, with a reason.
                    match version {
                        Version::V3 => writer.write_all(&0_u32.to_be_bytes())?,
                        _ => writer.write_all(&[0])?,
                    }
                    write_string(&mut writer, &reason)?;
                    writer.flush()?;
                    bail!("refused client: {reason}");
                }
            }
        }
        Some(password) => {
            match version {
                Version::V3 => writer.write_all(&2_u32.to_be_bytes())?,
                _ => {
                    writer.write_all(&[1, rfb::SECURITY_VNC_AUTH])?;
                    writer.flush()?;
                    choose(&mut reader, rfb::SECURITY_VNC_AUTH)?;
                }
            }
            let challenge = vncauth::challenge()?;
            writer.write_all(&challenge)?;
            writer.flush()?;
            let mut response = [0_u8; 16];
            reader.read_exact(&mut response)?;
            if !vncauth::matches(&vncauth::response(password, &challenge), &response) {
                refuse(&mut writer, version, "authentication failed")?;
                bail!("client sent a wrong VNC password");
            }
            let attachment = hub.attach().and_then(|attachment| {
                attachment.wait_ready(READY_TIMEOUT)?;
                Ok(attachment)
            });
            match attachment {
                Ok(attachment) => {
                    writer.write_all(&0_u32.to_be_bytes())?;
                    writer.flush()?;
                    attachment
                }
                Err(reason) => {
                    refuse(&mut writer, version, &reason)?;
                    bail!("refused client: {reason}");
                }
            }
        }
    };

    let mut shared_flag = [0_u8; 1];
    reader.read_exact(&mut shared_flag)?;
    let (width, height) = frame_size(&attachment).unwrap_or(PLACEHOLDER);
    writer.write_all(&width.to_be_bytes())?;
    writer.write_all(&height.to_be_bytes())?;
    writer.write_all(&PixelFormat::RGBX.to_bytes())?;
    write_string(&mut writer, &options.name)?;
    writer.flush()?;
    stream.set_read_timeout(None)?;

    let attachment = Arc::new(attachment);
    let client = Arc::new(Mutex::new(ClientState::default()));
    let input = {
        let attachment = attachment.clone();
        let client = client.clone();
        let layout = options.layout;
        thread::Builder::new()
            .name("ilom-vnc-input".into())
            .spawn(move || {
                let result = read_loop(&mut reader, &attachment, &client, layout);
                lock(&client).closed = true;
                attachment.hub().notify();
                result
            })?
    };
    let result = write_loop(&mut writer, &attachment, &client, (width, height));
    let _ = stream.shutdown(Shutdown::Both);
    let input_result = input
        .join()
        .unwrap_or_else(|_| Err(anyhow::anyhow!("input panicked")));
    result.and(input_result)
}

fn parse_version(bytes: &[u8; 12]) -> Result<Version> {
    let text = std::str::from_utf8(bytes).context("client version is not text")?;
    let minor = text
        .strip_prefix("RFB 003.")
        .and_then(|rest| rest.strip_suffix('\n'))
        .and_then(|minor| minor.parse::<u32>().ok())
        .with_context(|| format!("unexpected client version {text:?}"))?;
    Ok(match minor {
        0..=6 => Version::V3,
        7 => Version::V7,
        _ => Version::V8,
    })
}

fn choose(reader: &mut impl Read, offered: u8) -> Result<()> {
    let mut chosen = [0_u8; 1];
    reader.read_exact(&mut chosen)?;
    if chosen[0] != offered {
        bail!(
            "client chose security type {} that was not offered",
            chosen[0]
        );
    }
    Ok(())
}

fn write_string(writer: &mut impl Write, text: &str) -> std::io::Result<()> {
    writer.write_all(&(text.len() as u32).to_be_bytes())?;
    writer.write_all(text.as_bytes())
}

/// Failed SecurityResult; only version 3.8 carries a reason.
fn refuse(writer: &mut impl Write, version: Version, reason: &str) -> std::io::Result<()> {
    writer.write_all(&1_u32.to_be_bytes())?;
    if version == Version::V8 {
        write_string(writer, reason)?;
    }
    writer.flush()
}

fn frame_size(attachment: &Attachment) -> Option<(u16, u16)> {
    let frame = attachment.shared.latest_frame.lock().ok()?.clone()?;
    Some((
        u16::try_from(frame.width).ok()?,
        u16::try_from(frame.height).ok()?,
    ))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Encodings {
    zrle: bool,
    cursor: bool,
    desktop_size: bool,
    extended_key: bool,
}

struct ClientState {
    format: PixelFormat,
    encodings: Encodings,
    /// Pending FramebufferUpdateRequest: `Some(incremental)`.
    request: Option<bool>,
    encodings_changed: bool,
    closed: bool,
}

impl Default for ClientState {
    fn default() -> Self {
        Self {
            format: PixelFormat::RGBX,
            encodings: Encodings::default(),
            request: None,
            encodings_changed: false,
            closed: false,
        }
    }
}

fn lock(client: &Mutex<ClientState>) -> std::sync::MutexGuard<'_, ClientState> {
    client.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn read_loop(
    reader: &mut TcpStream,
    attachment: &Attachment,
    client: &Mutex<ClientState>,
    layout: Layout,
) -> Result<()> {
    let mut keyboard = Keyboard::new(layout);
    let mut pointer = Pointer::default();
    let result = (|| -> Result<()> {
        loop {
            let message = match rfb::read_client_message(reader) {
                Ok(message) => message,
                Err(error) if is_disconnect(&error) => return Ok(()),
                Err(error) => return Err(error),
            };
            match message {
                ClientMessage::SetPixelFormat(format) => {
                    format.check()?;
                    lock(client).format = format;
                }
                ClientMessage::SetEncodings(list) => {
                    let has = |encoding| list.contains(&encoding);
                    let mut state = lock(client);
                    state.encodings = Encodings {
                        zrle: has(rfb::ENCODING_ZRLE),
                        cursor: has(rfb::ENCODING_CURSOR),
                        desktop_size: has(rfb::ENCODING_DESKTOP_SIZE),
                        extended_key: has(rfb::ENCODING_QEMU_EXTENDED_KEY),
                    };
                    state.encodings_changed = true;
                    debug!(encodings = ?state.encodings, "client encodings");
                }
                ClientMessage::UpdateRequest { incremental, .. } => {
                    let mut state = lock(client);
                    // A full request wins over an incremental one.
                    state.request = Some(state.request.unwrap_or(true) && incremental);
                    drop(state);
                    attachment.hub().notify();
                }
                ClientMessage::Key { down, keysym } => {
                    if let Some(command) = keyboard.keysym(keysym, down) {
                        attachment.send(command);
                    }
                }
                ClientMessage::ExtendedKey { down, keysym, code } => {
                    if let Some(command) = keyboard.extended(keysym, code, down) {
                        attachment.send(command);
                    }
                }
                ClientMessage::Pointer { buttons, x, y } => {
                    pointer.event(attachment, buttons, x, y);
                }
                ClientMessage::CutText(text) => {
                    debug!(bytes = text.len(), "ignoring client clipboard");
                }
            }
        }
    })();
    if let Some(command) = keyboard.release_all() {
        attachment.send(command);
    }
    pointer.release(attachment);
    result
}

fn is_disconnect(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
            )
        })
    })
}

/// Pointer state of one client.
#[derive(Default)]
struct Pointer {
    buttons: u8,
    last: Option<(u16, u16)>,
}

impl Pointer {
    fn event(&mut self, attachment: &Attachment, mask: u8, x: u16, y: u16) {
        // RFB: bit 0 left, 1 middle, 2 right; 3–6 are wheel steps, which the
        // SP's HID reports cannot carry.
        let mut buttons = 0;
        for (bit, button) in [
            (1, hid::BUTTON_LEFT),
            (2, hid::BUTTON_MIDDLE),
            (4, hid::BUTTON_RIGHT),
        ] {
            if mask & bit != 0 {
                buttons |= button;
            }
        }
        let absolute = attachment
            .shared
            .status
            .lock()
            .is_ok_and(|status| status.absolute_mouse);
        if absolute {
            let Some((width, height)) = frame_size(attachment) else {
                return;
            };
            attachment.send(ViewerCommand::MouseAbsolute {
                buttons,
                x: u32::from(x.min(width.saturating_sub(1))),
                y: u32::from(y.min(height.saturating_sub(1))),
                width: u32::from(width),
                height: u32::from(height),
            });
        } else {
            let (last_x, last_y) = self.last.unwrap_or((x, y));
            let (mut dx, mut dy) = (
                i32::from(x) - i32::from(last_x),
                i32::from(y) - i32::from(last_y),
            );
            // Relative reports carry at most ±127 per axis.
            loop {
                let step_x = dx.clamp(-127, 127);
                let step_y = dy.clamp(-127, 127);
                attachment.send(ViewerCommand::MouseRelative {
                    buttons,
                    dx: step_x as i8,
                    dy: step_y as i8,
                });
                dx -= step_x;
                dy -= step_y;
                if dx == 0 && dy == 0 {
                    break;
                }
            }
        }
        self.buttons = buttons;
        self.last = Some((x, y));
    }

    fn release(&mut self, attachment: &Attachment) {
        if self.buttons != 0
            && let Some((x, y)) = self.last
        {
            self.event(attachment, 0, x, y);
        }
    }
}

enum Job {
    /// The client disconnected.
    Closed,
    /// The ILOM session ended for good.
    SessionEnded,
    Update {
        incremental: bool,
        format: PixelFormat,
        encodings: Encodings,
        encodings_changed: bool,
    },
}

/// Content version: frame sequence and cursor serial.
type ContentKey = (u64, u64);

fn content_key(attachment: &Attachment) -> ContentKey {
    let frame = attachment
        .shared
        .latest_frame
        .lock()
        .ok()
        .and_then(|frame| frame.as_ref().map(|frame| frame.sequence))
        .unwrap_or(0);
    let cursor = attachment
        .shared
        .cursor
        .lock()
        .ok()
        .and_then(|cursor| cursor.as_ref().map(|cursor| cursor.serial))
        .unwrap_or(0);
    (frame, cursor)
}

struct Framebuffer {
    width: u16,
    height: u16,
    /// What the client shows; empty until the first full update.
    sent: Vec<u8>,
    sent_key: Option<ContentKey>,
}

fn write_loop(
    writer: &mut impl Write,
    attachment: &Attachment,
    client: &Mutex<ClientState>,
    (width, height): (u16, u16),
) -> Result<()> {
    let mut framebuffer = Framebuffer {
        width,
        height,
        sent: Vec::new(),
        sent_key: None,
    };
    let mut zrle = Zrle::new();
    let mut extended_key_acked = false;
    let mut cursor_sent = false;
    loop {
        let job = attachment
            .hub()
            .wait_for(None, || {
                let mut state = lock(client);
                if state.closed {
                    return Some(Job::Closed);
                }
                if attachment.is_final() {
                    return Some(Job::SessionEnded);
                }
                let incremental = state.request?;
                // Clients without DesktopSize keep their size: frames are
                // clipped or padded instead.
                let resized = state.encodings.desktop_size
                    && frame_size(attachment)
                        .is_some_and(|size| size != (framebuffer.width, framebuffer.height));
                let changed = framebuffer.sent_key != Some(content_key(attachment));
                if !incremental || changed || resized || state.encodings_changed {
                    state.request = None;
                    let encodings_changed = std::mem::take(&mut state.encodings_changed);
                    Some(Job::Update {
                        incremental,
                        format: state.format,
                        encodings: state.encodings,
                        encodings_changed,
                    })
                } else {
                    None
                }
            })
            .expect("waits without a timeout");
        let (incremental, format, encodings) = match job {
            Job::Closed => return Ok(()),
            Job::SessionEnded => {
                let message = attachment.failure().unwrap_or_default();
                info!(%message, "ILOM session ended, closing the VNC client");
                return Ok(());
            }
            Job::Update {
                incremental,
                format,
                encodings,
                encodings_changed,
            } => {
                if encodings_changed && !encodings.cursor {
                    cursor_sent = false;
                }
                (incremental, format, encodings)
            }
        };

        let mut update = Update::new();
        if encodings.extended_key && !extended_key_acked {
            // An empty pseudo-rectangle tells the client to send QEMU
            // extended key events.
            update.header(
                Rect {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                },
                rfb::ENCODING_QEMU_EXTENDED_KEY,
            );
            extended_key_acked = true;
        }
        if encodings.cursor && !cursor_sent {
            dot_cursor(&mut update, &format);
            cursor_sent = true;
        }

        let key = content_key(attachment);
        let frame = attachment
            .shared
            .latest_frame
            .lock()
            .ok()
            .and_then(|f| f.clone());
        let cursor = attachment.shared.cursor.lock().ok().and_then(|c| c.clone());
        if let Some(frame) = &frame
            && let (Ok(width), Ok(height)) =
                (u16::try_from(frame.width), u16::try_from(frame.height))
            && (width, height) != (framebuffer.width, framebuffer.height)
            && encodings.desktop_size
        {
            info!(width, height, "host resolution changed");
            update.header(
                Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
                rfb::ENCODING_DESKTOP_SIZE,
            );
            framebuffer.width = width;
            framebuffer.height = height;
            framebuffer.sent.clear();
            framebuffer.sent_key = None;
            update.send(writer)?;
            continue;
        }

        let composed = compose(
            frame.as_deref(),
            cursor.as_deref(),
            framebuffer.width,
            framebuffer.height,
        );
        let rects = if incremental && !framebuffer.sent.is_empty() {
            dirty_rects(
                &framebuffer.sent,
                &composed,
                framebuffer.width,
                framebuffer.height,
            )
        } else {
            vec![Rect {
                x: 0,
                y: 0,
                width: framebuffer.width,
                height: framebuffer.height,
            }]
        };
        let stride = usize::from(framebuffer.width);
        for rect in rects {
            let pixels = rfb::crop(&composed, stride, rect);
            if encodings.zrle {
                let out = update.header(rect, rfb::ENCODING_ZRLE);
                zrle.encode(
                    &format,
                    &pixels,
                    usize::from(rect.width),
                    usize::from(rect.height),
                    out,
                )?;
            } else {
                format.encode(&pixels, update.header(rect, rfb::ENCODING_RAW));
            }
        }
        framebuffer.sent = composed;
        framebuffer.sent_key = Some(key);
        if update.is_empty() {
            // Nothing visible changed: keep the request for the next change.
            let mut state = lock(client);
            state.request.get_or_insert(true);
            continue;
        }
        update.send(writer)?;
    }
}

/// Frame (clipped or padded with black to the client framebuffer) with the
/// host cursor blended in.
fn compose(
    frame: Option<&DecodedFrame>,
    cursor: Option<&HostCursor>,
    width: u16,
    height: u16,
) -> Vec<u8> {
    let (width, height) = (usize::from(width), usize::from(height));
    let Some(frame) = frame else {
        return vec![0; width * height * 4];
    };
    let (frame_width, frame_height) = (frame.width as usize, frame.height as usize);
    let mut out = if (frame_width, frame_height) == (width, height) {
        frame.rgba.clone()
    } else {
        let mut out = vec![0; width * height * 4];
        let columns = width.min(frame_width) * 4;
        for row in 0..height.min(frame_height) {
            out[row * width * 4..][..columns]
                .copy_from_slice(&frame.rgba[row * frame_width * 4..][..columns]);
        }
        out
    };
    if let Some(image) = cursor.and_then(|cursor| cursor.render(frame)) {
        for row in 0..image.height as usize {
            let y = image.y as usize + row;
            if y >= height {
                break;
            }
            for column in 0..image.width as usize {
                let x = image.x as usize + column;
                if x >= width {
                    break;
                }
                let source = &image.rgba[(row * image.width as usize + column) * 4..][..4];
                let alpha = u16::from(source[3]);
                if alpha == 0 {
                    continue;
                }
                let target = &mut out[(y * width + x) * 4..][..3];
                for (below, &above) in target.iter_mut().zip(&source[..3]) {
                    *below = ((u16::from(above) * alpha + u16::from(*below) * (255 - alpha) + 127)
                        / 255) as u8;
                }
            }
        }
    }
    out
}

/// Side of the squares compared to find changed areas.
const TILE: usize = 64;

/// Changed areas between two RGBA framebuffers, as runs of changed tiles on
/// each tile row.
fn dirty_rects(before: &[u8], after: &[u8], width: u16, height: u16) -> Vec<Rect> {
    let (width, height) = (usize::from(width), usize::from(height));
    let mut rects = Vec::new();
    for tile_y in (0..height).step_by(TILE) {
        let tile_height = TILE.min(height - tile_y);
        let mut push = |start: usize, end: usize| {
            rects.push(Rect {
                x: start as u16,
                y: tile_y as u16,
                width: (end - start) as u16,
                height: tile_height as u16,
            })
        };
        let mut run: Option<usize> = None;
        for tile_x in (0..width).step_by(TILE) {
            let tile_width = TILE.min(width - tile_x);
            let changed = (tile_y..tile_y + tile_height).any(|row| {
                let start = (row * width + tile_x) * 4;
                let end = start + tile_width * 4;
                before[start..end] != after[start..end]
            });
            match (changed, run) {
                (true, None) => run = Some(tile_x),
                (false, Some(start)) => {
                    push(start, tile_x);
                    run = None;
                }
                _ => {}
            }
        }
        if let Some(start) = run {
            push(start, width);
        }
    }
    rects
}

/// Local cursor for clients that draw one: a small dot, since the host
/// cursor is already drawn into the framebuffer.
fn dot_cursor(update: &mut Update, format: &PixelFormat) {
    const SIZE: usize = 5;
    const DOT: [&[u8; SIZE]; SIZE] = [b" ### ", b"#...#", b"#...#", b"#...#", b" ### "];
    let mut rgba = Vec::with_capacity(SIZE * SIZE * 4);
    let mut mask = Vec::with_capacity(SIZE);
    for row in DOT {
        let mut bits = 0_u8;
        for (column, &cell) in row.iter().enumerate() {
            let value = if cell == b'.' { 255 } else { 0 };
            rgba.extend_from_slice(&[value, value, value, 0]);
            if cell != b' ' {
                bits |= 0x80 >> column;
            }
        }
        mask.push(bits);
    }
    let out = update.header(
        Rect {
            x: 2,
            y: 2,
            width: SIZE as u16,
            height: SIZE as u16,
        },
        rfb::ENCODING_CURSOR,
    );
    format.encode(&rgba, out);
    out.extend_from_slice(&mask);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version(b"RFB 003.003\n").unwrap(), Version::V3);
        assert_eq!(parse_version(b"RFB 003.007\n").unwrap(), Version::V7);
        assert_eq!(parse_version(b"RFB 003.008\n").unwrap(), Version::V8);
        // Apple Remote Desktop announces 3.889.
        assert_eq!(parse_version(b"RFB 003.889\n").unwrap(), Version::V8);
        assert!(parse_version(b"HTTP/1.1 200").is_err());
    }

    #[test]
    fn dirty_rects_merge_changed_tiles_on_a_row() {
        let (width, height) = (200_u16, 70_u16);
        let before = vec![0_u8; 200 * 70 * 4];
        let mut after = before.clone();
        // Change pixels in tiles 0 and 1 of row 0 and in the last tile of row 1.
        for (x, y) in [(10, 5), (70, 60), (199, 69)] {
            after[(y * 200 + x) * 4] = 1;
        }
        let rects = dirty_rects(&before, &after, width, height);
        assert_eq!(
            rects,
            vec![
                Rect {
                    x: 0,
                    y: 0,
                    width: 128,
                    height: 64
                },
                Rect {
                    x: 192,
                    y: 64,
                    width: 8,
                    height: 6
                },
            ]
        );
        assert!(dirty_rects(&before, &before, width, height).is_empty());
    }

    #[test]
    fn frames_are_padded_to_the_client_framebuffer() {
        let frame = DecodedFrame {
            width: 2,
            height: 1,
            sequence: 1,
            rgba: vec![9; 8],
        };
        let out = compose(Some(&frame), None, 3, 2);
        assert_eq!(out.len(), 3 * 2 * 4);
        assert_eq!(out[..8], [9; 8]);
        assert!(out[8..].iter().all(|&byte| byte == 0));
    }
}
