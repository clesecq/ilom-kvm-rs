//! Virtual media redirection: CD-ROM (port 5120) and floppy/USB (port 5123).
//!
//! After a raw 20-byte token, the SP forwards every SCSI command the host
//! sends to its virtual USB drive as an IUSB SCSI packet. The client answers
//! each one in lockstep from a local image.

use std::{
    io::{Read, Write},
    net::TcpStream,
};

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};

use crate::{
    scsi::{self, MediaImage, MediaKind, Sense},
    tls,
    tokend::TOKEN_LEN,
};

pub const CDROM_PORT: u16 = 5120;
pub const FLOPPY_PORT: u16 = 5123;

const IUSB_SIGNATURE: &[u8; 8] = b"IUSB    ";
const IUSB_HEADER_LEN: usize = 32;
const DIRECTION_TO_SP: u8 = 0x80;

// Offsets inside an IUSB SCSI packet.
const CDB_OFFSET: usize = 41;
const CDB_LEN: usize = 12;
const STATUS_OFFSET: usize = 53;
const DATA_LEN_OFFSET: usize = 57;
const DATA_OFFSET: usize = 61;
const CONNECTION_STATUS_OFFSET: usize = 62;

const DEVICE_REDIRECTION_ACK: u8 = 0xf1;
const CONNECTION_ACCEPTED: u8 = 1;
const CONNECTION_BUSY: u8 = 2;
const CONNECTION_UNSUPPORTED: u8 = 3;

/// Largest packet accepted from the SP (a 256-block floppy write plus header).
const MAX_PACKET: usize = DATA_OFFSET + 256 * 512 + 1024;

pub const fn port(kind: MediaKind) -> u16 {
    match kind {
        MediaKind::Cdrom => CDROM_PORT,
        MediaKind::Floppy => FLOPPY_PORT,
    }
}

/// Reads one IUSB packet (header and data).
fn read_packet(stream: &mut impl Read) -> Result<Vec<u8>> {
    let mut packet = vec![0_u8; IUSB_HEADER_LEN];
    stream.read_exact(&mut packet).context("read IUSB header")?;
    if &packet[..8] != IUSB_SIGNATURE {
        bail!("unexpected IUSB signature {:02x?}", &packet[..8]);
    }
    let len = u32::from_le_bytes(packet[12..16].try_into().unwrap()) as usize;
    if IUSB_HEADER_LEN + len > MAX_PACKET {
        bail!("oversized IUSB packet ({len} data bytes)");
    }
    packet.resize(IUSB_HEADER_LEN + len, 0);
    stream
        .read_exact(&mut packet[IUSB_HEADER_LEN..])
        .context("read IUSB data")?;
    Ok(packet)
}

/// Builds the response to `request`: the request's fixed part is echoed
/// (header fields, tag, CDB) with the status, sense and data filled in.
pub fn response_packet(request: &[u8], result: &scsi::ScsiResult) -> Vec<u8> {
    let mut packet = request[..DATA_OFFSET].to_vec();
    let (status, Sense(key, asc, ascq), data) = match result {
        Ok(data) => (0, Sense(0, 0, 0), data.as_slice()),
        Err(sense) => (1, *sense, &[][..]),
    };
    packet[STATUS_OFFSET..DATA_LEN_OFFSET].copy_from_slice(&[status, key, asc, ascq]);
    packet[DATA_LEN_OFFSET..DATA_OFFSET].copy_from_slice(&(data.len() as u32).to_le_bytes());
    packet.extend_from_slice(data);
    let data_len = (packet.len() - IUSB_HEADER_LEN) as u32;
    packet[12..16].copy_from_slice(&data_len.to_le_bytes());
    packet[19] = DIRECTION_TO_SP;
    packet[11] = 0;
    let sum = packet[..IUSB_HEADER_LEN]
        .iter()
        .fold(0_u8, |acc, byte| acc.wrapping_add(*byte));
    packet[11] = sum.wrapping_neg();
    packet
}

/// Counters for a status display.
#[derive(Debug, Default, Clone, Copy)]
pub struct MediaStats {
    pub commands: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
}

pub struct MediaChannel {
    stream: TcpStream,
    kind: MediaKind,
}

