use std::{
    io::{Read, Write},
    net::TcpStream,
};

use anyhow::{Context, Result, bail};
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::{
    crypto,
    ivtp::{self, Packet, RedirectPacket, fixed_field, video as kind},
    rc4::Rc4,
    tls::{self, CertPolicy},
    tokend::Tokend,
};

pub const PORT: u16 = 7578;
pub const SERIAL_PORT: u16 = 7579;

pub const RC4_KEY_PORT: u16 = 5555;
const RC4_KEY_LEN: usize = 16;

pub const SERVER_AST2000: u8 = 3;
pub const SERVER_AST2100: u8 = 4;

const DEVICE_CAPABILITIES: u16 = 6;
const USERNAME_FIELD: usize = 16;
const CHALLENGE_LEN: usize = 32;
const LOGIN_DENIED: u16 = 2058;

/// `ASP-2000` video header that precedes the AST image header in each frame.
const VIDEO_HEADER_LEN: usize = 39;
const SIGNATURE_OFFSET: usize = 19;
const SIGNATURE: &[u8; 8] = b"ASP-2000";
const AST_HEADER_LEN: usize = 86;
const LAST_FRAGMENT: u16 = 0x8000;

#[derive(Debug, Clone)]
pub struct AstFrameHeader {
    pub source_width: u16,
    pub source_height: u16,
    pub destination_width: u16,
    pub destination_height: u16,
    pub frame_number: u32,
    pub compression_mode: u8,
    pub jpeg_table_selector: u8,
    pub jpeg_yuv_table_mapping: u8,
    pub advance_table_selector: u8,
    pub rc4_enabled: bool,
    pub rc4_reset: bool,
    pub mode_420: bool,
    pub compressed_size: u32,
}

#[derive(Debug, Clone)]
pub struct CompressedFrame {
    pub header: AstFrameHeader,
    /// Compressed stream. While `header.rc4_enabled` is set this holds the
    /// whole encrypted remainder of the frame, because the keystream advances
    /// over every byte the SP sends, not just `compressed_size`.
    pub data: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum VideoError {
    #[error("video frame is shorter than its {0}-byte headers")]
    ShortFrame(usize),
    #[error("video frame has invalid signature {0:02x?}")]
    BadSignature([u8; 8]),
    #[error("invalid AST frame dimensions {0}x{1}")]
    InvalidDimensions(u16, u16),
    #[error("compressed size {size} exceeds the {available} bytes received")]
    Truncated { size: u32, available: usize },
}

/// Joins type-5 fragments. Each one starts with a u16 fragment number; the
/// counter restarts at 0 for a new frame and bit 15 marks the last fragment.
#[derive(Default)]
pub struct FrameAssembler {
    buffer: Vec<u8>,
}

impl FrameAssembler {
    pub fn push(&mut self, payload: &[u8]) -> Result<Option<CompressedFrame>, VideoError> {
        if payload.len() < 2 {
            return Err(VideoError::ShortFrame(2));
        }
        let fragment = u16::from_le_bytes([payload[0], payload[1]]);
        if fragment & !LAST_FRAGMENT == 0 {
            self.buffer.clear();
        }
        self.buffer.extend_from_slice(&payload[2..]);
        if fragment & LAST_FRAGMENT == 0 {
            return Ok(None);
        }
        let frame = std::mem::take(&mut self.buffer);
        parse_frame(&frame).map(Some)
    }
}

pub fn parse_frame(frame: &[u8]) -> Result<CompressedFrame, VideoError> {
    let headers = VIDEO_HEADER_LEN + AST_HEADER_LEN;
    if frame.len() < headers {
        return Err(VideoError::ShortFrame(headers));
    }
    let signature: [u8; 8] = frame[SIGNATURE_OFFSET..SIGNATURE_OFFSET + 8]
        .try_into()
        .unwrap();
    if &signature != SIGNATURE {
        return Err(VideoError::BadSignature(signature));
    }
    let header = parse_ast_header(&frame[VIDEO_HEADER_LEN..headers])?;
    let data = &frame[headers..];
    let size = header.compressed_size as usize;
    if size > data.len() {
        return Err(VideoError::Truncated {
            size: header.compressed_size,
            available: data.len(),
        });
    }
    let data = if header.rc4_enabled {
        data
    } else {
        &data[..size]
    };
    Ok(CompressedFrame {
        data: data.to_vec(),
        header,
    })
}

/// Decrypts RC4-protected frames in place, keeping the cipher state across
/// frames until the SP requests a reset.
pub struct FrameDecryptor {
    key: Option<Vec<u8>>,
    cipher: Option<Rc4>,
}

impl FrameDecryptor {
    pub fn new(key: Option<Vec<u8>>) -> Self {
        Self { key, cipher: None }
    }

