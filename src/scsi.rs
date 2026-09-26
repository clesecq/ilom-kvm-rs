//! SCSI emulation of a virtual CD-ROM or floppy/USB disk backed by an image.
//!
//! The SP answers device-identity commands (INQUIRY, MODE SENSE, ...) itself
//! and forwards only media access, so this covers the command set the vendor
//! image readers implement, with the same sense codes.

use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

use anyhow::{Context, Result, bail};
use tracing::warn;

pub const TEST_UNIT_READY: u8 = 0x00;
pub const FORMAT_UNIT: u8 = 0x04;
pub const START_STOP_UNIT: u8 = 0x1b;
pub const PREVENT_ALLOW_REMOVAL: u8 = 0x1e;
pub const READ_FORMAT_CAPACITIES: u8 = 0x23;
pub const READ_CAPACITY_10: u8 = 0x25;
pub const READ_10: u8 = 0x28;
pub const WRITE_10: u8 = 0x2a;
pub const READ_TOC: u8 = 0x43;
pub const READ_12: u8 = 0xa8;

/// ISO 9660 primary volume descriptor identifier, at byte 1 of sector 16.
const ISO_SIGNATURE: &[u8; 5] = b"CD001";
const ISO_SIGNATURE_OFFSET: u64 = 16 * 2048 + 1;
/// Lead-in length added to LBAs for MSF addresses (2 seconds).
const MSF_OFFSET: u64 = 150;
const LEAD_OUT_TRACK: u8 = 0xaa;
/// The vendor floppy reader refuses transfers above 256 blocks.
const FLOPPY_MAX_BLOCKS: u32 = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Cdrom,
    Floppy,
}

impl MediaKind {
    pub const fn block_size(self) -> u32 {
        match self {
            Self::Cdrom => 2048,
            Self::Floppy => 512,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Cdrom => "CD-ROM",
            Self::Floppy => "floppy",
        }
    }
}

/// Sense data of a failed command: key, additional sense code and qualifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sense(pub u8, pub u8, pub u8);

pub const INVALID_OPCODE: Sense = Sense(0x05, 0x20, 0x00);
pub const LBA_OUT_OF_RANGE: Sense = Sense(0x05, 0x21, 0x00);
pub const INVALID_FIELD_IN_CDB: Sense = Sense(0x05, 0x24, 0x00);
pub const INVALID_FIELD_IN_PARAMETERS: Sense = Sense(0x05, 0x26, 0x00);
pub const MEDIUM_CHANGED: Sense = Sense(0x06, 0x28, 0x00);
pub const UNRECOVERED_READ_ERROR: Sense = Sense(0x03, 0x11, 0x00);
pub const WRITE_ERROR: Sense = Sense(0x03, 0x0c, 0x00);
pub const WRITE_PROTECTED: Sense = Sense(0x07, 0x27, 0x00);

/// Result of one command: `Err` carries CHECK CONDITION sense data.
pub type ScsiResult = Result<Vec<u8>, Sense>;

pub trait Backing: Read + Write + Seek + Send {}
impl<T: Read + Write + Seek + Send> Backing for T {}

pub struct MediaImage {
    file: Box<dyn Backing>,
    kind: MediaKind,
    blocks: u64,
    writable: bool,
    /// Report "medium changed" once, as a newly inserted disc would.
    unit_attention: bool,
}

impl MediaImage {
    /// Opens an image file. CD images must be ISO 9660; floppy images are raw
    /// sector dumps and are opened read-only unless `writable` is set.
    pub fn open(path: &Path, kind: MediaKind, writable: bool) -> Result<Self> {
        let writable = writable && kind == MediaKind::Floppy;
        let mut file = OpenOptions::new()
            .read(true)
            .write(writable)
            .open(path)
            .with_context(|| format!("open {}", path.display()))?;
        let len = file.metadata()?.len();
        if kind == MediaKind::Cdrom && !has_iso_signature(&mut file)? {
            bail!("{} is not an ISO 9660 image", path.display());
        }
        if len % u64::from(kind.block_size()) != 0 {
            warn!(
                path = %path.display(),
                len,
                "image size is not a whole number of blocks; the tail is ignored"
            );
        }
        Ok(Self::new(Box::new(file), len, kind, writable))
    }

    pub fn new(file: Box<dyn Backing>, len: u64, kind: MediaKind, writable: bool) -> Self {
        Self {
            file,
            kind,
            blocks: len / u64::from(kind.block_size()),
            writable,
            unit_attention: true,
        }
    }

    pub fn kind(&self) -> MediaKind {
        self.kind
    }

