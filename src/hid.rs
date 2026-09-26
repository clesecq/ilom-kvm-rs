//! Keyboard and mouse redirection (port 5121).
//!
//! Control messages use a 16-byte `HIDCMD  ` header. Input reports are raw
//! IUSB packets (32-byte header plus a short report), not wrapped in IVTP.

use std::{
    io::{Read, Write},
    net::TcpStream,
};

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};

use crate::{crypto, ivtp::fixed_field, tls, tokend::Tokend};

pub const PORT: u16 = 5121;

const HID_SIGNATURE: &[u8; 8] = b"HIDCMD  ";
const IUSB_SIGNATURE: &[u8; 8] = b"IUSB    ";
const HID_HEADER_LEN: usize = 16;
const IUSB_HEADER_LEN: usize = 32;

const START_REDIRECTION: u16 = 100;
const DEVCAPS: u16 = 102;
const PORTCAPS: u16 = 103;
const ENCRYPTION_CHALLENGE: u16 = 107;
const TOKEN: u16 = 115;

const USERNAME_FIELD: usize = 16;
/// Salt field length for AST2100 servers (version > 1).
const SALT_LEN: usize = 12;
const CHALLENGE_LEN: usize = 32;

const DEVICE_KEYBOARD: u8 = 0x30;
const DEVICE_MOUSE: u8 = 0x31;
const PROTOCOL_KEYBOARD: u8 = 0x10;
const PROTOCOL_MOUSE: u8 = 0x20;
const DIRECTION_TO_DEVICE: u8 = 0x80;

/// Maximum coordinate of the absolute pointer report.
const ABS_MAX: u32 = 32767;

pub const BUTTON_LEFT: u8 = 1;
pub const BUTTON_RIGHT: u8 = 2;
pub const BUTTON_MIDDLE: u8 = 4;

#[derive(Debug)]
struct HidReply {
    command: u16,
    status: u16,
    payload: Vec<u8>,
}