    pub fn decrypt(&mut self, frame: &mut CompressedFrame) -> Result<()> {
        if !frame.header.rc4_enabled {
            return Ok(());
        }
        if frame.header.rc4_reset || self.cipher.is_none() {
            let key = self
                .key
                .as_deref()
                .context("SP sent an RC4-encrypted frame but no RC4 key was negotiated")?;
            self.cipher = Some(Rc4::new(key));
        }
        self.cipher.as_mut().unwrap().apply(&mut frame.data);
        frame.data.truncate(frame.header.compressed_size as usize);
        frame.header.rc4_enabled = false;
        Ok(())
    }
}

/// Fetches the dynamic RC4 video key from the SP (TLS, port 5555) using a
/// fresh redirection token.
fn fetch_rc4_key(host: &str, policy: CertPolicy, token: &[u8]) -> Result<Vec<u8>> {
    let mut stream =
        tls::connect(host, RC4_KEY_PORT, policy).context("connect to RC4 key service")?;
    stream.write_all(token)?;
    let mut status = [0_u8; 1];
    stream.read_exact(&mut status)?;
    if status[0] != 0 {
        bail!("RC4 key service rejected the token (status {})", status[0]);
    }
    stream.write_all(&[1])?;
    let mut key = [0_u8; RC4_KEY_LEN];
    stream.read_exact(&mut key).context("read RC4 key")?;
    stream.write_all(&[1])?;
    let _ = stream.shutdown();
    // The vendor client round-trips the key through a Java String (UTF-8),
    // which replaces invalid sequences before the bytes reach RC4.
    let key = String::from_utf8_lossy(&key).into_owned().into_bytes();
    if key.len() != RC4_KEY_LEN {
        warn!("RC4 key is not plain ASCII; applying the vendor's UTF-8 conversion");
    }
    Ok(key)
}

fn le_u16(input: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([input[offset], input[offset + 1]])
}

fn le_u32(input: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(input[offset..offset + 4].try_into().unwrap())
}

fn parse_ast_header(raw: &[u8]) -> Result<AstFrameHeader, VideoError> {
    let source_width = le_u16(raw, 4);
    let source_height = le_u16(raw, 6);
    if source_width == 0 || source_height == 0 || source_width > 4096 || source_height > 4096 {
        return Err(VideoError::InvalidDimensions(source_width, source_height));
    }
    Ok(AstFrameHeader {
        source_width,
        source_height,
        destination_width: le_u16(raw, 13),
        destination_height: le_u16(raw, 15),
        frame_number: le_u32(raw, 26),
        compression_mode: raw[42],
        jpeg_table_selector: raw[44],
        jpeg_yuv_table_mapping: raw[45],
        advance_table_selector: raw[47],
        rc4_enabled: raw[53] != 0,
        rc4_reset: raw[54] != 0,
        mode_420: raw[55] != 0,
        compressed_size: le_u32(raw, 69),
    })
}

/// Events delivered by the video channel after the handshake.
#[derive(Debug)]
pub enum VideoEvent {
    Frame(CompressedFrame),
    BlankScreen,
    Cursor(CursorUpdate),
    MouseMode(u8),
    ActiveClients(u8),
    Other(Packet),
}

/// Side of the square hardware cursor pattern, in pixels.
pub const CURSOR_SIZE: usize = 64;
const CURSOR_HEADER_LEN: usize = 57;

/// Hardware cursor update (type 48, `docs/protocol.md` §5.7).
#[derive(Clone)]
pub struct CursorUpdate {
    /// Pixels carry 4-bit alpha; otherwise AND/XOR bits.
    pub alpha: bool,
    /// Only `x`/`y` are valid; keep the previous type, offsets and pattern.
    pub position_only: bool,
    /// Host framebuffer position of the pattern's visible part.
    pub x: i16,
    pub y: i16,
    /// First pattern column/row shown (cursor clipped at the left/top edge).
    pub x_offset: i16,
    pub y_offset: i16,
    /// `CURSOR_SIZE`² pixels, row-major, when the shape changed.
    pub pattern: Option<Vec<u16>>,
}

impl CursorUpdate {
    fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < CURSOR_HEADER_LEN {
            return None;
        }
        let signed = |offset| le_u16(payload, offset) as i16;
        let pattern_bytes = CURSOR_SIZE * CURSOR_SIZE * 2;
        let pattern = payload[CURSOR_HEADER_LEN..]
            .get(..pattern_bytes)
            .map(|bytes| {
                bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&pixel| u16::from_le_bytes(pixel))
                    .collect()
            });
        Some(Self {
            alpha: le_u32(payload, 41) == 1,
            position_only: le_u32(payload, 45) == 0,
            x: signed(49),
            y: signed(51),
            x_offset: signed(53),
            y_offset: signed(55),
            pattern,
        })
    }
}

