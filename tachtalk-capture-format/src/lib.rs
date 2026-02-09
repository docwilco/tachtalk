//! Binary capture file format for TachTalk OBD2 traffic recording.
//!
//! This crate defines the `.ttcap` file format used by the TachTalk test
//! firmware to record raw TCP traffic between an OBD2 client and an ELM327
//! dongle.
//!
//! The file format consists of a 64-byte header followed by variable-length
//! binary records. Each record contains a timestamp, type tag, length, and
//! payload data.
//!
//! # File Layout
//!
//! ```text
//! [Header: 64 bytes]
//! [Record 0: 7 + data_len bytes]
//! [Record 1: 7 + data_len bytes]
//! ...
//! ```
//!
//! # Record Format
//!
//! Each record is:
//! - `timestamp_ms`: u32 LE — milliseconds since capture start
//! - `record_type`: u8 — see [`RecordType`]
//! - `data_len`: u16 LE — length of the following data
//! - `data`: `[u8; data_len]`


/// Capture file magic bytes: `TachTalk` (8 bytes).
pub const MAGIC: &[u8; 8] = b"TachTalk";

/// Current capture file format version.
pub const VERSION: u16 = 1;

/// Fixed header size in bytes.
pub const HEADER_SIZE: usize = 64;

/// Minimum size of a record (header only, no data): 4 + 1 + 2 = 7 bytes.
pub const RECORD_HEADER_SIZE: usize = 7;

/// Maximum firmware version string length (including null terminator).
pub const FIRMWARE_VERSION_MAX_LEN: usize = 16;

/// Reserved field size in bytes.
pub const RESERVED_SIZE: usize = 12;

/// Header flag: capture buffer overflowed.
pub const FLAG_OVERFLOW: u16 = 1 << 0;

/// Header flag: capture start timestamp is NTP-synced.
pub const FLAG_NTP_SYNCED: u16 = 1 << 1;

/// Capture record types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordType {
    /// Data sent from client to dongle.
    ClientToDongle = 0,
    /// Data sent from dongle to client.
    DongleToClient = 1,
    /// Client connected event (no data payload).
    Connect = 2,
    /// Client disconnected event (no data payload).
    Disconnect = 3,
}

impl RecordType {
    /// Try to convert a raw `u8` to a `RecordType`.
    #[must_use]
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::ClientToDongle),
            1 => Some(Self::DongleToClient),
            2 => Some(Self::Connect),
            3 => Some(Self::Disconnect),
            _ => None,
        }
    }

    /// Human-readable label for this record type.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::ClientToDongle => "TX",
            Self::DongleToClient => "RX",
            Self::Connect => "CONNECT",
            Self::Disconnect => "DISCONNECT",
        }
    }
}

/// 64-byte capture file header.
///
/// All multi-byte integers are little-endian except `dongle_ip` which is in
/// network byte order (big-endian, matching `Ipv4Addr::octets()`).
///
/// | Offset | Size | Field |
/// |--------|------|-------|
/// | 0  | 8  | Magic: `TachTalk` |
/// | 8  | 2  | Version (u16 LE) |
/// | 10 | 2  | Header size (u16 LE) |
/// | 12 | 4  | Record count (u32 LE) |
/// | 16 | 4  | Total data length (u32 LE) |
/// | 20 | 8  | Capture start (u64 LE, Unix epoch ms or 0) |
/// | 28 | 4  | Dongle IP (u32, network order) |
/// | 32 | 2  | Dongle port (u16 LE) |
/// | 34 | 2  | Flags (u16 LE) |
/// | 36 | 16 | Firmware version (null-terminated UTF-8) |
/// | 52 | 12 | Reserved (zero) |
#[derive(Debug, Clone)]
pub struct CaptureHeader {
    /// Format version.
    pub version: u16,
    /// Header size (allows future expansion).
    pub header_size: u16,
    /// Number of records in the file.
    pub record_count: u32,
    /// Total byte length of all record data (excludes header).
    pub data_length: u32,
    /// Capture start time as Unix epoch milliseconds, or 0 if unavailable.
    pub capture_start_ms: u64,
    /// Dongle `IPv4` address octets (network order).
    pub dongle_ip: [u8; 4],
    /// Dongle TCP port.
    pub dongle_port: u16,
    /// Flags (see `FLAG_OVERFLOW`, `FLAG_NTP_SYNCED`).
    pub flags: u16,
    /// Firmware version string (null-terminated, max 15 chars + null).
    pub firmware_version: [u8; FIRMWARE_VERSION_MAX_LEN],
}

