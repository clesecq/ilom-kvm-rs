use std::net::TcpStream;

use anyhow::{Context, Result, bail};
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::{
    crypto,
    ivtp::{self, Packet, RedirectPacket, fixed_field, video as kind},
    tls,
    tokend::Tokend,
};

pub const PORT: u16 = 7578;
pub const SERIAL_PORT: u16 = 7579;

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
    Ok(CompressedFrame {
        data: data[..size].to_vec(),
        header,
    })
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
    MouseMode(u8),
    ActiveClients(u8),
    Other(Packet),
}

pub struct VideoSession {
    stream: TcpStream,
    assembler: FrameAssembler,
    pub server_id: u8,
    pub server_version: u8,
}

impl VideoSession {
    /// Runs the AST2100 video handshake: device capabilities, server info,
    /// token authentication and the challenge login that follows it.
    pub fn connect(host: &str, username: &str, tokend: &mut Tokend) -> Result<Self> {
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
            bail!("video server is not AST2000-compatible (reserved {reserved:?}); only AST2100 ILOMs are supported");
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
        if reply.status != 0 {
            bail!("video token authentication failed with status {}", reply.status);
        }

        // After token authentication the vendor client still logs in, using the
        // literal password "token" against the salt and challenge from the SP.
        let salt_len = if server_version == 1 { 8 } else { 12 };
        let mut request = fixed_field(username.as_bytes(), USERNAME_FIELD);
        request.resize(USERNAME_FIELD + salt_len + CHALLENGE_LEN, 0);
        Packet::new(kind::GET_CHALLENGE, request).write_to(&mut stream)?;
        let challenge = Packet::read_from(&mut stream).context("read login challenge")?;
        if challenge.status == LOGIN_DENIED || challenge.status == 2 {
            bail!("video login denied for {username}");
        }
        if challenge.status != 0 || challenge.payload.len() < USERNAME_FIELD + salt_len + CHALLENGE_LEN {
            bail!("bad login challenge (status {})", challenge.status);
        }
        let salt = crypto::normalize_salt(&challenge.payload[USERNAME_FIELD..USERNAME_FIELD + salt_len]);
        let nonce = &challenge.payload[USERNAME_FIELD + salt_len..][..CHALLENGE_LEN];
        let hash = crypto::unix_hash("token", &salt, crypto::MD5_CRYPT_LEN)?;
        let mut login = fixed_field(username.as_bytes(), USERNAME_FIELD);
        login.extend_from_slice(&crypto::challenge_digest(&hash, nonce));
        Packet::new(kind::LOGIN, login).write_to(&mut stream)?;
        let reply = Packet::read_from(&mut stream).context("read login result")?;
        if reply.status != 0 {
            bail!("video login rejected with status {}", reply.status);
        }

        Packet::new(kind::GET_USB_MOUSE_MODE, vec![0]).write_to(&mut stream)?;
        info!("video redirection started");
        // Frames may take a while when the host screen is idle.
        stream.set_read_timeout(None)?;
        Ok(Self {
            stream,
            assembler: FrameAssembler::default(),
            server_id,
            server_version,
        })
    }

    pub fn try_clone_stream(&self) -> Result<TcpStream> {
        Ok(self.stream.try_clone()?)
    }

    pub fn next_event(&mut self) -> Result<VideoEvent> {
        loop {
            let packet = Packet::read_from(&mut self.stream)?;
            match packet.kind {
                kind::VIDEO_FRAGMENT => match self.assembler.push(&packet.payload) {
                    Ok(Some(frame)) => return Ok(VideoEvent::Frame(frame)),
                    Ok(None) => continue,
                    Err(error) => warn!(%error, "dropping malformed video frame"),
                },
                kind::BLANK_SCREEN => return Ok(VideoEvent::BlankScreen),
                kind::GET_USB_MOUSE_MODE => {
                    return Ok(VideoEvent::MouseMode(
                        packet.payload.first().copied().unwrap_or(0),
                    ));
                }
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
        let _ = Packet::new(ivtp::video::STOP_SESSION_IMMEDIATE, Vec::new()).write_to(&mut self.stream);
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

fn expect(stream: &mut TcpStream, expected: u8, what: &str) -> Result<Packet> {
    let packet = Packet::read_from(stream).with_context(|| format!("read {what}"))?;
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