impl MediaChannel {
    /// Connects and authenticates with a fresh tokend token. Fails when the
    /// SP refuses the redirection (for example another session already
    /// redirects this device).
    pub fn connect(host: &str, kind: MediaKind, token: &[u8; TOKEN_LEN]) -> Result<Self> {
        let mut stream = tls::connect_tcp(host, port(kind))
            .with_context(|| format!("connect to {} redirection", kind.label()))?;
        stream.write_all(token).context("send media token")?;
        let ack = read_packet(&mut stream).context("read media redirection ACK")?;
        let opcode = ack.get(CDB_OFFSET).copied().unwrap_or_default();
        if opcode != DEVICE_REDIRECTION_ACK {
            bail!("expected media redirection ACK, got opcode {opcode:#04x}");
        }
        let status = ack.get(CONNECTION_STATUS_OFFSET).copied();
        debug!(kind = kind.label(), ?status, ack = %hex::encode(&ack), "media ACK");
        match status {
            Some(CONNECTION_ACCEPTED) => {}
            Some(CONNECTION_BUSY) => bail!(
                "{} redirection is already in use by another session",
                kind.label()
            ),
            Some(CONNECTION_UNSUPPORTED) => {
                bail!("the host does not support {} redirection", kind.label())
            }
            other => bail!("{} redirection refused (status {other:?})", kind.label()),
        }
        // The host only sends commands while it uses the drive.
        stream.set_read_timeout(None)?;
        info!(kind = kind.label(), "media redirection accepted");
        Ok(Self { stream, kind })
    }

    pub fn try_clone_stream(&self) -> Result<TcpStream> {
        Ok(self.stream.try_clone()?)
    }