impl std::fmt::Debug for CursorUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CursorUpdate")
            .field("alpha", &self.alpha)
            .field("position_only", &self.position_only)
            .field("x", &self.x)
            .field("y", &self.y)
            .field("x_offset", &self.x_offset)
            .field("y_offset", &self.y_offset)
            .field("pattern", &self.pattern.is_some())
            .finish()
    }
}

pub struct VideoSession {
    stream: TcpStream,
    assembler: FrameAssembler,
    decryptor: FrameDecryptor,
    pub server_id: u8,
    pub server_version: u8,
}

impl VideoSession {
    /// Runs the AST2100 video handshake: device capabilities, server info,
    /// token authentication and the challenge login that follows it.
    pub fn connect(
        host: &str,
        policy: CertPolicy,
        username: &str,
        tokend: &mut Tokend,
    ) -> Result<Self> {
        let mut stream = tls::connect_tcp(host, PORT).context("connect to video server")?;

        // ports u16 = 0, reserved = [0, 2]; the SP answers in the same layout.
        RedirectPacket::new(DEVICE_CAPABILITIES, vec![0, 0, 0, 2]).write_to(&mut stream)?;
        let caps = RedirectPacket::read_from(&mut stream).context("read device capabilities")?;
        if caps.status != 0 || caps.payload.len() < 4 {
            bail!("device capabilities failed with status {}", caps.status);
        }
        let reserved = [caps.payload[2], caps.payload[3]];
        debug!(?reserved, "video device capabilities");
        if reserved[0] != 1 {
            bail!(
                "video server is not AST2000-compatible (reserved {reserved:?}); only AST2100 ILOMs are supported"
            );
        }
        if reserved[1] & 0x0f == 1 {
            bail!("the maximum number of video sessions has been reached");
        }

        let info = expect(&mut stream, kind::SERVER_INFO, "server info")?;
        if info.payload.len() < 2 {
            bail!("short server info packet");
        }
        let (server_id, server_version) = (info.payload[0], info.payload[1]);
        info!(server_id, server_version, "video server");
        if server_id != SERVER_AST2100 {
            bail!("unsupported video server id {server_id} (only AST2100 = 4 is implemented)");
        }

        let token = tokend.redirection_token()?;
        Packet::new(kind::TOKEN_AUTH, token.to_vec()).write_to(&mut stream)?;
        let reply = expect(&mut stream, kind::TOKEN_AUTH, "token authentication")?;
        debug!(status = reply.status, payload = %hex::encode(&reply.payload), "token reply");
        if reply.status != 0 {
            bail!(
                "video token authentication failed with status {}",
                reply.status
            );
        }

        // After token authentication the vendor client still logs in, using the
        // literal password "token" against the salt and challenge from the SP.
        let salt_len = if server_version == 1 { 8 } else { 12 };
        let mut request = fixed_field(username.as_bytes(), USERNAME_FIELD);
        request.resize(USERNAME_FIELD + salt_len + CHALLENGE_LEN, 0);
        Packet::new(kind::GET_CHALLENGE, request).write_to(&mut stream)?;
        let challenge = read_packet(&mut stream, salt_len).context("read login challenge")?;
        debug!(status = challenge.status, payload = %hex::encode(&challenge.payload), "login challenge");
        if challenge.status == LOGIN_DENIED || challenge.status == 2 {
            bail!("video login denied for {username}");
        }
        if challenge.status != 0
            || challenge.payload.len() < USERNAME_FIELD + salt_len + CHALLENGE_LEN
        {
            bail!("bad login challenge (status {})", challenge.status);
        }
        let salt =
            crypto::normalize_salt(&challenge.payload[USERNAME_FIELD..USERNAME_FIELD + salt_len]);
        let nonce = &challenge.payload[USERNAME_FIELD + salt_len..][..CHALLENGE_LEN];
        let hash = crypto::unix_hash("token", &salt, crypto::MD5_CRYPT_LEN)?;
        let mut login = fixed_field(username.as_bytes(), USERNAME_FIELD);
        login.extend_from_slice(&crypto::challenge_digest(&hash, nonce));
        Packet::new(kind::LOGIN, login).write_to(&mut stream)?;
        let reply = read_packet(&mut stream, salt_len).context("read login result")?;
        if reply.status != 0 {
            bail!("video login rejected with status {}", reply.status);
        }

        // AST2100 video is RC4-protected by default; the key comes from a
        // separate TLS service.
        let token = tokend.redirection_token()?;
        let rc4_key = match fetch_rc4_key(host, policy, &token) {
            Ok(key) => Some(key),
            Err(error) => {
                warn!(%error, "could not fetch RC4 video key");
                None
            }
        };

        Packet::new(kind::GET_USB_MOUSE_MODE, vec![0]).write_to(&mut stream)?;
        info!("video redirection started");
        // Frames may take a while when the host screen is idle.
        stream.set_read_timeout(None)?;
        Ok(Self {
            stream,
            assembler: FrameAssembler::default(),
            decryptor: FrameDecryptor::new(rc4_key),
            server_id,
            server_version,
        })
    }

