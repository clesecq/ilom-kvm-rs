//! Framing used by the ILOM redirection daemons (AMI "IVTP" and the older
//! "REDIRECT" header that is still used for the first video exchange).

use std::io::{Read, Write};

use anyhow::{Context, Result, bail};
use tracing::trace;

pub const HEADER_LEN: usize = 7;
const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

/// Packet types on the AST2000/AST2100 video channel.
pub mod video {
    pub const GET_CHALLENGE: u8 = 1;
    pub const LOGIN: u8 = 2;
    pub const VIDEO_FRAGMENT: u8 = 5;
    pub const PAUSE_REDIRECTION: u8 = 13;
    pub const RESUME_REDIRECTION: u8 = 14;
    pub const BLANK_SCREEN: u8 = 15;
    pub const STOP_SESSION_IMMEDIATE: u8 = 25;
    pub const GET_USB_MOUSE_MODE: u8 = 33;
    pub const SET_VIDEO_ENGINE_CONFIGS: u8 = 40;
    pub const GET_VIDEO_ENGINE_CONFIGS: u8 = 41;
    pub const HARDWARE_CURSOR: u8 = 48;
    pub const ACTIVE_CLIENTS: u8 = 51;
    pub const SERVER_INFO: u8 = 52;
    pub const TOKEN_AUTH: u8 = 58;
}

/// 7-byte little-endian header: `type u8, payload_len u32, status u16`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub kind: u8,
    pub status: u16,
    pub payload: Vec<u8>,
}

impl Packet {
    pub fn new(kind: u8, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            kind,
            status: 0,
            payload: payload.into(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.push(self.kind);
        out.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.status.to_le_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    pub fn write_to(&self, writer: &mut impl Write) -> Result<()> {
        writer.write_all(&self.encode())?;
        writer.flush()?;
        Ok(())
    }

    pub fn read_from(reader: &mut impl Read) -> Result<Self> {
        Self::read_with(reader, |_, announced| announced)
    }

    /// Reads one packet, letting `payload_len(kind, announced)` pick the real
    /// payload size. Some SP replies announce a length that does not match
    /// what they send (the vendor client parses fixed layouts instead).
    pub fn read_with(
        reader: &mut impl Read,
        payload_len: impl Fn(u8, usize) -> usize,
    ) -> Result<Self> {
        let mut header = [0_u8; HEADER_LEN];
        reader.read_exact(&mut header).context("read IVTP header")?;
        let announced = u32::from_le_bytes(header[1..5].try_into().unwrap()) as usize;
        let len = payload_len(header[0], announced);
        trace!(kind = header[0], announced, len, status = u16::from_le_bytes([header[5], header[6]]), "IVTP header");
        if len > MAX_PAYLOAD {
            bail!("IVTP packet type {} announces {len} bytes", header[0]);
        }
        let mut payload = vec![0_u8; len];
        reader
            .read_exact(&mut payload)
            .with_context(|| format!("read {len}-byte IVTP payload of type {}", header[0]))?;
        Ok(Self {
            kind: header[0],
            status: u16::from_le_bytes([header[5], header[6]]),
            payload,
        })
    }
}

pub const REDIRECT_HEADER_LEN: usize = 24;
const REDIRECT_SIGNATURE: &[u8; 8] = b"REDIRECT";

/// Legacy 24-byte header (`"REDIRECT"` signature), still used for the device
/// capabilities exchange that tells us which video server is listening.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedirectPacket {
    pub command: u16,
    pub status: u16,
    pub server_id: u8,
    pub server_version: u8,
    pub payload: Vec<u8>,
}

impl RedirectPacket {
    pub fn new(command: u16, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            command,
            status: 0,
            server_id: 0,
            server_version: 0,
            payload: payload.into(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(REDIRECT_HEADER_LEN + self.payload.len());
        out.extend_from_slice(REDIRECT_SIGNATURE);
        out.extend_from_slice(&(REDIRECT_HEADER_LEN as u16).to_le_bytes());
        out.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.command.to_le_bytes());
        out.extend_from_slice(&self.status.to_le_bytes());
        // Originator 1 = client.
        out.extend_from_slice(&[1, self.server_id, self.server_version, 0, 0, 0]);
        out.extend_from_slice(&self.payload);
        out
    }

    pub fn write_to(&self, writer: &mut impl Write) -> Result<()> {
        writer.write_all(&self.encode())?;
        writer.flush()?;
        Ok(())
    }

    pub fn read_from(reader: &mut impl Read) -> Result<Self> {
        let mut header = [0_u8; REDIRECT_HEADER_LEN];
        reader
            .read_exact(&mut header)
            .context("read REDIRECT header")?;
        if &header[..8] != REDIRECT_SIGNATURE {
            bail!("invalid REDIRECT signature {:02x?}", &header[..8]);
        }
        let header_len = u16::from_le_bytes([header[8], header[9]]) as usize;
        let len = u32::from_le_bytes(header[10..14].try_into().unwrap()) as usize;
        if len > MAX_PAYLOAD || header_len < REDIRECT_HEADER_LEN {
            bail!("invalid REDIRECT lengths: header {header_len}, payload {len}");
        }
        let mut extra = vec![0_u8; header_len - REDIRECT_HEADER_LEN];
        reader.read_exact(&mut extra)?;
        let mut payload = vec![0_u8; len];
        reader.read_exact(&mut payload).context("read REDIRECT payload")?;
        Ok(Self {
            command: u16::from_le_bytes([header[14], header[15]]),
            status: u16::from_le_bytes([header[16], header[17]]),
            server_id: header[19],
            server_version: header[20],
            payload,
        })
    }
}

/// Copies `value` into a zero-filled field of `len` bytes, truncating it.
pub fn fixed_field(value: &[u8], len: usize) -> Vec<u8> {
    let mut field = vec![0_u8; len];
    let copy = value.len().min(len);
    field[..copy].copy_from_slice(&value[..copy]);
    field
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ivtp_round_trip() {
        let packet = Packet::new(video::TOKEN_AUTH, vec![7; 20]);
        let encoded = packet.encode();
        assert_eq!(encoded.len(), HEADER_LEN + 20);
        assert_eq!(&encoded[..7], &[58, 20, 0, 0, 0, 0, 0]);
        let decoded = Packet::read_from(&mut encoded.as_slice()).unwrap();
        assert_eq!(decoded, packet);
    }

    #[test]
    fn redirect_round_trip() {
        let packet = RedirectPacket::new(6, vec![0, 0, 0, 2]);
        let encoded = packet.encode();
        assert_eq!(encoded.len(), 28);
        assert_eq!(&encoded[..8], b"REDIRECT");
        assert_eq!(encoded[18], 1);
        let decoded = RedirectPacket::read_from(&mut encoded.as_slice()).unwrap();
        assert_eq!(decoded.command, 6);
        assert_eq!(decoded.payload, vec![0, 0, 0, 2]);
    }
}
