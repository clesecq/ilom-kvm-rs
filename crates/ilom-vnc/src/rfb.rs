//! RFB (VNC) wire format, RFC 6143: pixel formats, client messages and the
//! Raw and ZRLE encodings.

use std::io::{self, Read, Write};

use anyhow::{Result, bail};
use flate2::{Compress, Compression, FlushCompress};

pub const ENCODING_RAW: i32 = 0;
pub const ENCODING_ZRLE: i32 = 16;
pub const ENCODING_CURSOR: i32 = -239;
pub const ENCODING_DESKTOP_SIZE: i32 = -223;
pub const ENCODING_QEMU_EXTENDED_KEY: i32 = -258;

pub const SECURITY_NONE: u8 = 1;
pub const SECURITY_VNC_AUTH: u8 = 2;

/// How the client wants pixels encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelFormat {
    pub bits_per_pixel: u8,
    pub depth: u8,
    pub big_endian: bool,
    pub true_colour: bool,
    pub red_max: u16,
    pub green_max: u16,
    pub blue_max: u16,
    pub red_shift: u8,
    pub green_shift: u8,
    pub blue_shift: u8,
}

impl PixelFormat {
    /// Server default: 32-bit little-endian with red in the lowest byte, so
    /// RGBA frames go out unchanged.
    pub const RGBX: Self = Self {
        bits_per_pixel: 32,
        depth: 24,
        big_endian: false,
        true_colour: true,
        red_max: 255,
        green_max: 255,
        blue_max: 255,
        red_shift: 0,
        green_shift: 8,
        blue_shift: 16,
    };

    pub fn to_bytes(self) -> [u8; 16] {
        let mut out = [0_u8; 16];
        out[0] = self.bits_per_pixel;
        out[1] = self.depth;
        out[2] = self.big_endian as u8;
        out[3] = self.true_colour as u8;
        out[4..6].copy_from_slice(&self.red_max.to_be_bytes());
        out[6..8].copy_from_slice(&self.green_max.to_be_bytes());
        out[8..10].copy_from_slice(&self.blue_max.to_be_bytes());
        out[10] = self.red_shift;
        out[11] = self.green_shift;
        out[12] = self.blue_shift;
        out
    }

    pub fn from_bytes(bytes: &[u8; 16]) -> Self {
        let u16_at = |offset: usize| u16::from_be_bytes([bytes[offset], bytes[offset + 1]]);
        Self {
            bits_per_pixel: bytes[0],
            depth: bytes[1],
            big_endian: bytes[2] != 0,
            true_colour: bytes[3] != 0,
            red_max: u16_at(4),
            green_max: u16_at(6),
            blue_max: u16_at(8),
            red_shift: bytes[10],
            green_shift: bytes[11],
            blue_shift: bytes[12],
        }
    }

    /// Only true-colour formats of 8, 16 or 32 bits are served; colour maps
    /// are not.
    pub fn check(&self) -> Result<()> {
        if !self.true_colour {
            bail!("colour-map pixel formats are not supported");
        }
        if !matches!(self.bits_per_pixel, 8 | 16 | 32) {
            bail!("unsupported pixel size of {} bits", self.bits_per_pixel);
        }
        let fits = |max: u16, shift: u8| {
            u32::from(shift) + (16 - max.leading_zeros()) <= u32::from(self.bits_per_pixel)
        };
        if !fits(self.red_max, self.red_shift)
            || !fits(self.green_max, self.green_shift)
            || !fits(self.blue_max, self.blue_shift)
        {
            bail!("colour fields do not fit the pixel size");
        }
        Ok(())
    }

    fn bytes_per_pixel(&self) -> usize {
        usize::from(self.bits_per_pixel / 8)
    }

    fn pixel(&self, rgb: &[u8]) -> u32 {
        let scale = |value: u8, max: u16| (u32::from(value) * u32::from(max) + 127) / 255;
        (scale(rgb[0], self.red_max) << self.red_shift)
            | (scale(rgb[1], self.green_max) << self.green_shift)
            | (scale(rgb[2], self.blue_max) << self.blue_shift)
    }

    fn write_pixel(&self, value: u32, len: usize, out: &mut Vec<u8>) {
        let bytes = if self.big_endian {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        };
        match (len, self.big_endian) {
            (4, _) => out.extend_from_slice(&bytes),
            (3, false) => out.extend_from_slice(&bytes[..3]),
            (3, true) => out.extend_from_slice(&bytes[1..]),
            (2, false) => out.extend_from_slice(&bytes[..2]),
            (2, true) => out.extend_from_slice(&bytes[2..]),
            _ => out.push(value as u8),
        }
    }

