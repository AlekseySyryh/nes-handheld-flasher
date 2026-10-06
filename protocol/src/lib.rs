//! Wire protocol between the flasher (host) and the RP2040 payload. See `docs/PROTOCOL.md`.
//!
//! The device is stateless: every request is self-contained and idempotent, so the host
//! recovers from any error (bad CRC, timeout, lost bytes) by simply repeating the request.
#![no_std]

use crc::{CRC_32_ISO_HDLC, Crc};

pub const MAGIC: [u8; 2] = [0xB0, 0x07];
pub const VERSION: u8 = 1;

/// USB identifiers of the payload's CDC-ACM port (lets the flasher find the right port).
pub const USB_VID: u16 = 0x2E8A;
pub const USB_PID: u16 = 0x000A;

/// Size of the flash chip dumped by the payload.
pub const FLASH_SIZE: u32 = 2 * 1024 * 1024;
/// Transfer block size (one flash sector).
pub const BLOCK_SIZE: usize = 4096;
pub const BLOCK_COUNT: u32 = FLASH_SIZE / BLOCK_SIZE as u32;

/// Request: magic(2) cmd(1) arg(4) crc32(4).
pub const REQUEST_LEN: usize = 11;
/// Response header: magic(2) kind(1) len(2); followed by `len` payload bytes and crc32(4).
pub const RESPONSE_HEADER_LEN: usize = 5;
pub const RESPONSE_OVERHEAD: usize = RESPONSE_HEADER_LEN + 4;
/// Payload of a `Block` response: index(4) + data.
pub const BLOCK_PAYLOAD_LEN: usize = 4 + BLOCK_SIZE;
/// Largest possible response frame.
pub const MAX_RESPONSE_LEN: usize = RESPONSE_OVERHEAD + BLOCK_PAYLOAD_LEN;
/// Payload of an `Info` response: version(1) flash_size(4) block_size(4).
pub const INFO_PAYLOAD_LEN: usize = 9;

/// CRC-32/ISO-HDLC (the common zlib/PNG/Ethernet CRC-32).
pub const CRC32: Crc<u32> = Crc::<u32>::new(&CRC_32_ISO_HDLC);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Command {
    /// Returns `Info`. `arg` is ignored.
    Hello = 0x01,
    /// Returns `Block` for block number `arg`.
    GetBlock = 0x02,
    /// Returns `Ok`, then resets the chip into the firmware stored in flash. `arg` is ignored.
    Reboot = 0x03,
}

impl Command {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0x01 => Self::Hello,
            0x02 => Self::GetBlock,
            0x03 => Self::Reboot,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Info = 0x81,
    Block = 0x82,
    Ok = 0x83,
    Error = 0xFF,
}

impl Kind {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0x81 => Self::Info,
            0x82 => Self::Block,
            0x83 => Self::Ok,
            0xFF => Self::Error,
            _ => return None,
        })
    }
}

/// Payload of an `Error` response (1 byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ErrorCode {
    UnknownCommand = 1,
    BlockOutOfRange = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub command: Command,
    pub arg: u32,
}

impl Request {
    pub fn encode(&self) -> [u8; REQUEST_LEN] {
        let mut b = [0u8; REQUEST_LEN];
        b[..2].copy_from_slice(&MAGIC);
        b[2] = self.command as u8;
        b[3..7].copy_from_slice(&self.arg.to_le_bytes());
        let crc = CRC32.checksum(&b[2..7]);
        b[7..].copy_from_slice(&crc.to_le_bytes());
        b
    }
}

/// What the device parser found in the incoming byte stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parsed {
    Request(Request),
    /// A frame with valid magic and CRC but an unknown command byte.
    UnknownCommand,
}

/// Resynchronising request parser: feed it bytes one by one. Garbage and corrupted frames are
/// skipped by sliding a window over the stream until magic and CRC match.
#[derive(Default)]
pub struct RequestParser {
    buf: [u8; REQUEST_LEN],
    len: usize,
}

impl RequestParser {
    pub const fn new() -> Self {
        Self { buf: [0; REQUEST_LEN], len: 0 }
    }

    pub fn push(&mut self, byte: u8) -> Option<Parsed> {
        if self.len == REQUEST_LEN {
            self.buf.copy_within(1.., 0);
            self.len -= 1;
        }
        self.buf[self.len] = byte;
        self.len += 1;
        if self.len < REQUEST_LEN || self.buf[..2] != MAGIC {
            return None;
        }
        let crc = u32::from_le_bytes(self.buf[7..].try_into().unwrap());
        if crc != CRC32.checksum(&self.buf[2..7]) {
            return None;
        }
        self.len = 0;
        let arg = u32::from_le_bytes(self.buf[3..7].try_into().unwrap());
        Some(match Command::from_u8(self.buf[2]) {
            Some(command) => Parsed::Request(Request { command, arg }),
            None => Parsed::UnknownCommand,
        })
    }
}