impl Default for CaptureHeader {
    #[allow(clippy::cast_possible_truncation)] // HEADER_SIZE is 64, fits in u16
    fn default() -> Self {
        Self {
            version: VERSION,
            header_size: HEADER_SIZE as u16,
            record_count: 0,
            data_length: 0,
            capture_start_ms: 0,
            dongle_ip: [0; 4],
            dongle_port: 0,
            flags: 0,
            firmware_version: [0; FIRMWARE_VERSION_MAX_LEN],
        }
    }
}

impl CaptureHeader {
    /// Set the firmware version string (truncated to 15 chars).
    pub fn set_firmware_version(&mut self, version: &str) {
        self.firmware_version = [0; FIRMWARE_VERSION_MAX_LEN];
        let bytes = version.as_bytes();
        let copy_len = bytes.len().min(FIRMWARE_VERSION_MAX_LEN - 1);
        self.firmware_version[..copy_len].copy_from_slice(&bytes[..copy_len]);
    }

    /// Get the firmware version as a string slice.
    #[must_use]
    pub fn firmware_version_str(&self) -> &str {
        let end = self
            .firmware_version
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(FIRMWARE_VERSION_MAX_LEN);
        // Safety: firmware version is always written from valid UTF-8
        core::str::from_utf8(&self.firmware_version[..end]).unwrap_or("<invalid>")
    }

    /// Returns `true` if the overflow flag is set.
    #[must_use]
    pub fn overflow(&self) -> bool {
        self.flags & FLAG_OVERFLOW != 0
    }

    /// Returns `true` if the NTP-synced flag is set.
    #[must_use]
    pub fn ntp_synced(&self) -> bool {
        self.flags & FLAG_NTP_SYNCED != 0
    }

    /// Serialize the header to a 64-byte array.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; HEADER_SIZE] {
        let mut buf = [0u8; HEADER_SIZE];

        buf[0..8].copy_from_slice(MAGIC);
        buf[8..10].copy_from_slice(&self.version.to_le_bytes());
        buf[10..12].copy_from_slice(&self.header_size.to_le_bytes());
        buf[12..16].copy_from_slice(&self.record_count.to_le_bytes());
        buf[16..20].copy_from_slice(&self.data_length.to_le_bytes());
        buf[20..28].copy_from_slice(&self.capture_start_ms.to_le_bytes());
        buf[28..32].copy_from_slice(&self.dongle_ip);
        buf[32..34].copy_from_slice(&self.dongle_port.to_le_bytes());
        buf[34..36].copy_from_slice(&self.flags.to_le_bytes());
        buf[36..36 + FIRMWARE_VERSION_MAX_LEN].copy_from_slice(&self.firmware_version);
        // buf[52..64] reserved, already zero

        buf
    }

    /// Parse a capture header from a reader.
    ///
    /// Reads exactly [`HEADER_SIZE`] bytes. Returns `Ok(None)` on immediate
    /// EOF (0 bytes read), `Ok(Some(header))` on success.
    ///
    /// # Errors
    ///
    /// Returns `io::ErrorKind::UnexpectedEof` if the header is truncated,
    /// or `io::ErrorKind::InvalidData` if the magic bytes don't match.
    pub fn from_reader(reader: &mut impl std::io::Read) -> std::io::Result<Option<Self>> {
        let mut buf = [0u8; HEADER_SIZE];
        // Detect clean EOF vs truncated header
        match reader.read(&mut buf[..1])? {
            0 => return Ok(None),
            1 => {}
            _ => unreachable!(),
        }
        std::io::Read::read_exact(reader, &mut buf[1..])?;

        if &buf[0..8] != MAGIC {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid capture header magic",
            ));
        }

        let version = u16::from_le_bytes([buf[8], buf[9]]);
        let header_size = u16::from_le_bytes([buf[10], buf[11]]);
        let record_count = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
        let data_length = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
        let capture_start_ms = u64::from_le_bytes([
            buf[20], buf[21], buf[22], buf[23], buf[24], buf[25], buf[26], buf[27],
        ]);

        let mut dongle_ip = [0u8; 4];
        dongle_ip.copy_from_slice(&buf[28..32]);

        let dongle_port = u16::from_le_bytes([buf[32], buf[33]]);
        let flags = u16::from_le_bytes([buf[34], buf[35]]);

        let mut firmware_version = [0u8; FIRMWARE_VERSION_MAX_LEN];
        firmware_version.copy_from_slice(&buf[36..36 + FIRMWARE_VERSION_MAX_LEN]);

        Ok(Some(Self {
            version,
            header_size,
            record_count,
            data_length,
            capture_start_ms,
            dongle_ip,
            dongle_port,
            flags,
            firmware_version,
        }))
    }
}