    fn salt_len(&self) -> usize {
        if self.server_version == 1 { 8 } else { 12 }
    }

    pub fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> Result<()> {
        Ok(self.stream.set_read_timeout(timeout)?)
    }

    pub fn try_clone_stream(&self) -> Result<TcpStream> {
        Ok(self.stream.try_clone()?)
    }

    pub fn next_event(&mut self) -> Result<VideoEvent> {
        loop {
            let salt_len = self.salt_len();
            let packet = read_packet(&mut self.stream, salt_len)?;
            match packet.kind {
                kind::VIDEO_FRAGMENT => match self.assembler.push(&packet.payload) {
                    Ok(Some(mut frame)) => {
                        self.decryptor.decrypt(&mut frame)?;
                        return Ok(VideoEvent::Frame(frame));
                    }
                    Ok(None) => continue,
                    Err(error) => warn!(%error, "dropping malformed video frame"),
                },
                kind::BLANK_SCREEN => return Ok(VideoEvent::BlankScreen),
                kind::GET_USB_MOUSE_MODE => {
                    return Ok(VideoEvent::MouseMode(
                        packet.payload.first().copied().unwrap_or(0),
                    ));
                }
                kind::HARDWARE_CURSOR => match CursorUpdate::parse(&packet.payload) {
                    Some(cursor) => return Ok(VideoEvent::Cursor(cursor)),
                    None => warn!(len = packet.payload.len(), "dropping short cursor packet"),
                },
                kind::ACTIVE_CLIENTS => {
                    return Ok(VideoEvent::ActiveClients(
                        packet.payload.first().copied().unwrap_or(0),
                    ));
                }
                _ => return Ok(VideoEvent::Other(packet)),
            }
        }
    }

    pub fn send(&mut self, packet: &Packet) -> Result<()> {
        packet.write_to(&mut self.stream)
    }