    /// ZRLE packs 32-bit pixels whose colours fit in three bytes into three
    /// bytes: `(length, shift applied to the pixel value first)`.
    fn compact_pixel(&self) -> (usize, u32) {
        if self.bits_per_pixel != 32 || self.depth > 24 {
            return (self.bytes_per_pixel(), 0);
        }
        let top = |max: u16, shift: u8| u32::from(shift) + (16 - max.leading_zeros());
        let highest = top(self.red_max, self.red_shift)
            .max(top(self.green_max, self.green_shift))
            .max(top(self.blue_max, self.blue_shift));
        let lowest = self.red_shift.min(self.green_shift).min(self.blue_shift);
        if highest <= 24 {
            (3, 0)
        } else if lowest >= 8 {
            (3, 8)
        } else {
            (4, 0)
        }
    }

    /// Appends RGBA pixels in this format.
    pub fn encode(&self, rgba: &[u8], out: &mut Vec<u8>) {
        if *self == Self::RGBX {
            out.extend_from_slice(rgba);
            return;
        }
        let len = self.bytes_per_pixel();
        out.reserve(rgba.len() / 4 * len);
        for pixel in rgba.as_chunks::<4>().0 {
            self.write_pixel(self.pixel(pixel), len, out);
        }
    }
}

/// Client-to-server messages this server handles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMessage {
    SetPixelFormat(PixelFormat),
    SetEncodings(Vec<i32>),
    UpdateRequest {
        incremental: bool,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    },
    Key {
        down: bool,
        keysym: u32,
    },
    Pointer {
        buttons: u8,
        x: u16,
        y: u16,
    },
    CutText(Vec<u8>),
    /// QEMU extended key event: keysym plus XT scancode.
    ExtendedKey {
        down: bool,
        keysym: u32,
        code: u32,
    },
}

/// Largest clipboard text accepted from a client.
const MAX_CUT_TEXT: usize = 1 << 20;