/// A parsed capture record.
#[derive(Debug, Clone)]
pub struct CaptureRecord {
    /// Milliseconds since capture start.
    pub timestamp_ms: u32,
    /// Record type.
    pub record_type: RecordType,
    /// Payload data.
    pub data: Vec<u8>,
}

/// Iterator over capture records from a reader.
///
/// Reads record data (everything after the file header) from an `impl Read`.
/// After yielding an error, subsequent behavior depends on the reader state
/// (mirroring `std::io::Bytes` semantics).
pub struct RecordIter<R> {
    reader: R,
    offset: u64,
}

impl<R: std::io::Read> RecordIter<R> {
    /// Create a new record iterator over the given reader.
    ///
    /// The reader should be positioned at the start of the record data
    /// (i.e., immediately after the 64-byte file header).
    pub fn new(reader: R) -> Self {
        Self { reader, offset: 0 }
    }

    /// Returns the number of bytes consumed so far.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }
}

impl<R: std::io::Read> Iterator for RecordIter<R> {
    type Item = Result<CaptureRecord, RecordError>;

    fn next(&mut self) -> Option<Self::Item> {
        // Try reading the first byte to detect clean EOF at record boundary
        let mut header = [0u8; RECORD_HEADER_SIZE];
        match self.reader.read(&mut header[..1]) {
            Ok(0) => return None,
            Ok(1) => {}
            Ok(_) => unreachable!(),
            Err(e) => return Some(Err(RecordError::Io(e))),
        }

        // Read remaining header bytes
        if let Err(e) = std::io::Read::read_exact(&mut self.reader, &mut header[1..]) {
            return Some(Err(RecordError::Io(e)));
        }

        let timestamp_ms = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let type_byte = header[4];
        let data_len = u16::from_le_bytes([header[5], header[6]]) as usize;

        let Some(record_type) = RecordType::from_u8(type_byte) else {
            return Some(Err(RecordError::InvalidType {
                offset: self.offset,
                type_byte,
            }));
        };

        let mut data = vec![0u8; data_len];
        if let Err(e) = std::io::Read::read_exact(&mut self.reader, &mut data) {
            return Some(Err(RecordError::Io(e)));
        }

        self.offset += (RECORD_HEADER_SIZE + data_len) as u64;

        Some(Ok(CaptureRecord {
            timestamp_ms,
            record_type,
            data,
        }))
    }
}

/// Errors that can occur while parsing capture records.
#[derive(Debug)]
pub enum RecordError {
    /// An I/O error from the underlying reader.
    ///
    /// `ErrorKind::UnexpectedEof` indicates a truncated record.
    Io(std::io::Error),
    /// Invalid record type byte.
    InvalidType {
        /// Byte offset where the error occurred.
        offset: u64,
        /// The invalid type byte value.
        type_byte: u8,
    },
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::InvalidType { offset, type_byte } => {
                write!(
                    f,
                    "invalid record type 0x{type_byte:02x} at offset {offset}"
                )
            }
        }
    }
}

impl std::error::Error for RecordError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::InvalidType { .. } => None,
        }
    }
}