    pub fn stop(mut self) {
        let _ =
            Packet::new(ivtp::video::STOP_SESSION_IMMEDIATE, Vec::new()).write_to(&mut self.stream);
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

/// Payload sizes of fixed-layout server packets. The SP sometimes announces
/// a length that includes the header (e.g. 27 for the 20-byte token reply).
fn video_payload_len(kind: u8, announced: usize, salt_len: usize) -> usize {
    match kind {
        kind::TOKEN_AUTH => 20,
        kind::LOGIN | kind::BLANK_SCREEN => 0,
        kind::GET_CHALLENGE => USERNAME_FIELD + salt_len + CHALLENGE_LEN,
        kind::SERVER_INFO => 2,
        kind::GET_USB_MOUSE_MODE | kind::ACTIVE_CLIENTS => 1,
        kind::GET_VIDEO_ENGINE_CONFIGS => 8,
        _ => announced,
    }
}

fn read_packet(stream: &mut TcpStream, salt_len: usize) -> Result<Packet> {
    Packet::read_with(stream, |kind, announced| {
        video_payload_len(kind, announced, salt_len)
    })
}

fn expect(stream: &mut TcpStream, expected: u8, what: &str) -> Result<Packet> {
    let packet = read_packet(stream, 12).with_context(|| format!("read {what}"))?;
    if packet.kind != expected {
        bail!(
            "expected {what} (type {expected}), got type {} status {}",
            packet.kind,
            packet.status
        );
    }
    Ok(packet)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u16, height: u16, data: &[u8]) -> Vec<u8> {
        let mut frame = vec![0_u8; VIDEO_HEADER_LEN + AST_HEADER_LEN];
        frame[SIGNATURE_OFFSET..SIGNATURE_OFFSET + 8].copy_from_slice(SIGNATURE);
        let ast = &mut frame[VIDEO_HEADER_LEN..];
        ast[4..6].copy_from_slice(&width.to_le_bytes());
        ast[6..8].copy_from_slice(&height.to_le_bytes());
        ast[69..73].copy_from_slice(&(data.len() as u32).to_le_bytes());
        frame.extend_from_slice(data);
        frame
    }

    #[test]
    fn parses_cursor_packets() {
        let mut payload = vec![0_u8; CURSOR_HEADER_LEN];
        payload[41] = 1;
        payload[45] = 7;
        payload[49..51].copy_from_slice(&100_u16.to_le_bytes());
        payload[51..53].copy_from_slice(&50_u16.to_le_bytes());
        payload[53] = 2;
        let moved = CursorUpdate::parse(&payload).unwrap();
        assert!(moved.alpha && !moved.position_only && moved.pattern.is_none());
        assert_eq!(
            (moved.x, moved.y, moved.x_offset, moved.y_offset),
            (100, 50, 2, 0)
        );
        payload.extend(std::iter::repeat_n(0_u8, CURSOR_SIZE * CURSOR_SIZE * 2));
        payload[CURSOR_HEADER_LEN] = 0x34;
        payload[CURSOR_HEADER_LEN + 1] = 0x12;
        let shaped = CursorUpdate::parse(&payload).unwrap();
        assert_eq!(shaped.pattern.unwrap()[0], 0x1234);
        assert!(CursorUpdate::parse(&payload[..56]).is_none());
    }

    #[test]
    fn reassembles_fragmented_frame() {
        let raw = frame(800, 600, &[1, 2, 3, 4]);
        let (first, second) = raw.split_at(100);
        let mut assembler = FrameAssembler::default();
        let mut packet = 0_u16.to_le_bytes().to_vec();
        packet.extend_from_slice(first);
        assert!(assembler.push(&packet).unwrap().is_none());
        let mut packet = (1_u16 | LAST_FRAGMENT).to_le_bytes().to_vec();
        packet.extend_from_slice(second);
        let frame = assembler.push(&packet).unwrap().unwrap();
        assert_eq!(frame.header.source_width, 800);
        assert_eq!(frame.data, vec![1, 2, 3, 4]);
    }

    #[test]
    fn single_fragment_frame() {
        let mut packet = LAST_FRAGMENT.to_le_bytes().to_vec();
        packet.extend(frame(1024, 768, &[9; 10]));
        let frame = FrameAssembler::default().push(&packet).unwrap().unwrap();
        assert_eq!(frame.header.source_height, 768);
        assert_eq!(frame.data.len(), 10);
    }
}