    /// Answers SP requests from `image` until the connection closes.
    /// `progress` is called after each command.
    pub fn serve(
        &mut self,
        image: &mut MediaImage,
        mut progress: impl FnMut(&MediaStats),
    ) -> Result<()> {
        let mut stats = MediaStats::default();
        loop {
            let request = match read_packet(&mut self.stream) {
                Ok(request) => request,
                Err(error) if is_closed(&error) => {
                    info!(kind = self.kind.label(), "media connection closed");
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            if request.len() < DATA_OFFSET {
                warn!(len = request.len(), "short IUSB SCSI packet ignored");
                continue;
            }
            let cdb: [u8; CDB_LEN] = request[CDB_OFFSET..CDB_OFFSET + CDB_LEN]
                .try_into()
                .unwrap();
            let result = image.execute(&cdb, &request[DATA_OFFSET..]);
            match &result {
                Ok(data) => debug!(
                    opcode = %format!("{:#04x}", cdb[0]),
                    command = scsi::opcode_name(cdb[0]),
                    cdb = %hex::encode(cdb),
                    bytes = data.len(),
                    "SCSI command"
                ),
                Err(sense) if *sense == scsi::INVALID_OPCODE => warn!(
                    opcode = %format!("{:#04x}", cdb[0]),
                    command = scsi::opcode_name(cdb[0]),
                    cdb = %hex::encode(cdb),
                    "unsupported SCSI command"
                ),
                Err(sense) => debug!(
                    opcode = %format!("{:#04x}", cdb[0]),
                    command = scsi::opcode_name(cdb[0]),
                    ?sense,
                    "SCSI command failed"
                ),
            }
            stats.commands += 1;
            if let Ok(data) = &result {
                stats.bytes_read += data.len() as u64;
            }
            if cdb[0] == scsi::WRITE_10 && result.is_ok() {
                stats.bytes_written += (request.len() - DATA_OFFSET) as u64;
            }
            self.stream
                .write_all(&response_packet(&request, &result))
                .context("send SCSI response")?;
            progress(&stats);
        }
    }

    pub fn close(self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

fn is_closed(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::NotConnected
            )
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(cdb: &[u8]) -> Vec<u8> {
        let mut packet = vec![0_u8; DATA_OFFSET];
        packet[..8].copy_from_slice(IUSB_SIGNATURE);
        packet[8..11].copy_from_slice(&[1, 0, 32]);
        packet[12..16].copy_from_slice(&29_u32.to_le_bytes());
        packet[17] = 0x05;
        packet[18] = 0x01;
        packet[24..28].copy_from_slice(&7_u32.to_le_bytes());
        packet[36..40].copy_from_slice(&0x1234_u32.to_le_bytes());
        packet[CDB_OFFSET..CDB_OFFSET + cdb.len()].copy_from_slice(cdb);
        packet
    }

    #[test]
    fn response_echoes_request_and_appends_data() {
        let request = request(&[scsi::READ_CAPACITY_10]);
        let response = response_packet(&request, &Ok(vec![0, 0, 0, 9, 0, 0, 8, 0]));
        assert_eq!(response.len(), DATA_OFFSET + 8);
        assert_eq!(&response[..8], IUSB_SIGNATURE);
        assert_eq!(u32::from_le_bytes(response[12..16].try_into().unwrap()), 37);
        assert_eq!(
            (response[17], response[18], response[19]),
            (0x05, 0x01, 0x80)
        );
        assert_eq!(&response[24..28], &7_u32.to_le_bytes());
        assert_eq!(&response[36..40], &0x1234_u32.to_le_bytes());
        assert_eq!(response[CDB_OFFSET], scsi::READ_CAPACITY_10);
        assert_eq!(&response[STATUS_OFFSET..DATA_LEN_OFFSET], [0, 0, 0, 0]);
        assert_eq!(
            &response[DATA_LEN_OFFSET..DATA_OFFSET],
            &8_u32.to_le_bytes()
        );
        assert_eq!(&response[DATA_OFFSET..], [0, 0, 0, 9, 0, 0, 8, 0]);
        let sum = response[..32]
            .iter()
            .fold(0_u8, |acc, byte| acc.wrapping_add(*byte));
        assert_eq!(sum, 0);
    }

    #[test]
    fn failed_command_carries_sense() {
        let request = request(&[0x12]);
        let response = response_packet(&request, &Err(scsi::INVALID_OPCODE));
        assert_eq!(response.len(), DATA_OFFSET);
        assert_eq!(
            &response[STATUS_OFFSET..DATA_LEN_OFFSET],
            [1, 0x05, 0x20, 0x00]
        );
        assert_eq!(&response[DATA_LEN_OFFSET..DATA_OFFSET], [0, 0, 0, 0]);
    }

    #[test]
    fn serves_requests_over_a_socket() {
        use std::{io::Cursor, net::TcpListener, thread};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let sp = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut token = [0_u8; TOKEN_LEN];
            socket.read_exact(&mut token).unwrap();
            assert_eq!(token, [9; TOKEN_LEN]);
            let mut ack = request(&[DEVICE_REDIRECTION_ACK]);
            ack.resize(DATA_OFFSET + 2, 0);
            ack[12..16].copy_from_slice(&31_u32.to_le_bytes());
            ack[CONNECTION_STATUS_OFFSET] = CONNECTION_ACCEPTED;
            socket.write_all(&ack).unwrap();
            socket
                .write_all(&request(&[scsi::READ_10, 0, 0, 0, 0, 1, 0, 0, 1]))
                .unwrap();
            let response = read_packet(&mut socket).unwrap();
            assert_eq!(response.len(), DATA_OFFSET + 2048);
            assert!(response[DATA_OFFSET..].iter().all(|&byte| byte == 1));
        });

        let data: Vec<u8> = (0..4 * 2048).map(|i| (i / 2048) as u8).collect();
        let len = data.len() as u64;
        let mut image = MediaImage::new(Box::new(Cursor::new(data)), len, MediaKind::Cdrom, false);
        let stream = TcpStream::connect(address).unwrap();
        let mut channel = MediaChannel {
            stream,
            kind: MediaKind::Cdrom,
        };
        channel.stream.write_all(&[9; TOKEN_LEN]).unwrap();
        let ack = read_packet(&mut channel.stream).unwrap();
        assert_eq!(ack[CONNECTION_STATUS_OFFSET], CONNECTION_ACCEPTED);
        let mut last = MediaStats::default();
        channel.serve(&mut image, |stats| last = *stats).unwrap();
        sp.join().unwrap();
        assert_eq!((last.commands, last.bytes_read), (1, 2048));
    }
}