impl From<std::io::Error> for RecordError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let mut header = CaptureHeader {
            record_count: 42,
            data_length: 1234,
            dongle_ip: [192, 168, 1, 100],
            dongle_port: 35000,
            flags: FLAG_OVERFLOW,
            ..CaptureHeader::default()
        };
        header.set_firmware_version("0.1.0");

        let bytes = header.to_bytes();
        let parsed = CaptureHeader::from_reader(&mut std::io::Cursor::new(&bytes))
            .expect("should not fail")
            .expect("should parse");

        assert_eq!(parsed.version, VERSION);
        let expected_header_size = u16::try_from(HEADER_SIZE).expect("HEADER_SIZE fits in u16");
        assert_eq!(parsed.header_size, expected_header_size);
        assert_eq!(parsed.record_count, 42);
        assert_eq!(parsed.data_length, 1234);
        assert_eq!(parsed.dongle_ip, [192, 168, 1, 100]);
        assert_eq!(parsed.dongle_port, 35000);
        assert!(parsed.overflow());
        assert!(!parsed.ntp_synced());
        assert_eq!(parsed.firmware_version_str(), "0.1.0");
    }

    #[test]
    fn bad_magic_rejected() {
        let mut bytes = [0u8; HEADER_SIZE];
        bytes[0..8].copy_from_slice(b"NotValid");
        let result = CaptureHeader::from_reader(&mut std::io::Cursor::new(&bytes));
        assert!(result.is_err());
    }

    #[test]
    fn too_short_rejected() {
        let bytes = [0u8; 32];
        let result = CaptureHeader::from_reader(&mut std::io::Cursor::new(&bytes));
        assert!(result.is_err());
    }

    #[test]
    fn record_iter_basic() {
        let mut data = Vec::new();
        // Record: timestamp=100ms, type=TX, len=3, data="ATZ"
        data.extend_from_slice(&100u32.to_le_bytes());
        data.push(RecordType::ClientToDongle as u8);
        data.extend_from_slice(&3u16.to_le_bytes());
        data.extend_from_slice(b"ATZ");

        // Record: timestamp=150ms, type=RX, len=5, data="ELM\r>"
        data.extend_from_slice(&150u32.to_le_bytes());
        data.push(RecordType::DongleToClient as u8);
        data.extend_from_slice(&5u16.to_le_bytes());
        data.extend_from_slice(b"ELM\r>");

        let records: Vec<_> = RecordIter::new(data.as_slice())
            .collect::<Result<Vec<_>, _>>()
            .expect("should parse");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].timestamp_ms, 100);
        assert_eq!(records[0].record_type, RecordType::ClientToDongle);
        assert_eq!(records[0].data, b"ATZ".as_slice());
        assert_eq!(records[1].timestamp_ms, 150);
        assert_eq!(records[1].record_type, RecordType::DongleToClient);
        assert_eq!(records[1].data, b"ELM\r>".as_slice());
    }

    #[test]
    fn record_iter_connect_disconnect() {
        let mut data = Vec::new();
        // Connect event: no data
        data.extend_from_slice(&0u32.to_le_bytes());
        data.push(RecordType::Connect as u8);
        data.extend_from_slice(&0u16.to_le_bytes());

        // Disconnect event: no data
        data.extend_from_slice(&5000u32.to_le_bytes());
        data.push(RecordType::Disconnect as u8);
        data.extend_from_slice(&0u16.to_le_bytes());

        let records: Vec<_> = RecordIter::new(data.as_slice())
            .collect::<Result<Vec<_>, _>>()
            .expect("should parse");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].record_type, RecordType::Connect);
        assert!(records[0].data.is_empty());
        assert_eq!(records[1].record_type, RecordType::Disconnect);
        assert_eq!(records[1].timestamp_ms, 5000);
    }

    #[test]
    fn record_iter_truncated_header() {
        let data = [0u8; 5]; // less than RECORD_HEADER_SIZE
        let result: Result<Vec<_>, _> = RecordIter::new(data.as_slice()).collect();
        let err = result.unwrap_err();
        match err {
            RecordError::Io(ref e) => {
                assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof);
            }
            RecordError::InvalidType { .. } => panic!("expected IO error, got: {err:?}"),
        }
    }
}