    pub fn blocks(&self) -> u64 {
        self.blocks
    }

    pub fn writable(&self) -> bool {
        self.writable
    }

    /// Runs one command. `data_out` is the data the host sent with it
    /// (WRITE payload).
    pub fn execute(&mut self, cdb: &[u8; 12], data_out: &[u8]) -> ScsiResult {
        let opcode = cdb[0];
        match (self.kind, opcode) {
            (_, TEST_UNIT_READY) => self.check_attention().map(|()| Vec::new()),
            // The vendor CD reader rejects PREVENT ALLOW; hosts cope either way.
            (_, START_STOP_UNIT | PREVENT_ALLOW_REMOVAL) => Ok(Vec::new()),
            (_, READ_CAPACITY_10) => {
                self.check_attention()?;
                Ok(self.read_capacity())
            }
            (_, READ_10) => self.read(be32(&cdb[2..6]), u32::from(be16(&cdb[7..9]))),
            (MediaKind::Cdrom, READ_12) => self.read(be32(&cdb[2..6]), be32(&cdb[6..10])),
            (MediaKind::Cdrom, READ_TOC) => self.read_toc(cdb),
            (MediaKind::Floppy, FORMAT_UNIT) => Ok(Vec::new()),
            (MediaKind::Floppy, READ_FORMAT_CAPACITIES) => Ok(truncate(
                self.read_format_capacities(),
                usize::from(be16(&cdb[7..9])),
            )),
            (MediaKind::Floppy, WRITE_10) => {
                self.write(be32(&cdb[2..6]), u32::from(be16(&cdb[7..9])), data_out)
            }
            _ => Err(INVALID_OPCODE),
        }
    }

    fn check_attention(&mut self) -> Result<(), Sense> {
        if std::mem::take(&mut self.unit_attention) {
            return Err(MEDIUM_CHANGED);
        }
        Ok(())
    }

    fn last_lba(&self) -> u32 {
        self.blocks.saturating_sub(1).min(u64::from(u32::MAX)) as u32
    }

    fn read_capacity(&self) -> Vec<u8> {
        let mut data = self.last_lba().to_be_bytes().to_vec();
        data.extend_from_slice(&self.kind.block_size().to_be_bytes());
        data
    }

    fn read_format_capacities(&self) -> Vec<u8> {
        const FORMATTED_MEDIA: u32 = 0x0200_0000;
        let mut data = vec![0, 0, 0, 8];
        data.extend_from_slice(&(self.blocks.min(u64::from(u32::MAX)) as u32).to_be_bytes());
        data.extend_from_slice(&(FORMATTED_MEDIA | self.kind.block_size()).to_be_bytes());
        data
    }

    fn check_range(&self, lba: u32, count: u32) -> Result<(), Sense> {
        if u64::from(lba) + u64::from(count) > self.blocks {
            return Err(LBA_OUT_OF_RANGE);
        }
        if self.kind == MediaKind::Floppy && count > FLOPPY_MAX_BLOCKS {
            return Err(INVALID_FIELD_IN_PARAMETERS);
        }
        Ok(())
    }

    fn offset(&self, lba: u32) -> u64 {
        u64::from(lba) * u64::from(self.kind.block_size())
    }

    fn read(&mut self, lba: u32, count: u32) -> ScsiResult {
        self.check_range(lba, count)?;
        let mut data = vec![0_u8; count as usize * self.kind.block_size() as usize];
        let offset = self.offset(lba);
        self.file
            .seek(SeekFrom::Start(offset))
            .and_then(|_| self.file.read_exact(&mut data))
            .map_err(|error| {
                warn!(%error, lba, count, "image read failed");
                UNRECOVERED_READ_ERROR
            })?;
        Ok(data)
    }

    fn write(&mut self, lba: u32, count: u32, data: &[u8]) -> ScsiResult {
        if !self.writable {
            return Err(WRITE_PROTECTED);
        }
        self.check_range(lba, count)?;
        let len = count as usize * self.kind.block_size() as usize;
        let Some(data) = data.get(..len) else {
            return Err(INVALID_FIELD_IN_PARAMETERS);
        };
        let offset = self.offset(lba);
        self.file
            .seek(SeekFrom::Start(offset))
            .and_then(|_| self.file.write_all(data))
            .and_then(|()| self.file.flush())
            .map_err(|error| {
                warn!(%error, lba, count, "image write failed");
                WRITE_ERROR
            })?;
        Ok(Vec::new())
    }