fn read_array<const N: usize>(reader: &mut impl Read) -> io::Result<[u8; N]> {
    let mut buffer = [0_u8; N];
    reader.read_exact(&mut buffer)?;
    Ok(buffer)
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([bytes[offset], bytes[offset + 1]])
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

/// Reads one client message; errors on unknown message types, which cannot
/// be skipped because their length is unknown.
pub fn read_client_message(reader: &mut impl Read) -> Result<ClientMessage> {
    let [kind] = read_array::<1>(reader)?;
    Ok(match kind {
        0 => {
            let bytes = read_array::<19>(reader)?;
            ClientMessage::SetPixelFormat(PixelFormat::from_bytes(bytes[3..].try_into().unwrap()))
        }
        2 => {
            let header = read_array::<3>(reader)?;
            let count = usize::from(u16_at(&header, 1));
            let mut list = vec![0_u8; count * 4];
            reader.read_exact(&mut list)?;
            ClientMessage::SetEncodings(
                list.as_chunks::<4>()
                    .0
                    .iter()
                    .map(|&bytes| i32::from_be_bytes(bytes))
                    .collect(),
            )
        }
        3 => {
            let bytes = read_array::<9>(reader)?;
            ClientMessage::UpdateRequest {
                incremental: bytes[0] != 0,
                x: u16_at(&bytes, 1),
                y: u16_at(&bytes, 3),
                width: u16_at(&bytes, 5),
                height: u16_at(&bytes, 7),
            }
        }
        4 => {
            let bytes = read_array::<7>(reader)?;
            ClientMessage::Key {
                down: bytes[0] != 0,
                keysym: u32_at(&bytes, 3),
            }
        }
        5 => {
            let bytes = read_array::<5>(reader)?;
            ClientMessage::Pointer {
                buttons: bytes[0],
                x: u16_at(&bytes, 1),
                y: u16_at(&bytes, 3),
            }
        }
        6 => {
            let bytes = read_array::<7>(reader)?;
            let length = u32_at(&bytes, 3) as usize;
            if length > MAX_CUT_TEXT {
                bail!("clipboard text of {length} bytes is too large");
            }
            let mut text = vec![0_u8; length];
            reader.read_exact(&mut text)?;
            ClientMessage::CutText(text)
        }
        255 => {
            let [subtype] = read_array::<1>(reader)?;
            if subtype != 0 {
                bail!("unsupported QEMU client message {subtype}");
            }
            let bytes = read_array::<10>(reader)?;
            ClientMessage::ExtendedKey {
                down: u16_at(&bytes, 0) != 0,
                keysym: u32_at(&bytes, 2),
                code: u32_at(&bytes, 6),
            }
        }
        other => bail!("unsupported client message type {other}"),
    })
}

/// A rectangle of a framebuffer, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

/// Builds one FramebufferUpdate message.
pub struct Update {
    buffer: Vec<u8>,
    rects: u16,
}

impl Default for Update {
    fn default() -> Self {
        Self::new()
    }
}

impl Update {
    pub fn new() -> Self {
        Self {
            buffer: vec![0, 0, 0, 0],
            rects: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rects == 0
    }

    /// Rectangle header; the caller appends the encoded data.
    pub fn header(&mut self, rect: Rect, encoding: i32) -> &mut Vec<u8> {
        self.rects += 1;
        for value in [rect.x, rect.y, rect.width, rect.height] {
            self.buffer.extend_from_slice(&value.to_be_bytes());
        }
        self.buffer.extend_from_slice(&encoding.to_be_bytes());
        &mut self.buffer
    }

    pub fn send(mut self, writer: &mut impl Write) -> io::Result<()> {
        self.buffer[2..4].copy_from_slice(&self.rects.to_be_bytes());
        writer.write_all(&self.buffer)?;
        writer.flush()
    }
}

/// Copies the rows of `rect` out of an RGBA framebuffer `stride` pixels wide.
pub fn crop(rgba: &[u8], stride: usize, rect: Rect) -> Vec<u8> {
    let (x, width) = (usize::from(rect.x), usize::from(rect.width));
    let mut out = Vec::with_capacity(width * usize::from(rect.height) * 4);
    for row in usize::from(rect.y)..usize::from(rect.y) + usize::from(rect.height) {
        let start = (row * stride + x) * 4;
        out.extend_from_slice(&rgba[start..start + width * 4]);
    }
    out
}

/// ZRLE encoder. One zlib stream spans the whole connection, as required.
pub struct Zrle {
    stream: Compress,
    tiles: Vec<u8>,
    compressed: Vec<u8>,
}

const ZRLE_TILE: usize = 64;

impl Default for Zrle {
    fn default() -> Self {
        Self::new()
    }
}

impl Zrle {
    pub fn new() -> Self {
        Self {
            stream: Compress::new(Compression::fast(), true),
            tiles: Vec::new(),
            compressed: Vec::new(),
        }
    }

    /// Appends the ZRLE data of `rgba` (a `width`×`height` block) to `out`.
    pub fn encode(
        &mut self,
        format: &PixelFormat,
        rgba: &[u8],
        width: usize,
        height: usize,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let (len, shift) = format.compact_pixel();
        self.tiles.clear();
        let mut values = Vec::with_capacity(ZRLE_TILE * ZRLE_TILE);
        for tile_y in (0..height).step_by(ZRLE_TILE) {
            let tile_height = ZRLE_TILE.min(height - tile_y);
            for tile_x in (0..width).step_by(ZRLE_TILE) {
                let tile_width = ZRLE_TILE.min(width - tile_x);
                values.clear();
                for row in tile_y..tile_y + tile_height {
                    let start = (row * width + tile_x) * 4;
                    values.extend(
                        rgba[start..start + tile_width * 4]
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(|pixel| format.pixel(pixel) >> shift),
                    );
                }
                self.tile(format, len, &values, tile_width);
            }
        }
        self.compressed.clear();
        let mut input: &[u8] = &self.tiles;
        // A sync flush is complete once all input is consumed and the output
        // buffer still has room.
        loop {
            if self.compressed.len() == self.compressed.capacity() {
                self.compressed.reserve(64 * 1024);
            }
            let before = self.stream.total_in();
            self.stream
                .compress_vec(input, &mut self.compressed, FlushCompress::Sync)?;
            input = &input[(self.stream.total_in() - before) as usize..];
            if input.is_empty() && self.compressed.len() < self.compressed.capacity() {
                break;
            }
        }
        out.extend_from_slice(&(self.compressed.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.compressed);
        Ok(())
    }

    fn tile(&mut self, format: &PixelFormat, len: usize, values: &[u32], width: usize) {
        let mut palette: Vec<u32> = Vec::with_capacity(16);
        for &value in values {
            if !palette.contains(&value) {
                if palette.len() == 16 {
                    palette.clear();
                    break;
                }
                palette.push(value);
            }
        }
        let out = &mut self.tiles;
        match palette.len() {
            1 => {
                out.push(1);
                format.write_pixel(palette[0], len, out);
            }
            2..=16 => {
                out.push(palette.len() as u8);
                for &colour in &palette {
                    format.write_pixel(colour, len, out);
                }
                let bits = match palette.len() {
                    2 => 1,
                    3 | 4 => 2,
                    _ => 4,
                };
                for row in values.chunks(width) {
                    let (mut byte, mut used) = (0_u8, 0);
                    for value in row {
                        let index = palette.iter().position(|colour| colour == value).unwrap();
                        byte |= (index as u8) << (8 - bits - used);
                        used += bits;
                        if used == 8 {
                            out.push(byte);
                            (byte, used) = (0, 0);
                        }
                    }
                    if used > 0 {
                        out.push(byte);
                    }
                }
            }
            _ => {
                out.push(0);
                for &value in values {
                    format.write_pixel(value, len, out);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use flate2::read::ZlibDecoder;

    use super::*;

    const BGR565: PixelFormat = PixelFormat {
        bits_per_pixel: 16,
        depth: 16,
        big_endian: false,
        true_colour: true,
        red_max: 31,
        green_max: 63,
        blue_max: 31,
        red_shift: 11,
        green_shift: 5,
        blue_shift: 0,
    };

    #[test]
    fn default_format_passes_rgba_through() {
        let mut out = Vec::new();
        PixelFormat::RGBX.encode(&[1, 2, 3, 4], &mut out);
        assert_eq!(out, [1, 2, 3, 4]);
        assert_eq!(
            PixelFormat::from_bytes(&PixelFormat::RGBX.to_bytes()),
            PixelFormat::RGBX
        );
    }

    #[test]
    fn pixels_are_scaled_and_shifted() {
        let mut out = Vec::new();
        BGR565.encode(&[255, 0, 0, 0, 0, 0, 255, 0], &mut out);
        assert_eq!(out, [0x00, 0xf8, 0x1f, 0x00]);
        let bgrx_big_endian = PixelFormat {
            big_endian: true,
            red_shift: 16,
            green_shift: 8,
            blue_shift: 0,
            ..PixelFormat::RGBX
        };
        out.clear();
        bgrx_big_endian.encode(&[1, 2, 3, 0], &mut out);
        assert_eq!(out, [0, 1, 2, 3]);
        assert!(BGR565.check().is_ok());
        assert!(
            PixelFormat {
                true_colour: false,
                ..BGR565
            }
            .check()
            .is_err()
        );
    }

    #[test]
    fn client_messages_parse() {
        let mut bytes: &[u8] = &[3, 1, 0, 0, 0, 0, 0, 8, 0, 4];
        assert_eq!(
            read_client_message(&mut bytes).unwrap(),
            ClientMessage::UpdateRequest {
                incremental: true,
                x: 0,
                y: 0,
                width: 8,
                height: 4
            }
        );
        let mut bytes: &[u8] = &[2, 0, 0, 2, 0, 0, 0, 16, 0xff, 0xff, 0xfe, 0xfe];
        assert_eq!(
            read_client_message(&mut bytes).unwrap(),
            ClientMessage::SetEncodings(vec![ENCODING_ZRLE, ENCODING_QEMU_EXTENDED_KEY])
        );
        let mut bytes: &[u8] = &[255, 0, 0, 1, 0, 0, 0, 0x61, 0, 0, 0, 0x1e];
        assert_eq!(
            read_client_message(&mut bytes).unwrap(),
            ClientMessage::ExtendedKey {
                down: true,
                keysym: 0x61,
                code: 0x1e
            }
        );
        let mut bytes: &[u8] = &[7];
        assert!(read_client_message(&mut bytes).is_err());
    }

    /// Inflates the ZRLE data of consecutive rectangles with one stream.
    fn inflate(chunks: &[Vec<u8>]) -> Vec<u8> {
        let mut stream = Vec::new();
        for chunk in chunks {
            let length = u32::from_be_bytes(chunk[..4].try_into().unwrap()) as usize;
            assert_eq!(length, chunk.len() - 4);
            stream.extend_from_slice(&chunk[4..]);
        }
        let mut out = Vec::new();
        // A sync flush leaves the stream open; read what is there.
        let _ = ZlibDecoder::new(stream.as_slice()).read_to_end(&mut out);
        out
    }

    #[test]
    fn zrle_tiles_are_solid_packed_or_raw() {
        let mut zrle = Zrle::new();
        // 66×1: a full 64-pixel tile and a 2-pixel tile.
        let mut rgba = vec![0_u8; 66 * 4];
        rgba[64 * 4..].fill(255);
        rgba[65 * 4] = 0;
        let mut solid_and_packed = Vec::new();
        zrle.encode(&PixelFormat::RGBX, &rgba, 66, 1, &mut solid_and_packed)
            .unwrap();
        // 17 colours in one tile: raw.
        let raw: Vec<u8> = (0..17_u8).flat_map(|i| [i, 0, 0, 0]).collect();
        let mut raw_rect = Vec::new();
        zrle.encode(&PixelFormat::RGBX, &raw, 17, 1, &mut raw_rect)
            .unwrap();
        let data = inflate(&[solid_and_packed, raw_rect]);
        let mut expected = vec![1, 0, 0, 0]; // solid black, 3-byte CPIXEL
        expected.extend_from_slice(&[2, 255, 255, 255, 0, 255, 255, 0b0100_0000]);
        expected.push(0);
        expected.extend((0..17_u8).flat_map(|i| [i, 0, 0]));
        assert_eq!(data, expected);
    }
}