/// Builds a response frame in `out` and returns its total length.
/// `parts` are concatenated to form the payload.
pub fn encode_response(out: &mut [u8], kind: Kind, parts: &[&[u8]]) -> usize {
    let len: usize = parts.iter().map(|p| p.len()).sum();
    out[..2].copy_from_slice(&MAGIC);
    out[2] = kind as u8;
    out[3..5].copy_from_slice(&(len as u16).to_le_bytes());
    let mut pos = RESPONSE_HEADER_LEN;
    for p in parts {
        out[pos..pos + p.len()].copy_from_slice(p);
        pos += p.len();
    }
    let crc = CRC32.checksum(&out[2..pos]);
    out[pos..pos + 4].copy_from_slice(&crc.to_le_bytes());
    pos + 4
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// Not enough bytes yet.
    Incomplete,
    BadMagic,
    BadCrc,
    UnknownKind,
}

/// Parses one response frame from the start of `buf`; returns the kind, the payload and the
/// number of bytes consumed. Used by the host.
pub fn parse_response(buf: &[u8]) -> Result<(Kind, &[u8], usize), ParseError> {
    if buf.len() < RESPONSE_HEADER_LEN {
        return Err(ParseError::Incomplete);
    }
    if buf[..2] != MAGIC {
        return Err(ParseError::BadMagic);
    }
    let len = u16::from_le_bytes([buf[3], buf[4]]) as usize;
    let total = RESPONSE_OVERHEAD + len;
    if buf.len() < total {
        return Err(ParseError::Incomplete);
    }
    let crc = u32::from_le_bytes(buf[total - 4..total].try_into().unwrap());
    if crc != CRC32.checksum(&buf[2..total - 4]) {
        return Err(ParseError::BadCrc);
    }
    let kind = Kind::from_u8(buf[2]).ok_or(ParseError::UnknownKind)?;
    Ok((kind, &buf[RESPONSE_HEADER_LEN..total - 4], total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip_with_garbage_prefix() {
        let req = Request { command: Command::GetBlock, arg: 42 };
        let mut p = RequestParser::new();
        let mut out = None;
        for b in [0xB0, 0xB0, 0x07, 0x00, 0x13].into_iter().chain(req.encode()) {
            out = out.or(p.push(b));
        }
        assert_eq!(out, Some(Parsed::Request(req)));
    }

    #[test]
    fn corrupted_request_is_dropped_then_next_is_accepted() {
        let req = Request { command: Command::Hello, arg: 0 };
        let mut bad = req.encode();
        bad[4] ^= 1;
        let mut p = RequestParser::new();
        assert!(bad.into_iter().all(|b| p.push(b).is_none()));
        assert_eq!(req.encode().into_iter().filter_map(|b| p.push(b)).next(), Some(Parsed::Request(req)));
    }

    #[test]
    fn unknown_command_is_reported() {
        let mut b = Request { command: Command::Hello, arg: 0 }.encode();
        b[2] = 0x7E;
        let crc = CRC32.checksum(&b[2..7]);
        b[7..].copy_from_slice(&crc.to_le_bytes());
        let mut p = RequestParser::new();
        assert_eq!(b.into_iter().filter_map(|x| p.push(x)).next(), Some(Parsed::UnknownCommand));
    }

    #[test]
    fn block_response_roundtrip_and_corruption() {
        let data = [0xA5u8; BLOCK_SIZE];
        let mut out = [0u8; MAX_RESPONSE_LEN];
        let n = encode_response(&mut out, Kind::Block, &[&7u32.to_le_bytes(), &data]);
        assert_eq!(n, MAX_RESPONSE_LEN);
        let (kind, payload, used) = parse_response(&out[..n]).unwrap();
        assert_eq!((kind, used, payload.len()), (Kind::Block, n, BLOCK_PAYLOAD_LEN));
        assert_eq!(&payload[..4], &7u32.to_le_bytes());
        assert_eq!(parse_response(&out[..n - 1]), Err(ParseError::Incomplete));
        out[100] ^= 0x10;
        assert_eq!(parse_response(&out[..n]), Err(ParseError::BadCrc));
    }

    #[test]
    fn crc32_check_value() {
        assert_eq!(CRC32.checksum(b"123456789"), 0xCBF4_3926);
    }
}