    /// READ TOC for a single data track. Formats 0 (TOC) and 1 (session
    /// info) are distinguished; like the vendor reader, others get the TOC.
    fn read_toc(&self, cdb: &[u8; 12]) -> ScsiResult {
        let msf = cdb[1] & 0x02 != 0;
        let format = match cdb[2] & 0x0f {
            0 => cdb[9] >> 6,
            format => format,
        };
        let start_track = cdb[6];
        let allocation = usize::from(be16(&cdb[7..9]));
        let address = |lba: u64| -> [u8; 4] {
            if msf {
                let frames = lba + MSF_OFFSET;
                [
                    0,
                    (frames / 4500) as u8,
                    (frames / 75 % 60) as u8,
                    (frames % 75) as u8,
                ]
            } else {
                (lba as u32).to_be_bytes()
            }
        };
        let descriptor = |adr_control: u8, track: u8, lba: u64| -> Vec<u8> {
            let mut entry = vec![0, adr_control, track, 0];
            entry.extend_from_slice(&address(lba));
            entry
        };
        // ADR 1 (current position), control 4 = data track.
        let data_track = descriptor(0x14, 1, 0);
        let mut body = if format == 1 {
            data_track
        } else {
            let lead_out = descriptor(0x16, LEAD_OUT_TRACK, self.blocks);
            match start_track {
                0 | 1 => [data_track, lead_out].concat(),
                LEAD_OUT_TRACK => lead_out,
                _ => return Err(INVALID_FIELD_IN_CDB),
            }
        };
        let mut data = ((body.len() + 2) as u16).to_be_bytes().to_vec();
        data.extend_from_slice(&[1, 1]);
        data.append(&mut body);
        Ok(truncate(data, allocation))
    }
}

fn has_iso_signature(file: &mut File) -> Result<bool> {
    let mut signature = [0_u8; 5];
    file.seek(SeekFrom::Start(ISO_SIGNATURE_OFFSET))?;
    let found = file.read_exact(&mut signature).is_ok() && &signature == ISO_SIGNATURE;
    file.seek(SeekFrom::Start(0))?;
    Ok(found)
}

fn truncate(mut data: Vec<u8>, allocation: usize) -> Vec<u8> {
    data.truncate(allocation);
    data
}