fn hid_command(command: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HID_HEADER_LEN + payload.len());
    out.extend_from_slice(HID_SIGNATURE);
    out.extend_from_slice(&command.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Payload sizes the vendor client parses for each reply. As on the video
/// channel, announced lengths are not trusted.
fn reply_len(command: u16, salt_len: usize) -> usize {
    match command {
        START_REDIRECTION => 4,
        DEVCAPS | PORTCAPS => 8,
        ENCRYPTION_CHALLENGE => USERNAME_FIELD + salt_len + CHALLENGE_LEN,
        _ => 0,
    }
}

fn read_reply(stream: &mut impl Read, salt_len: usize) -> Result<HidReply> {
    let mut header = [0_u8; HID_HEADER_LEN];
    stream.read_exact(&mut header).context("read HID header")?;
    if &header[..8] != HID_SIGNATURE {
        bail!("unexpected HID reply signature {:02x?}", &header[..8]);
    }
    let command = u16::from_le_bytes([header[8], header[9]]);
    let status = u16::from_le_bytes([header[10], header[11]]);
    let announced = u32::from_le_bytes(header[12..16].try_into().unwrap());
    let len = if status == 0 { reply_len(command, salt_len) } else { 0 };
    let mut payload = vec![0_u8; len];
    stream.read_exact(&mut payload)?;
    debug!(command, status, announced, payload = %hex::encode(&payload), "HID reply");
    Ok(HidReply {
        command,
        status,
        payload,
    })
}

fn exchange(stream: &mut TcpStream, command: u16, payload: &[u8], what: &str) -> Result<HidReply> {
    stream.write_all(&hid_command(command, payload))?;
    let reply = read_reply(stream, SALT_LEN).with_context(|| format!("read {what} reply"))?;
    if reply.command != command {
        bail!("expected {what} reply ({command}), got command {}", reply.command);
    }
    if reply.status != 0 {
        bail!("{what} failed with HID status {}", reply.status);
    }
    Ok(reply)
}

/// AES key material negotiated for HID encryption level 2.
#[derive(Clone)]
pub struct HidCipher {
    key: [u8; 16],
    iv: [u8; 16],
}

/// Builds an IUSB packet with the header checksum in byte 11. `data[0]` is
/// the report length byte; with a cipher the rest of the report is replaced
/// by its AES-CBC encryption and reserved byte 0 flags it as encrypted.
fn iusb_packet(
    device: u8,
    protocol: u8,
    interface: u8,
    sequence: u32,
    data: &[u8],
    cipher: Option<&HidCipher>,
) -> Vec<u8> {
    let body = match cipher {
        Some(cipher) => {
            let mut body = vec![data[0]];
            body.extend(crypto::aes_cbc_encrypt(&cipher.key, &cipher.iv, &data[1..]));
            body
        }
        None => data.to_vec(),
    };
    let mut packet = Vec::with_capacity(IUSB_HEADER_LEN + body.len());
    packet.extend_from_slice(IUSB_SIGNATURE);
    packet.extend_from_slice(&[1, 0, IUSB_HEADER_LEN as u8, 0]);
    packet.extend_from_slice(&(body.len() as u32).to_le_bytes());
    packet.extend_from_slice(&[0, device, protocol, DIRECTION_TO_DEVICE, 2, interface, 0, 0]);
    packet.extend_from_slice(&sequence.to_le_bytes());
    packet.extend_from_slice(&[cipher.is_some() as u8, 0, 0, 0]);
    let sum = packet.iter().fold(0_u8, |acc, byte| acc.wrapping_add(*byte));
    packet[11] = sum.wrapping_neg();
    packet.extend_from_slice(&body);
    packet
}

/// Keyboard report: `[8, modifiers, keybreak, key1..key6]`. With `keybreak`
/// set the SP releases the keys by itself (the vendor client's default
/// "auto keybreak" mode), so no separate release report is needed.
pub fn keyboard_packet(
    sequence: u32,
    modifiers: u8,
    usages: &[u8],
    keybreak: bool,
    cipher: Option<&HidCipher>,
) -> Vec<u8> {
    let mut report = [0_u8; 9];
    report[0] = 8;
    report[1] = modifiers;
    report[2] = keybreak as u8;
    for (slot, usage) in report[3..].iter_mut().zip(usages.iter().take(6)) {
        *slot = *usage;
    }
    iusb_packet(DEVICE_KEYBOARD, PROTOCOL_KEYBOARD, 0, sequence, &report, cipher)
}

/// Absolute pointer report; `x`/`y` are remote framebuffer pixels.
pub fn absolute_mouse_packet(
    sequence: u32,
    buttons: u8,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    cipher: Option<&HidCipher>,
) -> Vec<u8> {
    let scale = |value: u32, extent: u32| -> u16 {
        if extent == 0 {
            return 0;
        }
        ((value.min(extent) as u64 * ABS_MAX as u64 / extent as u64) as f64 + 0.5) as u16
    };
    let mut report = Vec::with_capacity(13);
    report.extend_from_slice(&[12, buttons, 0, 0]);
    report.extend_from_slice(&scale(x, width).to_le_bytes());
    report.extend_from_slice(&scale(y, height).to_le_bytes());
    report.extend_from_slice(&1024_u16.to_le_bytes());
    report.extend_from_slice(&768_u16.to_le_bytes());
    report.push(0);
    iusb_packet(DEVICE_MOUSE, PROTOCOL_MOUSE, 1, sequence, &report, cipher)
}

/// Relative pointer report with signed deltas.
pub fn relative_mouse_packet(
    sequence: u32,
    buttons: u8,
    dx: i8,
    dy: i8,
    cipher: Option<&HidCipher>,
) -> Vec<u8> {
    iusb_packet(
        DEVICE_MOUSE,
        PROTOCOL_MOUSE,
        1,
        sequence,
        &[3, buttons, dx as u8, dy as u8],
        cipher,
    )
}

pub struct HidSession {
    stream: TcpStream,
    cipher: Option<HidCipher>,
    pub absolute: bool,
}

impl HidSession {
    pub fn connect(host: &str, username: &str, tokend: &mut Tokend) -> Result<Self> {
        let mut stream = tls::connect_tcp(host, PORT).context("connect to HID server")?;

        let token = tokend.redirection_token()?;
        let mut payload = fixed_field(username.as_bytes(), 16);
        payload.extend_from_slice(&token);
        exchange(&mut stream, TOKEN, &payload, "HID token")?;

        // protocol u8, ports u32, reserved[3] = [2, 0, 0]
        let client_level: u8 = std::env::var("ILOM_HID_LEVEL").ok().and_then(|v| v.parse().ok()).unwrap_or(2);
        let devcaps = exchange(&mut stream, DEVCAPS, &[0, 0, 0, 0, 0, client_level, 0, 0], "HID devcaps")?;
        let encryption = devcaps.payload[5];
        exchange(&mut stream, PORTCAPS, &[0; 8], "HID portcaps")?;

        let cipher = match encryption {
            0 => None,
            2 => {
                let mut request = fixed_field(username.as_bytes(), USERNAME_FIELD);
                request.resize(USERNAME_FIELD + SALT_LEN + CHALLENGE_LEN, 0);
                let reply = exchange(&mut stream, ENCRYPTION_CHALLENGE, &request, "HID encryption challenge")?;
                let challenge = &reply.payload[USERNAME_FIELD + SALT_LEN..];
                let echoed = String::from_utf8_lossy(&reply.payload[..USERNAME_FIELD])
                    .trim_end_matches('\0')
                    .to_string();
                let handle: [u8; 4] = challenge[..4].try_into().unwrap();
                let data = tokend.challenge_data(&handle)?;
                debug!(username, %echoed, challenge_data = %hex::encode(data), "HID key inputs");
                let key_user = if std::env::var("ILOM_HID_KEY_USER").as_deref() == Ok("echo") {
                    echoed.as_str()
                } else {
                    username
                };
                Some(HidCipher {
                    key: crypto::hid_aes_key(key_user, &data),
                    iv: data[..16].try_into().unwrap(),
                })
            }
            level => bail!("HID encryption level {level} (Blowfish) is not implemented"),
        };
        debug!(encryption, "HID encryption negotiated");

        // port u16, mode (0 = absolute), reserved
        let start = exchange(&mut stream, START_REDIRECTION, &[0, 0, 0, 0], "HID start")?;
        let absolute = start.payload[2] == 0;
        info!(absolute, "HID redirection started");
        if !absolute {
            warn!("SP selected relative mouse mode");
        }
        Ok(Self {
            stream,
            cipher,
            absolute,
        })
    }

    // The vendor client always sends IUSB sequence number 0.
    fn next_sequence(&mut self) -> u32 {
        0
    }

    /// Sends the full keyboard state (held modifiers and keys).
    pub fn send_keyboard(&mut self, modifiers: u8, usages: &[u8]) -> Result<()> {
        let sequence = self.next_sequence();
        self.stream.write_all(&keyboard_packet(
            sequence,
            modifiers,
            usages,
            false,
            self.cipher.as_ref(),
        ))?;
        Ok(())
    }

    /// Presses `usage` with `modifiers` and lets the SP release it.
    pub fn send_keystroke(&mut self, modifiers: u8, usage: u8) -> Result<()> {
        let sequence = self.next_sequence();
        self.stream.write_all(&keyboard_packet(
            sequence,
            modifiers,
            &[usage],
            true,
            self.cipher.as_ref(),
        ))?;
        Ok(())
    }

    pub fn send_absolute_mouse(&mut self, buttons: u8, x: u32, y: u32, width: u32, height: u32) -> Result<()> {
        let sequence = self.next_sequence();
        self.stream
            .write_all(&absolute_mouse_packet(
                sequence,
                buttons,
                x,
                y,
                width,
                height,
                self.cipher.as_ref(),
            ))?;
        Ok(())
    }

    pub fn send_relative_mouse(&mut self, buttons: u8, dx: i8, dy: i8) -> Result<()> {
        let sequence = self.next_sequence();
        self.stream
            .write_all(&relative_mouse_packet(sequence, buttons, dx, dy, self.cipher.as_ref()))?;
        Ok(())
    }

    /// Stream for reading IUSB status / LED packets on another thread.
    pub fn try_clone_stream(&self) -> Result<TcpStream> {
        Ok(self.stream.try_clone()?)
    }

    pub fn close(mut self) {
        let _ = self.send_keyboard(0, &[]);
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iusb_keyboard_packet_layout() {
        let packet = keyboard_packet(3, 0x02, &[0x04], false, None);
        assert_eq!(packet.len(), IUSB_HEADER_LEN + 9);
        assert_eq!(&packet[..8], IUSB_SIGNATURE);
        let sum = packet[..IUSB_HEADER_LEN]
            .iter()
            .fold(0_u8, |acc, byte| acc.wrapping_add(*byte));
        assert_eq!(sum, 0);
        assert_eq!(u32::from_le_bytes(packet[12..16].try_into().unwrap()), 9);
        assert_eq!(&packet[IUSB_HEADER_LEN..], &[8, 0x02, 0, 0x04, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn absolute_mouse_scales_to_full_range() {
        let packet = absolute_mouse_packet(0, BUTTON_LEFT, 1024, 768, 1024, 768, None);
        let report = &packet[IUSB_HEADER_LEN..];
        assert_eq!(report[0], 12);
        assert_eq!(report[1], BUTTON_LEFT);
        assert_eq!(u16::from_le_bytes([report[4], report[5]]), 32767);
        assert_eq!(u16::from_le_bytes([report[6], report[7]]), 32767);
    }

    #[test]
    fn encrypted_packet_keeps_length_byte_in_clear() {
        let cipher = HidCipher { key: [1; 16], iv: [2; 16] };
        let packet = keyboard_packet(0, 0, &[0x04], true, Some(&cipher));
        assert_eq!(packet.len(), IUSB_HEADER_LEN + 17);
        assert_eq!(u32::from_le_bytes(packet[12..16].try_into().unwrap()), 17);
        assert_eq!(packet[28], 1);
        assert_eq!(packet[IUSB_HEADER_LEN], 8);
    }

    #[test]
    fn hid_command_header() {
        let packet = hid_command(DEVCAPS, &[1, 2]);
        assert_eq!(&packet[..8], HID_SIGNATURE);
        assert_eq!(packet[8], 102);
        assert_eq!(u32::from_le_bytes(packet[12..16].try_into().unwrap()), 2);
    }
}