fn be16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// Human-readable command name for logs.
pub fn opcode_name(opcode: u8) -> &'static str {
    match opcode {
        TEST_UNIT_READY => "TEST UNIT READY",
        FORMAT_UNIT => "FORMAT UNIT",
        START_STOP_UNIT => "START STOP UNIT",
        PREVENT_ALLOW_REMOVAL => "PREVENT ALLOW MEDIUM REMOVAL",
        READ_FORMAT_CAPACITIES => "READ FORMAT CAPACITIES",
        READ_CAPACITY_10 => "READ CAPACITY(10)",
        READ_10 => "READ(10)",
        WRITE_10 => "WRITE(10)",
        READ_TOC => "READ TOC",
        READ_12 => "READ(12)",
        0x03 => "REQUEST SENSE",
        0x12 => "INQUIRY",
        0x1a => "MODE SENSE(6)",
        0x46 => "GET CONFIGURATION",
        0x4a => "GET EVENT STATUS NOTIFICATION",
        0x51 => "READ DISC INFORMATION",
        0x5a => "MODE SENSE(10)",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn image(kind: MediaKind, blocks: usize, writable: bool) -> MediaImage {
        let size = kind.block_size() as usize;
        let data: Vec<u8> = (0..blocks * size).map(|i| (i / size) as u8).collect();
        let len = data.len() as u64;
        let mut image = MediaImage::new(Box::new(Cursor::new(data)), len, kind, writable);
        image.unit_attention = false;
        image
    }

    fn cdb(bytes: &[u8]) -> [u8; 12] {
        let mut cdb = [0_u8; 12];
        cdb[..bytes.len()].copy_from_slice(bytes);
        cdb
    }

    #[test]
    fn first_test_unit_ready_reports_medium_change() {
        let mut image = image(MediaKind::Cdrom, 4, false);
        image.unit_attention = true;
        assert_eq!(
            image.execute(&cdb(&[TEST_UNIT_READY]), &[]),
            Err(MEDIUM_CHANGED)
        );
        assert_eq!(image.execute(&cdb(&[TEST_UNIT_READY]), &[]), Ok(Vec::new()));
    }

    #[test]
    fn read_capacity_reports_last_lba_and_block_size() {
        let mut image = image(MediaKind::Cdrom, 300, false);
        let data = image.execute(&cdb(&[READ_CAPACITY_10]), &[]).unwrap();
        assert_eq!(data, [0, 0, 1, 43, 0, 0, 8, 0]);
    }

    #[test]
    fn read_10_returns_requested_sectors() {
        let mut image = image(MediaKind::Cdrom, 8, false);
        let data = image
            .execute(&cdb(&[READ_10, 0, 0, 0, 0, 2, 0, 0, 3]), &[])
            .unwrap();
        assert_eq!(data.len(), 3 * 2048);
        assert_eq!((data[0], data[2048], data[4096]), (2, 3, 4));
    }

    #[test]
    fn read_12_uses_32_bit_length() {
        let mut image = image(MediaKind::Cdrom, 8, false);
        let data = image
            .execute(&cdb(&[READ_12, 0, 0, 0, 0, 7, 0, 0, 0, 1]), &[])
            .unwrap();
        assert_eq!((data.len(), data[0]), (2048, 7));
    }

    #[test]
    fn reads_past_the_end_are_rejected() {
        let mut image = image(MediaKind::Cdrom, 8, false);
        let result = image.execute(&cdb(&[READ_10, 0, 0, 0, 0, 7, 0, 0, 2]), &[]);
        assert_eq!(result, Err(LBA_OUT_OF_RANGE));
    }

    #[test]
    fn toc_lists_data_track_and_lead_out() {
        let mut image = image(MediaKind::Cdrom, 1000, false);
        let lba = image
            .execute(&cdb(&[READ_TOC, 0, 0, 0, 0, 0, 0, 0, 0xff]), &[])
            .unwrap();
        assert_eq!(
            lba,
            [
                0, 18, 1, 1, 0, 0x14, 1, 0, 0, 0, 0, 0, 0, 0x16, 0xaa, 0, 0, 0, 0x03, 0xe8
            ]
        );
        let msf = image
            .execute(&cdb(&[READ_TOC, 0x02, 0, 0, 0, 0, 0, 0, 0xff]), &[])
            .unwrap();
        // Track 1 at 00:02:00; lead-out at LBA 1000 + 150 = 00:15:25.
        assert_eq!(&msf[8..12], [0, 0, 2, 0]);
        assert_eq!(&msf[16..20], [0, 0, 15, 25]);
    }

    #[test]
    fn toc_honours_allocation_length() {
        let mut image = image(MediaKind::Cdrom, 10, false);
        let data = image
            .execute(&cdb(&[READ_TOC, 0, 0, 0, 0, 0, 0, 0, 4]), &[])
            .unwrap();
        assert_eq!(data, [0, 18, 1, 1]);
    }

    #[test]
    fn floppy_format_capacities() {
        let mut image = image(MediaKind::Floppy, 2880, false);
        let data = image
            .execute(
                &cdb(&[READ_FORMAT_CAPACITIES, 0, 0, 0, 0, 0, 0, 0, 0xfc]),
                &[],
            )
            .unwrap();
        assert_eq!(data, [0, 0, 0, 8, 0, 0, 0x0b, 0x40, 2, 0, 2, 0]);
    }

    #[test]
    fn floppy_write_needs_writable_image() {
        let write = cdb(&[WRITE_10, 0, 0, 0, 0, 1, 0, 0, 1]);
        let payload = vec![0xee_u8; 512];
        let mut read_only = image(MediaKind::Floppy, 4, false);
        assert_eq!(read_only.execute(&write, &payload), Err(WRITE_PROTECTED));

        let mut writable = image(MediaKind::Floppy, 4, true);
        assert_eq!(writable.execute(&write, &payload), Ok(Vec::new()));
        let data = writable
            .execute(&cdb(&[READ_10, 0, 0, 0, 0, 1, 0, 0, 1]), &[])
            .unwrap();
        assert!(data.iter().all(|&byte| byte == 0xee));
    }

    #[test]
    fn floppy_rejects_oversized_transfers() {
        let mut image = image(MediaKind::Floppy, 1000, false);
        let result = image.execute(&cdb(&[READ_10, 0, 0, 0, 0, 0, 0, 1, 1]), &[]);
        assert_eq!(result, Err(INVALID_FIELD_IN_PARAMETERS));
    }

    #[test]
    fn unsupported_commands_are_invalid_opcodes() {
        let mut cd = image(MediaKind::Cdrom, 4, false);
        assert_eq!(cd.execute(&cdb(&[0x12]), &[]), Err(INVALID_OPCODE));
        assert_eq!(cd.execute(&cdb(&[WRITE_10]), &[]), Err(INVALID_OPCODE));
    }
}
