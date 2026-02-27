//! OBD2 proxy with proactive PID caching.
//!
//! Architecture:
//! - Dongle task: owns the TCP connection, polls PIDs based on fast/slow queues
//! - Cache manager task: receives responses, updates per-client caches, manages PID priority
//! - Proxy server: accepts client connections, reads from cache
//! - AT commands (ATE0, ATZ, etc.) are handled locally per connection using `tachtalk_elm327`

use anyhow::{Context, Result};
use indexmap::IndexSet;
use log::{debug, error, info, warn};
use smallvec::SmallVec;
use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tachtalk_capture_format::{
    CaptureHeader, RecordType, FLAG_OVERFLOW, HEADER_SIZE, RECORD_HEADER_SIZE,
};
use tachtalk_elm327_lib::ClientState;

use crate::config::{PidDataLengths, SlowPollMode, MODE01_PID_DATA_LENGTHS};
use crate::rpm_leds::RpmTaskMessage;
use crate::watchdog::WatchdogHandle;
use crate::State;

const RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// Maintenance interval for checking promotion/demotion/removal
const MAINTENANCE_INTERVAL: Duration = Duration::from_millis(2000);
/// RPM PID (always polled).
const RPM_PID: u8 = 0x0C;

/// Type alias for small OBD2 wire-format buffers (ASCII hex commands/responses).
/// 20 bytes on 32-bit; 16 inline bytes covers multi-PID commands without heap.
type Obd2Buffer = SmallVec<[u8; 16]>;

/// OBD2 PID byte. Mode is always 0x01 for this application.
type Pid = u8;

/// Data bytes for a single ECU response (excluding mode/PID prefix).
/// Max 8 bytes inline covers all Mode 01 PIDs (longest is 6 bytes).
/// Size: 12 bytes on 32-bit.
type PidData = SmallVec<[u8; 8]>;

/// Cached OBD2 response: one data entry per ECU. Most PIDs get exactly 1.
type CachedResponse = SmallVec<[PidData; 1]>;

/// A group of PIDs sharing the same learned ECU count, for multi-PID batching.
type PidGroup = SmallVec<[Pid; 8]>;

// ============================================================================
// PID/Wire Conversion Helpers
// ============================================================================

/// Convert a PID to wire-format Mode 01 command (e.g., `0x0C` → `b"010C"`).
fn pid_to_wire_command(pid: Pid) -> Obd2Buffer {
    // Always 4 bytes ("01XX"), fits inline in SmallVec<[u8; 16]>
    let mut buf = Obd2Buffer::new();
    write!(buf, "01{pid:02X}").unwrap();
    buf
}

/// Build a wire-format multi-PID command (e.g., `[0x0C, 0x0D]` → `b"010C0D 1"`).
///
/// Appends the ECU `response_count` suffix so the adapter knows how many
/// responses to wait for. All PIDs in a batch must share the same ECU count
/// (callers group by count before calling this).
fn build_multi_pid_wire_command(pids: &[Pid], response_count: u8) -> Obd2Buffer {
    // Maximum 6 PIDs: "01" + 6×"XX" + " N" = 16 bytes, fits inline
    // in SmallVec<[u8; 16]>
    let mut cmd = Obd2Buffer::new();
    cmd.extend_from_slice(b"01");
    for &pid in pids {
        write!(cmd, "{pid:02X}").unwrap();
    }
    write!(cmd, " {response_count}").unwrap();
    cmd
}

/// Parse a wire-format command (e.g., `b"010C"`) to PID byte.
/// Returns `None` for invalid/non-OBD commands or non-Mode-01 commands.
fn wire_command_to_pid(cmd: &[u8]) -> Option<Pid> {
    // Filter out AT commands
    if cmd.starts_with(b"AT") || cmd.starts_with(b"at") {
        return None;
    }
    // Need at least 4 hex chars for mode + PID
    if cmd.len() < 4 {
        return None;
    }
    // Parse first 2 bytes (4 hex chars) as mode + PID
    let hex_str = std::str::from_utf8(&cmd[..4]).ok()?;
    let mode = u8::from_str_radix(&hex_str[..2], 16).ok()?;
    // Only accept Mode 01
    if mode != 0x01 {
        return None;
    }
    let pid = u8::from_str_radix(&hex_str[2..4], 16).ok()?;
    Some(pid)
}

/// Format a cached response back to wire format for a client.
///
/// Produces the format clients expect: `{le}{line1}{le}{line2}...{le}>`
/// where each line is `41{pid}{data hex}` formatted per client settings.
fn format_cached_for_client(
    pid: Pid,
    values: &CachedResponse,
    client_state: &ClientState,
) -> Obd2Buffer {
    let le = client_state.line_ending();
    let mut result = Obd2Buffer::new();

    for data in values {
        result.extend_from_slice(le.as_bytes());
        // Build hex response "41{pid:02X}{data hex}" into a temp buffer,
        // then write_response formats it (adding spaces) directly into result.
        let mut hex = Obd2Buffer::new();
        write!(hex, "41{pid:02X}").unwrap();
        for &b in data.as_slice() {
            write!(hex, "{b:02X}").unwrap();
        }
        client_state.write_response(&hex, &mut result).unwrap();
    }
    result.extend_from_slice(le.as_bytes());
    result.push(b'>');
    result
}

/// Cached supported-PID query responses plus a readiness flag.
///
/// The `ready` flag is `false` while connecting or after a disconnect, and
/// `true` once all 8 supported-PID queries have been attempted.
#[derive(Default)]
pub struct SupportedPidsCache {
    pub entries: [Option<Obd2Buffer>; 8],
    pub ready: bool,
}

/// Supported PID query commands (0100, 0120, 0140, 0160, 0180, 01A0, 01C0, 01E0)
const SUPPORTED_PID_QUERIES: [&[u8]; 8] = [
    b"0100", b"0120", b"0140", b"0160", b"0180", b"01A0", b"01C0", b"01E0",
];

/// Firmware version string for capture header
const FIRMWARE_VERSION: &str = env!("GIT_VERSION");

// ============================================================================
// Capture Types and Helpers
// ============================================================================

/// Record an event to the shared capture buffer in `State`.
///
/// Acquires the capture buffer lock briefly to append a single record.
fn record_event(
    state: &State,
    elapsed: Duration,
    record_type: RecordType,
    data: &[u8],
    capture_buffer_size: u32,
) {
    let record_size = RECORD_HEADER_SIZE + data.len();

    let mut buf_guard = state.capture_buffer.lock().unwrap();

    // Check for overflow - stop capturing when buffer is full
    if buf_guard.len() + record_size > capture_buffer_size as usize {
        if !state
            .polling_metrics
            .capture_overflow
            .load(Ordering::Relaxed)
        {
            warn!("Capture buffer full, stopping capture");
            state
                .polling_metrics
                .capture_overflow
                .store(true, Ordering::Relaxed);
        }
        return;
    }

    // Write record
    // Hot path: wraps after ~49 days, acceptable for relative timestamps
    #[allow(clippy::cast_possible_truncation)]
    let timestamp_ms = elapsed.as_millis() as u32;
    buf_guard.extend_from_slice(&timestamp_ms.to_le_bytes());
    buf_guard.push(record_type as u8);
    // Hot path: data comes from 1024-byte TCP read buffers, always fits u16
    #[allow(clippy::cast_possible_truncation)]
    let len = data.len() as u16;
    buf_guard.extend_from_slice(&len.to_le_bytes());
    buf_guard.extend_from_slice(data);

    state
        .polling_metrics
        .records_captured
        .fetch_add(1, Ordering::Relaxed);
}

/// Build a [`CaptureHeader`] from the current device state.
pub fn build_capture_header(state: &State, buffer_len: u32) -> [u8; HEADER_SIZE] {
    let record_count = state
        .polling_metrics
        .records_captured
        .load(Ordering::Relaxed);

    let (dongle_ip, dongle_port) = {
        let cfg_guard = state.config.lock().unwrap();
        let ip: std::net::Ipv4Addr = cfg_guard
            .obd2
            .dongle_ip
            .parse()
            .unwrap_or(std::net::Ipv4Addr::UNSPECIFIED);
        (ip.octets(), cfg_guard.obd2.dongle_port)
    };

    let flags = if state
        .polling_metrics
        .capture_overflow
        .load(Ordering::Relaxed)
    {
        FLAG_OVERFLOW
    } else {
        0
    };

    let mut header = CaptureHeader {
        record_count,
        data_length: buffer_len,
        dongle_ip,
        dongle_port,
        flags,
        ..CaptureHeader::default()
    };

    header.set_firmware_version(FIRMWARE_VERSION);

    header.to_bytes()
}

/// Check if a command is a supported PIDs query, returns index (0-7) if so
fn supported_pids_index(cmd: &[u8]) -> Option<usize> {
    SUPPORTED_PID_QUERIES
        .iter()
        .position(|&q| cmd.eq_ignore_ascii_case(q))
}

/// Normalize an OBD2 command for caching and lookup.
///
/// Strips trailing `" 1"` (single response count) and removes all spaces to
/// produce a canonical hex form, since the ELM327 accepts both `"010C"` and
/// `"01 0C"`.
///
/// **Known limitation:** stripping `" 1"` discards the client's response count
/// hint, and commands with other counts (e.g., `" 2"`) are kept as-is. This
/// will be replaced by proper command parsing (see TODO: `ParsedClientCommand`).
fn normalize_obd_command(cmd: &[u8]) -> Obd2Buffer {
    // Strip trailing " 1" first, while the space boundary distinguishes it
    // from hex data (e.g., "01 0C 1" → "01 0C", but "0100" stays "0100").
    let cmd = if cmd.len() >= 2 && cmd.ends_with(b" 1") {
        &cmd[..cmd.len() - 2]
    } else {
        cmd
    };
    // Strip all spaces to get canonical hex form
    cmd.iter().filter(|&&b| b != b' ').copied().collect()
}

// ============================================================================
// Response Parsing (Framing Layer)
// ============================================================================

/// A single parsed response line, optionally with ECU CAN ID.
#[derive(Debug, Clone)]
struct ParsedLine {
    /// Parsed 11-bit CAN ID when framing is enabled (e.g., `0x7E8`).
    ecu_id: Option<u16>,
    /// OBD data bytes (after PCI byte when framing, or raw hex bytes when not).
    data: Obd2Buffer,
}

/// Parse a raw dongle response into structured lines.
///
/// When `framing_enabled` is true (ATH1), expects format like:
/// `7E8 03 41 0C 1A F8\r7E9 03 41 0C 1B 00\r>`
///
/// When `framing_enabled` is false (ATH0), expects format like:
/// `41 0C 1A F8\r41 0C 1B 00\r>`
fn parse_response_framed(response: &[u8], framing_enabled: bool) -> SmallVec<[ParsedLine; 4]> {
    let response_str = String::from_utf8_lossy(response);
    let mut lines = SmallVec::new();

    // ELM327 with ATL0 uses bare '\r' as line terminator.
    // str::lines() only splits on '\n' and '\r\n', not bare '\r'.
    for line in response_str.split('\r') {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.contains('>') {
            continue;
        }

        if framing_enabled {
            // Framing on: expect `{3-char CAN ID}{PCI byte}{OBD data...}`
            // With ATS0 (spaces off), format is `7E80341 0C1AF8`
            // First 3 chars = CAN ID, next 2 = PCI hex byte, rest = data
            let bytes: SmallVec<[u8; 24]> = trimmed.replace(' ', "").into_bytes().into();
            if bytes.len() < 5 {
                // Too short for framing format, return as-is
                lines.push(ParsedLine {
                    ecu_id: None,
                    data: bytes.into_iter().collect(),
                });
                continue;
            }

            let ecu_id = std::str::from_utf8(&bytes[..3])
                .ok()
                .and_then(|s| u16::from_str_radix(s, 16).ok())
                .unwrap_or(0);

            // Parse PCI byte (2 hex chars after CAN ID) - indicates data length
            let pci_str = std::str::from_utf8(&bytes[3..5]).unwrap_or("00");
            let pci_byte = u8::from_str_radix(pci_str, 16).unwrap_or(0);

            // Remaining data after PCI (hex pairs)
            let data_hex = &bytes[5..];
            let actual_data_bytes = data_hex.len() / 2;

            if actual_data_bytes != usize::from(pci_byte) {
                warn!(
                    "PCI byte mismatch: PCI={pci_byte}, actual data bytes={actual_data_bytes}, line={trimmed}"
                );
            }

            // Parse the data hex bytes
            let mut data: Obd2Buffer = Obd2Buffer::new();
            let mut i = 0;
            while i + 1 < data_hex.len() {
                if let Ok(b) =
                    u8::from_str_radix(std::str::from_utf8(&data_hex[i..i + 2]).unwrap_or("00"), 16)
                {
                    data.push(b);
                }
                i += 2;
            }

            lines.push(ParsedLine {
                ecu_id: Some(ecu_id),
                data,
            });
        } else {
            // Framing off: raw hex line, parse bytes
            let hex_str = trimmed.replace(' ', "");
            let mut data: Obd2Buffer = Obd2Buffer::new();
            let hex_bytes = hex_str.as_bytes();
            let mut i = 0;
            while i + 1 < hex_bytes.len() {
                if let Ok(b) = u8::from_str_radix(
                    std::str::from_utf8(&hex_bytes[i..i + 2]).unwrap_or("00"),
                    16,
                ) {
                    data.push(b);
                }
                i += 2;
            }
            lines.push(ParsedLine { ecu_id: None, data });
        }
    }

    lines
}

/// Look up PID data length from a (possibly runtime-updated) lookup table.
#[inline]
fn resolve_pid_data_length(pid: u8, pid_lengths: &PidDataLengths) -> Option<u8> {
    pid_lengths[pid as usize]
}

/// Count ECUs per PID from parsed response lines.
///
/// Returns a map of PID byte -> ECU count. Works for both single and multi-PID
/// queries by parsing the full response data using `resolve_pid_data_length()` to
/// determine byte boundaries.
///
/// For multi-PID responses like `41 0C 0C B8 05 62`, this parses:
/// - 0x41 = mode 01 response
/// - 0x0C = PID (RPM), followed by 2 data bytes
/// - 0x05 = PID (coolant), followed by 1 data byte
///
/// When framing is enabled, also verifies unique CAN IDs per PID.
fn count_ecus_per_pid(
    parsed: &[ParsedLine],
    framing_enabled: bool,
    pid_lengths: &PidDataLengths,
) -> HashMap<u8, u8> {
    let mut counts: HashMap<u8, u8> = HashMap::new();

    if framing_enabled {
        // Group by (CAN ID, PID) to count unique ECUs per PID
        let mut seen_per_pid: HashMap<u8, SmallVec<[u16; 4]>> = HashMap::new();
        for line in parsed {
            if let Some(ecu_id) = line.ecu_id {
                for pid in extract_pids_from_response(&line.data, pid_lengths) {
                    let ecus = seen_per_pid.entry(pid).or_default();
                    if !ecus.contains(&ecu_id) {
                        ecus.push(ecu_id);
                    }
                }
            }
        }
        for (pid, ecus) in seen_per_pid {
            #[allow(clippy::cast_possible_truncation)]
            let count = ecus.len() as u8;
            counts.insert(pid, count);
        }
    } else {
        // Count PID occurrences across response lines (no deduplication without CAN IDs)
        for line in parsed {
            for pid in extract_pids_from_response(&line.data, pid_lengths) {
                *counts.entry(pid).or_insert(0) += 1;
            }
        }
    }

    counts
}

/// Extract all PIDs from a Mode 01 response data buffer.
///
/// Response format: `41 <PID1> <DATA1...> <PID2> <DATA2...> ...`
/// Uses `resolve_pid_data_length()` (with runtime-learned overrides) to determine how
/// many data bytes follow each PID.
fn extract_pids_from_response(data: &[u8], pid_lengths: &PidDataLengths) -> PidGroup {
    let mut pids = PidGroup::new();

    // Need at least service byte + PID
    if data.len() < 2 || data[0] != 0x41 {
        return pids;
    }

    let mut i = 1; // Skip service byte (0x41)
    while i < data.len() {
        let pid = data[i];
        pids.push(pid);

        // Skip PID byte + data bytes for this PID
        let Some(data_len) = resolve_pid_data_length(pid, pid_lengths) else {
            // Single-PID response for an unknown-length PID: the one PID is already
            // pushed, so breaking is correct.  Multi-PID responses should never
            // contain unknown-length PIDs (partition_pids_by_ecu_count filters them).
            debug_assert!(
                i == 1,
                "Unknown PID length for {pid:#04X} at offset {i} — \
                 multi-PID response contains unlearned PID"
            );
            break;
        };
        i += 1 + usize::from(data_len);
    }

    pids
}

/// Parse a raw dongle response into cache entries grouped by PID.
///
/// The dongle returns responses like `b"410C1AF8\r410C1B00\r\r>"`.
/// This splits on `\r`, parses each ECU response, and groups data by PID.
/// For multi-PID responses like `410C0CB805627D`, splits into separate entries.
///
/// Returns a map of PID to `CachedResponse` (data per ECU).
/// Only parses Mode 01 (0x41) responses.
fn parse_response_to_cache(
    raw: &Obd2Buffer,
    pid_lengths: &PidDataLengths,
) -> HashMap<Pid, CachedResponse> {
    let mut result: HashMap<Pid, CachedResponse> = HashMap::new();

    for line in raw.split(|&b| b == b'\r') {
        if line.is_empty() || line == b">" {
            continue;
        }

        // Parse hex string to bytes
        let hex_str: String = line
            .iter()
            .filter(|b| b.is_ascii_hexdigit())
            .map(|&b| b as char)
            .collect();
        let bytes: SmallVec<[u8; 12]> = hex_str
            .as_bytes()
            .chunks(2)
            .filter_map(|chunk| {
                if chunk.len() == 2 {
                    u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()
                } else {
                    None
                }
            })
            .collect();

        // Need at least mode + PID
        if bytes.len() < 2 {
            continue;
        }

        // Only accept Mode 01 responses (0x41)
        if bytes[0] != 0x41 {
            continue;
        }

        // Extract PIDs and their data
        let mut i = 1;
        while i < bytes.len() {
            let pid = bytes[i];
            let Some(data_len) = resolve_pid_data_length(pid, pid_lengths) else {
                let data: PidData = bytes[i + 1..].iter().copied().collect();
                result.entry(pid).or_default().push(data);
                break;
            };
            let data_len = usize::from(data_len);

            // Extract data bytes for this PID
            let data_end = (i + 1 + data_len).min(bytes.len());
            let data: PidData = bytes[i + 1..data_end].iter().copied().collect();

            // Add to result (append if multiple ECUs responded)
            result.entry(pid).or_default().push(data);

            // Move to next PID
            i += 1 + data_len;
        }
    }

    result
}

// ============================================================================
// Cache Types
// ============================================================================

/// Entry in a per-client PID cache
pub enum CacheEntry {
    /// Fresh value ready to be consumed (one buffer per ECU response)
    Fresh(CachedResponse),
    /// No value available (consumed or not yet received)
    Empty,
    /// Client is waiting for this PID
    Waiting(oneshot::Sender<()>),
}

/// Per-client cache of PID values
pub struct ClientCache {
    entries: HashMap<Pid, CacheEntry>,
}

impl ClientCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }
}

/// Unique identifier for a client (monotonic counter; `u32` is sufficient for embedded use).
type ClientId = u32;

/// Priority level for PID polling
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidPriority {
    Fast,
    Slow,
}

/// Tracking info for a PID
struct PidInfo {
    /// Last time any client had to wait for this PID
    last_waiter_time: Option<Instant>,
    /// Last time any client consumed this PID
    last_consumed_time: Option<Instant>,
    /// Current priority
    priority: PidPriority,
    /// Request count since last maintenance (for rate logging)
    request_count: u32,
}

// ============================================================================
// Channel Types
// ============================================================================

/// Message to cache manager (from dongle task and client handlers)
pub enum CacheManagerMessage {
    /// Parsed dongle response, grouped by PID (may contain multiple PIDs from multi-PID queries).
    /// Only sent on successful parse — errors are handled in the dongle task.
    DongleResponse {
        responses: HashMap<Pid, CachedResponse>,
    },
    /// Client registered a waiter for this PID (promote to fast)
    Waiting(Pid),
    /// Client consumed this PID (None = cache hit, Some = cache miss with wait duration)
    Consumed {
        pid: Pid,
        wait_duration: Option<Duration>,
    },
    /// Register a new client, returns their cache
    RegisterClient(oneshot::Sender<(ClientId, Arc<Mutex<ClientCache>>)>),
    /// Unregister a client
    UnregisterClient(ClientId),
}

/// Sender for cache manager messages
pub type CacheManagerSender = Sender<CacheManagerMessage>;

// ============================================================================
// Dongle Types
// ============================================================================

/// Request to the dongle task (for polling control)
pub enum DongleMessage {
    /// Add or update a PID's priority
    SetPidPriority(Pid, PidPriority),
    /// Remove a PID from polling
    RemovePid(Pid),
}

pub type DongleSender = Sender<DongleMessage>;
pub type DongleReceiver = Receiver<DongleMessage>;

/// Tri-state for dongle/ECU feature probing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeatureSupport {
    /// Not yet tested.
    Unknown,
    /// Confirmed supported.
    Yes,
    /// Confirmed unsupported.
    No,
}

/// Errors from the dongle task
#[derive(Debug, Clone)]
pub enum DongleError {
    NotConnected,
    Timeout,
    Disconnected,
    IoError(String),
}

impl std::fmt::Display for DongleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConnected => write!(f, "Not connected to dongle"),
            Self::Timeout => write!(f, "Dongle timeout"),
            Self::Disconnected => write!(f, "Dongle disconnected"),
            Self::IoError(e) => write!(f, "IO error: {e}"),
        }
    }
}

impl std::error::Error for DongleError {}

impl DongleError {
    /// Convert to ELM327-style error message
    pub fn to_elm327_error(&self) -> &'static str {
        match self {
            Self::NotConnected => "UNABLE TO CONNECT",
            Self::Timeout => "NO DATA",
            Self::Disconnected => "CAN ERROR",
            Self::IoError(_) => "BUS ERROR",
        }
    }
}

// ============================================================================
// Dongle Connection State
// ============================================================================

/// State for the dongle connection
struct DongleState {
    stream: TcpStream,
    /// Reference to shared state for capture recording
    state: Arc<State>,
    /// Capture buffer size snapshot from config
    capture_buffer_size: u32,
    /// Capture start time (for relative timestamps)
    capture_start: Instant,
    /// Whether the dongle supports the repeat command
    supports_repeat: FeatureSupport,
    /// The repeat command to send (e.g. b"1" or empty for bare CR).
    /// Only meaningful when `supports_repeat != No`.
    repeat_cmd: Obd2Buffer,
    /// Last wire command sent to the dongle (includes response count suffix if learned)
    last_command: Option<Obd2Buffer>,
    /// Learned ECU counts per PID (how many ECUs respond to each PID).
    /// On first request for a PID, we send without a count to learn it.
    /// Subsequent requests append the count for faster dongle response.
    response_counts: HashMap<Pid, u8>,
    /// PID → data-byte-count lookup table.
    /// Initialized from the static SAE J1979 table; updated at runtime when
    /// single-PID query responses reveal lengths for vendor-specific PIDs.
    /// Reset to the static table on reconnect.
    pid_lengths: PidDataLengths,
    /// Whether the ECU supports multi-PID queries.
    /// Set to `Yes` after a validated multi-PID response, `No` on failure.
    /// Reset on reconnect.
    supports_multi_pid: FeatureSupport,
    /// Whether framing (ATH1) is enabled for CAN header parsing
    framing_enabled: bool,
}

impl DongleState {
    /// Build the wire command for a PID, appending the learned response count if known.
    fn build_wire_command(&self, pid: Pid) -> Obd2Buffer {
        let mut cmd = pid_to_wire_command(pid);
        if let Some(&count) = self.response_counts.get(&pid) {
            write!(cmd, " {count}").unwrap();
        }
        cmd
    }

    /// Send a command to the dongle without waiting for response.
    /// Records to capture buffer if capture is enabled.
    fn send_command(&mut self, command: &[u8]) -> Result<(), DongleError> {
        let mut cmd_with_cr: Obd2Buffer = command.into();
        if !cmd_with_cr.ends_with(b"\r") {
            cmd_with_cr.push(b'\r');
        }

        debug!(
            "Sending to dongle: {:?}",
            String::from_utf8_lossy(&cmd_with_cr)
        );

        // Record to capture buffer
        if self.state.capture_active.load(Ordering::Relaxed) {
            record_event(
                &self.state,
                self.capture_start.elapsed(),
                RecordType::ClientToDongle,
                &cmd_with_cr,
                self.capture_buffer_size,
            );
        }

        self.stream
            .write_all(&cmd_with_cr)
            .map_err(|e| DongleError::IoError(e.to_string()))
    }

    /// Receive response from dongle until `>` prompt.
    /// Records to capture buffer if capture is enabled.
    fn recv_response(&mut self, timeout: Duration) -> Result<Obd2Buffer, DongleError> {
        let mut buffer = [0u8; 128];
        let mut response = Obd2Buffer::new();
        let start = Instant::now();

        loop {
            match self.stream.read(&mut buffer) {
                Ok(0) => return Err(DongleError::Disconnected),
                Ok(n) => {
                    response.extend_from_slice(&buffer[..n]);
                    debug!("Read {} bytes from dongle, total: {}", n, response.len());
                    if response.contains(&b'>') {
                        debug!(
                            "Complete response: {:?}",
                            String::from_utf8_lossy(&response)
                        );
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    warn!("Unexpected WouldBlock on blocking socket - lwIP quirk?");
                    if start.elapsed() > timeout {
                        return Err(DongleError::Timeout);
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    return Err(DongleError::Timeout);
                }
                Err(e) => return Err(DongleError::IoError(e.to_string())),
            }
        }

        // Record to capture buffer
        if self.state.capture_active.load(Ordering::Relaxed) {
            record_event(
                &self.state,
                self.capture_start.elapsed(),
                RecordType::DongleToClient,
                &response,
                self.capture_buffer_size,
            );
        }

        Ok(response)
    }

    /// Send a command and receive the response (raw send + receive with capture recording).
    fn raw_execute(
        &mut self,
        command: &[u8],
        timeout: Duration,
    ) -> Result<Obd2Buffer, DongleError> {
        self.send_command(command)?;
        self.recv_response(timeout)
    }

    /// Learn ECU counts from a response, storing per-PID.
    ///
    /// Parses the response to extract per-PID ECU counts and stores them.
    /// Works for both single-PID and multi-PID responses.
    fn learn_response_counts(&mut self, response: &Obd2Buffer) {
        let parsed = parse_response_framed(response, self.framing_enabled);
        let counts = count_ecus_per_pid(&parsed, self.framing_enabled, &self.pid_lengths);

        for (pid, count) in counts {
            if !self.response_counts.contains_key(&pid) && count > 0 {
                info!("Learned ECU count for PID {pid:02X}: {count}");
                self.response_counts.insert(pid, count);
            }
        }
    }

    /// Learn the data byte count for a PID from a single-PID query response.
    ///
    /// For a single-PID query `01XX`, each ECU response line has format:
    /// `41 XX DD DD DD...` — the bytes after service byte + PID are data bytes.
    /// Verifies all ECU responses agree on the length before storing.
    fn learn_pid_data_length(&mut self, pid: Pid, response: &Obd2Buffer) {
        // Skip if already known (static table or previously learned)
        if resolve_pid_data_length(pid, &self.pid_lengths).is_some() {
            return;
        }

        let parsed = parse_response_framed(response, self.framing_enabled);
        let mut learned_len: Option<u8> = None;

        for line in &parsed {
            // Expect [0x41, pid, data...]
            if line.data.len() < 2 || line.data[0] != 0x41 || line.data[1] != pid {
                continue;
            }
            // OBD2 CAN single-frame data is at most 7 bytes (minus service + PID = 5),
            // so this always fits u8.
            #[allow(clippy::cast_possible_truncation)]
            let data_len = (line.data.len() - 2) as u8;
            match learned_len {
                None => learned_len = Some(data_len),
                Some(prev) if prev != data_len => {
                    warn!(
                        "Inconsistent data length for PID {pid:02X}: {prev} vs {data_len}, \
                         not learning"
                    );
                    return;
                }
                _ => {}
            }
        }

        if let Some(len) = learned_len {
            info!("Learned data length for PID {pid:02X}: {len} bytes");
            self.pid_lengths[pid as usize] = Some(len);
        }
    }

    /// Execute a wire command with the ELM327 repeat optimization.
    ///
    /// If `wire_cmd` matches `last_command` and repeat is supported (or
    /// untested), sends the repeat command instead of the full command.
    /// Falls back to a full resend when the dongle replies with `?`.
    ///
    /// Maintains `last_command` and `supports_repeat` state. Callers are
    /// responsible for running any learning logic (ECU counts, data lengths)
    /// on the returned response.
    fn execute(
        &mut self,
        wire_cmd: Obd2Buffer,
        timeout: Duration,
    ) -> Result<Obd2Buffer, DongleError> {
        let can_try_repeat = self.supports_repeat != FeatureSupport::No
            && self.last_command.as_ref() == Some(&wire_cmd);

        if can_try_repeat {
            debug!("Trying repeat command");
            let repeat_result = self.raw_execute(&self.repeat_cmd.clone(), timeout);

            if let Ok(response) = &repeat_result {
                let response_str = String::from_utf8_lossy(response);
                if response_str.contains('?') {
                    // Repeat not supported, mark and resend full command
                    info!("Dongle does not support repeat command");
                    self.supports_repeat = FeatureSupport::No;
                    let result = self.raw_execute(&wire_cmd, timeout);
                    self.last_command = if result.is_ok() { Some(wire_cmd) } else { None };
                    result
                } else {
                    if self.supports_repeat == FeatureSupport::Unknown {
                        info!("Dongle supports repeat command");
                        self.supports_repeat = FeatureSupport::Yes;
                    }
                    repeat_result
                }
            } else {
                self.last_command = None;
                repeat_result
            }
        } else {
            let result = self.raw_execute(&wire_cmd, timeout);
            self.last_command = if result.is_ok() { Some(wire_cmd) } else { None };
            result
        }
    }

    /// Query a single PID from the ECU.
    ///
    /// Builds the wire command (with learned response count if known), sends
    /// via [`execute`], then learns data length and ECU counts from
    /// the response.
    fn query_single_pid(&mut self, pid: Pid, timeout: Duration) -> Result<Obd2Buffer, DongleError> {
        let wire_cmd = self.build_wire_command(pid);
        let result = self.execute(wire_cmd, timeout);
        if let Ok(ref resp) = result {
            self.learn_pid_data_length(pid, resp);
            self.learn_response_counts(resp);
        }
        result
    }

    /// Query multiple PIDs from the ECU in a single command.
    ///
    /// Builds a combined wire command (e.g., `010C0D 1`) and sends via
    /// [`execute`], then learns ECU counts from the response.
    fn query_multiple_pids(
        &mut self,
        pids: &[Pid],
        response_count: u8,
        timeout: Duration,
    ) -> Result<Obd2Buffer, DongleError> {
        let wire_cmd = build_multi_pid_wire_command(pids, response_count);
        let result = self.execute(wire_cmd, timeout);
        if let Ok(ref resp) = result {
            self.learn_response_counts(resp);
        }
        result
    }

    /// Validate multi-PID support from the first multi-PID response.
    ///
    /// Checks whether ≥ 2 of the queried PIDs appear in the response.
    /// Sets `supports_multi_pid` to `Yes` or `No`; no-ops if already known.
    fn validate_multi_pid_support(
        &mut self,
        result: &Result<Obd2Buffer, DongleError>,
        queried_pids: &[Pid],
    ) {
        if self.supports_multi_pid != FeatureSupport::Unknown {
            return;
        }
        if let Ok(raw) = result {
            let parsed = parse_response_to_cache(raw, &self.pid_lengths);
            let matched = queried_pids
                .iter()
                .filter(|pid| parsed.contains_key(pid))
                .count();
            if matched >= 2 {
                info!("ECU supports multi-PID queries ({matched} PIDs in response)");
                self.supports_multi_pid = FeatureSupport::Yes;
            } else {
                info!(
                    "ECU does not support multi-PID queries \
                     (only {matched} PIDs in response)"
                );
                self.supports_multi_pid = FeatureSupport::No;
            }
        } else {
            info!("Multi-PID query failed, assuming not supported");
            self.supports_multi_pid = FeatureSupport::No;
        }
    }

    /// Run the standard ELM327 initialization sequence.
    ///
    /// Sends ATZ reset, then ATE0/ATL0/ATS0/ATSP0, and finally ATH1 or ATH0
    /// depending on `use_framing`.
    fn run_init_commands(
        &mut self,
        timeout: Duration,
        use_framing: bool,
    ) -> Result<(), DongleError> {
        // ATZ needs raw write (does not follow normal echo/prompt pattern after reset)
        debug!("Sending ATZ reset");
        self.stream
            .write_all(b"ATZ\r")
            .map_err(|e| DongleError::IoError(e.to_string()))?;
        std::thread::sleep(Duration::from_millis(500));
        let mut buf = [0u8; 256];
        let _ = self.stream.read(&mut buf);

        // Normal init commands
        self.raw_execute(b"ATE0", timeout)?;
        self.raw_execute(b"ATL0", timeout)?;
        self.raw_execute(b"ATS0", timeout)?;
        self.raw_execute(b"ATSP0", timeout)?;

        // Set framing mode based on config
        if use_framing {
            self.raw_execute(b"ATH1", timeout)?;
            self.framing_enabled = true;
            info!("Framing enabled (ATH1)");
        } else {
            self.raw_execute(b"ATH0", timeout)?;
            self.framing_enabled = false;
        }

        Ok(())
    }
}

// ============================================================================
// Dongle Task
// ============================================================================

/// Polling state for the dongle task
struct PollingState {
    fast_pids: IndexSet<Pid>,
    slow_pids: IndexSet<Pid>,
    fast_index: usize,
    slow_index: usize,
    last_slow_poll: Instant,
    fast_requests_since_slow: u32,
}

impl Default for PollingState {
    fn default() -> Self {
        // Always start with RPM in fast queue
        Self {
            fast_pids: IndexSet::from([RPM_PID]),
            slow_pids: IndexSet::new(),
            fast_index: 0,
            slow_index: 0,
            last_slow_poll: Instant::now(),
            fast_requests_since_slow: 0,
        }
    }
}

impl PollingState {
    fn next_fast_pid(&mut self) -> Option<Pid> {
        let pid = self.fast_pids.get_index(self.fast_index).copied()?;
        self.fast_index = (self.fast_index + 1) % self.fast_pids.len();
        Some(pid)
    }

    fn next_slow_pid(&mut self) -> Option<Pid> {
        let pid = self.slow_pids.get_index(self.slow_index).copied()?;
        self.slow_index = (self.slow_index + 1) % self.slow_pids.len();
        Some(pid)
    }

    fn set_pid_priority(&mut self, pid: Pid, priority: PidPriority) {
        // Remove from both sets first
        self.fast_pids.swap_remove(&pid);
        self.slow_pids.swap_remove(&pid);

        // Add to appropriate set
        match priority {
            PidPriority::Fast => {
                self.fast_pids.insert(pid);
            }
            PidPriority::Slow => {
                self.slow_pids.insert(pid);
            }
        }

        self.fix_indices();
    }

    fn remove_pid(&mut self, pid: Pid) {
        self.fast_pids.swap_remove(&pid);
        self.slow_pids.swap_remove(&pid);
        self.fix_indices();
    }

    fn fix_indices(&mut self) {
        if self.fast_pids.is_empty() {
            self.fast_index = 0;
        } else {
            self.fast_index %= self.fast_pids.len();
        }
        if self.slow_pids.is_empty() {
            self.slow_index = 0;
        } else {
            self.slow_index %= self.slow_pids.len();
        }
    }

    /// Partition active PIDs by learned ECU count for multi-PID batching.
    ///
    /// Returns groups keyed by `Option<u8>` ECU count:
    /// - `Some(count)`: PIDs with known data length and ECU count, batchable in multi-PID queries
    /// - `None`: PIDs needing individual queries to learn their ECU count or data length
    fn partition_pids_by_ecu_count(
        &self,
        dongle: &DongleState,
    ) -> SmallVec<[(Option<u8>, PidGroup); 4]> {
        let mut groups: HashMap<Option<u8>, PidGroup> = HashMap::new();

        for &pid in self.fast_pids.iter().chain(self.slow_pids.iter()) {
            let has_data_len = resolve_pid_data_length(pid, &dongle.pid_lengths).is_some();
            let key = if has_data_len {
                dongle.response_counts.get(&pid).copied()
            } else {
                None
            };
            groups.entry(key).or_default().push(pid);
        }

        groups.into_iter().collect()
    }

    /// Sync PID counts and lists to the shared polling metrics in `State`.
    fn sync_metrics(&self, state: &Arc<State>) {
        state.polling_metrics.fast_pid_count.store(
            self.fast_pids.len().try_into().unwrap_or(u32::MAX),
            Ordering::Relaxed,
        );
        state.polling_metrics.slow_pid_count.store(
            self.slow_pids.len().try_into().unwrap_or(u32::MAX),
            Ordering::Relaxed,
        );
        if let Ok(mut fast) = state.polling_metrics.fast_pids.lock() {
            *fast = self
                .fast_pids
                .iter()
                .map(|k| String::from_utf8_lossy(&pid_to_wire_command(*k)).into_owned())
                .collect();
        }
        if let Ok(mut slow) = state.polling_metrics.slow_pids.lock() {
            *slow = self
                .slow_pids
                .iter()
                .map(|k| String::from_utf8_lossy(&pid_to_wire_command(*k)).into_owned())
                .collect();
        }
    }

    /// Poll PIDs and send results to the cache manager.
    ///
    /// When multi-PID mode is active (config + ECU support confirmed or
    /// untested), batches all known-length PIDs into chunked commands.
    /// Falls back to single-PID polling when multi-PID is disabled or
    /// confirmed unsupported.
    ///
    /// Returns the number of dongle requests made this iteration.
    fn poll_pids(
        &mut self,
        connection: &mut Option<DongleState>,
        state: &Arc<State>,
        timeout: Duration,
    ) -> u32 {
        if connection.is_none() {
            return 0;
        }

        let (
            slow_poll_mode,
            slow_poll_interval,
            slow_poll_ratio,
            use_multi_pid,
            max_pids_per_query,
        ) = {
            let cfg = state.config.lock().unwrap();
            (
                cfg.obd2.slow_poll_mode,
                Duration::from_millis(cfg.obd2.slow_poll_interval_ms),
                cfg.obd2.slow_poll_ratio,
                cfg.obd2.use_multi_pid,
                cfg.obd2.max_pids_per_query,
            )
        };

        // Use multi-PID when enabled and not confirmed unsupported
        let try_multi = use_multi_pid
            && connection
                .as_ref()
                .is_some_and(|ds| ds.supports_multi_pid != FeatureSupport::No);

        if try_multi {
            self.poll_pids_multi(connection, state, timeout, max_pids_per_query)
        } else {
            self.poll_pids_single(
                connection,
                state,
                timeout,
                slow_poll_mode,
                slow_poll_interval,
                slow_poll_ratio,
            )
        }
    }

    /// Single-PID polling: one fast PID round-robin, plus one slow PID when due.
    fn poll_pids_single(
        &mut self,
        connection: &mut Option<DongleState>,
        state: &Arc<State>,
        timeout: Duration,
        slow_poll_mode: SlowPollMode,
        slow_poll_interval: Duration,
        slow_poll_ratio: u32,
    ) -> u32 {
        let Some(dongle_state) = connection.as_mut() else {
            return 0;
        };

        let mut requests = 0;

        // Always try to poll a fast PID
        if let Some(pid) = self.next_fast_pid() {
            let result = dongle_state.query_single_pid(pid, timeout);
            handle_dongle_result(result, connection, state);

            requests += 1;
            self.fast_requests_since_slow += 1;
            state
                .polling_metrics
                .dongle_requests_total
                .fetch_add(1, Ordering::Relaxed);
        } else {
            // RPM is always in fast queue, so we should always poll something
            debug_assert!(
                false,
                "Expected to poll at least RPM, but fast_pids is empty"
            );
        }

        // Poll slow PID based on mode (interval or ratio)
        let should_poll_slow = match slow_poll_mode {
            SlowPollMode::Interval => self.last_slow_poll.elapsed() >= slow_poll_interval,
            SlowPollMode::Ratio => self.fast_requests_since_slow >= slow_poll_ratio,
        };

        if should_poll_slow {
            if let Some(pid) = self.next_slow_pid() {
                // Re-check connection after potential fast poll failure
                if let Some(ref mut dongle_state) = connection {
                    let result = dongle_state.query_single_pid(pid, timeout);
                    handle_dongle_result(result, connection, state);

                    requests += 1;
                    state
                        .polling_metrics
                        .dongle_requests_total
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            self.last_slow_poll = Instant::now();
            self.fast_requests_since_slow = 0;
        }

        requests
    }

    /// Multi-PID polling: batch all PIDs (fast and slow) into chunked commands
    /// of at most `max_pids` PIDs each, grouped by learned ECU count.
    ///
    /// PIDs are grouped by their learned ECU count so each multi-PID command
    /// uses an accurate response count suffix. PIDs whose data length is
    /// unknown are queried individually (one per iteration) so their length
    /// gets learned for future batches.
    ///
    /// On the first multi-PID attempt (`supports_multi_pid == Unknown`), the
    /// response is validated: if it contains data for ≥ 2 of the queried PIDs
    /// the feature is confirmed; otherwise it is marked unsupported and the
    /// caller will fall back to `poll_pids_single` on the next iteration.
    fn poll_pids_multi(
        &mut self,
        connection: &mut Option<DongleState>,
        state: &Arc<State>,
        timeout: Duration,
        max_pids: u8,
    ) -> u32 {
        let Some(dongle_state) = connection.as_mut() else {
            return 0;
        };

        let pid_groups = self.partition_pids_by_ecu_count(dongle_state);

        let mut requests: u32 = 0;
        let chunk_size = usize::from(max_pids.clamp(2, 6));

        for (ecu_count, group) in &pid_groups {
            match ecu_count {
                // Known ECU count: batch into multi-PID commands
                Some(count) => {
                    for chunk in group.chunks(chunk_size) {
                        let Some(ref mut ds) = connection else {
                            break;
                        };

                        if chunk.len() == 1 {
                            let result = ds.query_single_pid(chunk[0], timeout);
                            handle_dongle_result(result, connection, state);
                        } else {
                            let result = ds.query_multiple_pids(chunk, *count, timeout);
                            ds.validate_multi_pid_support(&result, chunk);
                            handle_dongle_result(result, connection, state);

                            // If multi-PID was just rejected, stop sending more chunks —
                            // the next poll_pids call will use single-PID mode.
                            if connection
                                .as_ref()
                                .map_or(true, |ds| ds.supports_multi_pid == FeatureSupport::No)
                            {
                                requests += 1;
                                state
                                    .polling_metrics
                                    .dongle_requests_total
                                    .fetch_add(1, Ordering::Relaxed);
                                return requests;
                            }
                        }

                        requests += 1;
                        state
                            .polling_metrics
                            .dongle_requests_total
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
                // Unknown ECU count or data length: query individually to learn
                None => {
                    for &pid in group {
                        let Some(ref mut ds) = connection else {
                            break;
                        };
                        let result = ds.query_single_pid(pid, timeout);
                        handle_dongle_result(result, connection, state);

                        requests += 1;
                        state
                            .polling_metrics
                            .dongle_requests_total
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }

        requests
    }
}

/// Run the dongle task - owns the connection and polls PIDs
pub fn dongle_task(state: &Arc<State>, control_rx: &DongleReceiver) {
    info!("OBD2 dongle task starting...");

    let mut connection: Option<DongleState> = None;
    let mut last_connect_attempt: Option<Instant> = None;
    let mut polling = PollingState::default();

    // For requests/sec calculation
    let mut requests_this_second: u32 = 0;
    let mut last_rate_update = Instant::now();

    let watchdog = WatchdogHandle::register(c"obd2_dongle");

    info!("OBD2 dongle task started");

    loop {
        watchdog.feed();

        // Get config values
        let (timeout, dongle_ip, dongle_port) = {
            let cfg = state.config.lock().unwrap();
            (
                Duration::from_millis(cfg.obd2_timeout_ms),
                cfg.obd2.dongle_ip.clone(),
                cfg.obd2.dongle_port,
            )
        };

        // Process control messages (non-blocking)
        while let Ok(msg) = control_rx.try_recv() {
            match msg {
                DongleMessage::SetPidPriority(pid, priority) => {
                    polling.set_pid_priority(pid, priority);
                }
                DongleMessage::RemovePid(pid) => {
                    polling.remove_pid(pid);
                }
            }
        }

        // Try to ensure we have a connection (with reconnect delay)
        if connection.is_none() {
            try_reconnect(
                &mut connection,
                &mut last_connect_attempt,
                &dongle_ip,
                dongle_port,
                timeout,
                &watchdog,
                state,
            );
        }

        // Poll PIDs if connected
        if connection.is_some() {
            requests_this_second += polling.poll_pids(&mut connection, state, timeout);
        } else {
            // Not connected, sleep before retry
            std::thread::sleep(Duration::from_millis(100));
        }

        // Update requests/sec counter
        if last_rate_update.elapsed() >= Duration::from_secs(1) {
            state
                .polling_metrics
                .dongle_requests_per_sec
                .store(requests_this_second, Ordering::Relaxed);
            requests_this_second = 0;
            last_rate_update = Instant::now();
        }

        // Update PID counts and lists
        polling.sync_metrics(state);
    }
}

/// Attempt to reconnect to the dongle if the reconnect delay has elapsed.
fn try_reconnect(
    connection: &mut Option<DongleState>,
    last_connect_attempt: &mut Option<Instant>,
    dongle_ip: &str,
    dongle_port: u16,
    timeout: Duration,
    watchdog: &WatchdogHandle,
    state: &Arc<State>,
) {
    let should_try = match *last_connect_attempt {
        Some(t) => t.elapsed() >= RECONNECT_DELAY,
        None => true,
    };
    if should_try {
        *last_connect_attempt = Some(Instant::now());
        if let Some((dongle_state, local_addr, remote_addr)) =
            try_connect(dongle_ip, dongle_port, timeout, watchdog, state)
        {
            *connection = Some(dongle_state);
            state.dongle_connected.store(true, Ordering::Relaxed);
            *state.dongle_tcp_info.lock().unwrap() = Some((local_addr, remote_addr));
        } else {
            state.dongle_connected.store(false, Ordering::Relaxed);
            *state.dongle_tcp_info.lock().unwrap() = None;
            *state.supported_pids.lock().unwrap() = SupportedPidsCache::default();
        }
    }
}

/// Handle a dongle result: parse the response on success, tear down the
/// connection on fatal errors, and forward parsed data to the cache manager.
fn handle_dongle_result(
    result: Result<Obd2Buffer, DongleError>,
    connection: &mut Option<DongleState>,
    state: &Arc<State>,
) {
    match result {
        Ok(raw) => {
            // Connection is guaranteed to be Some when the result is Ok —
            // only fatal errors (Disconnected/IoError) drop it.
            let pid_lengths = &connection.as_ref().unwrap().pid_lengths;
            let responses = parse_response_to_cache(&raw, pid_lengths);
            state
                .cache_manager_tx
                .send(CacheManagerMessage::DongleResponse { responses })
                .expect("cache manager task dead");
        }
        Err(DongleError::Disconnected | DongleError::IoError(_)) => {
            warn!("Dongle connection lost, will reconnect");
            *connection = None;
            state.dongle_connected.store(false, Ordering::Relaxed);
            *state.dongle_tcp_info.lock().unwrap() = None;
            // Clear stale capability data so a reconnect to a different vehicle
            // doesn't serve outdated supported-PID responses.
            *state.supported_pids.lock().unwrap() = SupportedPidsCache::default();
        }
        Err(DongleError::Timeout | DongleError::NotConnected) => {}
    }
}

// ============================================================================
// Cache Manager Task
// ============================================================================

/// Cache hit/miss metrics, reset each maintenance interval.
struct CacheMetrics {
    hits: u32,
    misses: u32,
    total_wait_time: Duration,
}

/// Run periodic maintenance: demote/remove inactive PIDs, log metrics.
fn run_maintenance(
    pid_info: &mut HashMap<Pid, PidInfo>,
    state: &Arc<State>,
    cache_metrics: &mut CacheMetrics,
    elapsed: Duration,
) {
    let (fast_demotion_ms, pid_inactive_removal_ms) = {
        let cfg = state.config.lock().unwrap();
        (
            Duration::from_millis(cfg.obd2.fast_demotion_ms),
            Duration::from_millis(cfg.obd2.pid_inactive_removal_ms),
        )
    };

    let mut pids_to_remove = Vec::new();
    let mut pids_to_demote = Vec::new();

    for (pid, info) in &mut *pid_info {
        // Skip RPM - always fast, never removed
        if *pid == RPM_PID {
            continue;
        }

        // Check removal: no recent consumption AND no recent waiters
        // (need both conditions to handle newly added PIDs that haven't been polled yet)
        let no_recent_consumption = info
            .last_consumed_time
            .map_or(true, |t| t.elapsed() > pid_inactive_removal_ms);
        let no_recent_waiters = info
            .last_waiter_time
            .map_or(true, |t| t.elapsed() > pid_inactive_removal_ms);
        if no_recent_consumption && no_recent_waiters {
            pids_to_remove.push(*pid);
            continue;
        }

        // Check demotion (no waiters recently)
        if info.priority == PidPriority::Fast {
            let should_demote = info
                .last_waiter_time
                .map_or(true, |t| t.elapsed() > fast_demotion_ms);
            if should_demote {
                pids_to_demote.push(*pid);
            }
        }
    }

    // Demote PIDs
    for pid in pids_to_demote {
        if let Some(info) = pid_info.get_mut(&pid) {
            info.priority = PidPriority::Slow;
        }
        state
            .polling_metrics
            .demotions
            .fetch_add(1, Ordering::Relaxed);
        info!("PID {pid:02X}: demoted to slow");
        state
            .dongle_control_tx
            .send(DongleMessage::SetPidPriority(pid, PidPriority::Slow))
            .expect("dongle task dead");
    }

    // Remove inactive PIDs
    for pid in pids_to_remove {
        pid_info.remove(&pid);
        state
            .polling_metrics
            .removals
            .fetch_add(1, Ordering::Relaxed);
        info!("PID {pid:02X}: removed (inactive)");
        state
            .dongle_control_tx
            .send(DongleMessage::RemovePid(pid))
            .expect("dongle task dead");
    }

    // Log client request rates and reset counters
    // Values reset every MAINTENANCE_INTERVAL (500ms), so they stay small enough
    // that u32 -> f32 precision loss is irrelevant
    #[allow(clippy::cast_precision_loss)]
    let elapsed_secs = elapsed.as_secs_f32();
    let mut rates: Vec<_> = pid_info
        .iter_mut()
        .filter(|(_, info)| info.request_count > 0)
        .map(|(pid, info)| {
            #[allow(clippy::cast_precision_loss)]
            let rate = info.request_count as f32 / elapsed_secs;
            let pid_str = String::from_utf8_lossy(&pid_to_wire_command(*pid)).into_owned();
            info.request_count = 0;
            (pid_str, rate)
        })
        .collect();
    rates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    if !rates.is_empty() {
        let rate_strs: Vec<_> = rates.iter().map(|(p, r)| format!("{p}:{r:.1}")).collect();
        info!("Client req/s: {}", rate_strs.join(", "));
    }

    // Log cache hit/miss metrics
    let total_requests = cache_metrics.hits + cache_metrics.misses;
    if total_requests > 0 {
        #[allow(clippy::cast_precision_loss)]
        let hit_rate = 100.0 * cache_metrics.hits as f32 / total_requests as f32;
        #[allow(clippy::cast_precision_loss)]
        let avg_wait_ms = if cache_metrics.misses > 0 {
            cache_metrics.total_wait_time.as_secs_f32() * 1000.0 / cache_metrics.misses as f32
        } else {
            0.0
        };
        info!(
            "Cache: {} hits, {} misses ({hit_rate:.1}% hit rate), avg wait {avg_wait_ms:.1}ms",
            cache_metrics.hits, cache_metrics.misses,
        );
    }
    *cache_metrics = CacheMetrics {
        hits: 0,
        misses: 0,
        total_wait_time: Duration::ZERO,
    };
}

/// Push fresh data to all client caches, waking any that were waiting on this PID.
fn update_client_caches(
    clients: &HashMap<ClientId, Arc<Mutex<ClientCache>>>,
    pid: Pid,
    data: &CachedResponse,
) {
    for client_cache in clients.values() {
        let mut cache_guard = client_cache.lock().unwrap();
        match cache_guard.entries.get_mut(&pid) {
            Some(CacheEntry::Waiting(_)) => {
                // Take the sender, replace with Fresh, then send wake
                let old = std::mem::replace(
                    cache_guard.entries.get_mut(&pid).unwrap(),
                    CacheEntry::Fresh(data.clone()),
                );
                if let CacheEntry::Waiting(tx) = old {
                    let _ = tx.send(());
                }
            }
            Some(entry) => {
                *entry = CacheEntry::Fresh(data.clone());
            }
            None => {
                cache_guard
                    .entries
                    .insert(pid, CacheEntry::Fresh(data.clone()));
            }
        }
    }
}

/// Handle a PID entering the Waiting state: create or promote PID info as appropriate.
fn handle_waiting_pid(pid: Pid, pid_info: &mut HashMap<Pid, PidInfo>, state: &Arc<State>) {
    let slow_poll_mode = {
        let cfg = state.config.lock().unwrap();
        cfg.obd2.slow_poll_mode
    };

    let is_new = !pid_info.contains_key(&pid);
    let info = pid_info.entry(pid).or_insert_with(|| PidInfo {
        last_waiter_time: None,
        last_consumed_time: None,
        priority: PidPriority::Fast,
        request_count: 0,
    });
    info.last_waiter_time = Some(Instant::now());

    // Promote to fast if it was slow (in interval mode, promote immediately)
    // In ratio mode, we don't auto-promote here — we check wait time on Consumed
    if info.priority == PidPriority::Slow && slow_poll_mode == SlowPollMode::Interval {
        info.priority = PidPriority::Fast;
        state
            .polling_metrics
            .promotions
            .fetch_add(1, Ordering::Relaxed);
        log::info!("PID {pid:02X}: promoted to fast");
        state
            .dongle_control_tx
            .send(DongleMessage::SetPidPriority(pid, PidPriority::Fast))
            .expect("dongle task dead");
    } else if is_new {
        log::info!("PID {pid:02X}: added to fast queue");
        state
            .dongle_control_tx
            .send(DongleMessage::SetPidPriority(pid, PidPriority::Fast))
            .expect("dongle task dead");
    }
}

/// Handle a PID being consumed by a client: track metrics and promote slow PIDs if wait time
/// exceeds the configured threshold (in ratio mode).
fn handle_consumed_pid(
    pid: Pid,
    wait_duration: Option<Duration>,
    pid_info: &mut HashMap<Pid, PidInfo>,
    state: &Arc<State>,
    cache_metrics: &mut CacheMetrics,
) {
    let (slow_poll_mode, promotion_threshold) = {
        let cfg = state.config.lock().unwrap();
        (
            cfg.obd2.slow_poll_mode,
            Duration::from_millis(cfg.obd2.promotion_wait_threshold_ms),
        )
    };

    if let Some(info) = pid_info.get_mut(&pid) {
        info.last_consumed_time = Some(Instant::now());
        info.request_count += 1;

        if slow_poll_mode == SlowPollMode::Ratio
            && info.priority == PidPriority::Slow
            && wait_duration.is_some_and(|d| d >= promotion_threshold)
        {
            info.priority = PidPriority::Fast;
            state
                .polling_metrics
                .promotions
                .fetch_add(1, Ordering::Relaxed);
            log::info!(
                "PID {pid:02X}: promoted to fast (wait {}ms >= {}ms threshold)",
                wait_duration.unwrap().as_millis(),
                promotion_threshold.as_millis()
            );
            state
                .dongle_control_tx
                .send(DongleMessage::SetPidPriority(pid, PidPriority::Fast))
                .expect("dongle task dead");
        }
    }
    if let Some(duration) = wait_duration {
        cache_metrics.misses += 1;
        cache_metrics.total_wait_time += duration;
    } else {
        cache_metrics.hits += 1;
    }
}

/// Run the cache manager task
pub fn cache_manager_task(state: &Arc<State>, manager_rx: &Receiver<CacheManagerMessage>) {
    info!("Cache manager task starting...");

    let watchdog = WatchdogHandle::register(c"cache_manager");

    let mut clients: HashMap<ClientId, Arc<Mutex<ClientCache>>> = HashMap::new();
    let mut pid_info: HashMap<Pid, PidInfo> = HashMap::new();
    let mut next_client_id: ClientId = 1;
    let mut last_maintenance = Instant::now();

    let mut cache_metrics = CacheMetrics {
        hits: 0,
        misses: 0,
        total_wait_time: Duration::ZERO,
    };

    // Initialize RPM PID info
    pid_info.insert(
        RPM_PID,
        PidInfo {
            last_waiter_time: None,
            last_consumed_time: Some(Instant::now()),
            priority: PidPriority::Fast,
            request_count: 0,
        },
    );

    info!("Cache manager task started");

    loop {
        watchdog.feed();

        // Process messages with timeout (wakes immediately on message, or after maintenance interval)
        match manager_rx.recv_timeout(MAINTENANCE_INTERVAL) {
            Ok(CacheManagerMessage::DongleResponse { responses }) => {
                // Update all PIDs from the response (may be multi-PID)
                for (pid, data) in &responses {
                    update_client_caches(&clients, *pid, data);

                    // Extract RPM from the first ECU response
                    if *pid == RPM_PID {
                        if let Some(first) = data.first() {
                            // RPM data is 2 bytes: RPM = ((A * 256) + B) / 4
                            if first.len() >= 2 {
                                let rpm = (u32::from(first[0]) * 256 + u32::from(first[1])) / 4;
                                debug!("Cache manager extracted RPM: {rpm}");
                                let _ = state.rpm_tx.send(RpmTaskMessage::Rpm(rpm));
                                *state.shared_rpm.lock().unwrap() = Some(rpm);
                            }
                        }
                    }
                }
            }
            Ok(CacheManagerMessage::Waiting(pid)) => {
                handle_waiting_pid(pid, &mut pid_info, state);
            }
            Ok(CacheManagerMessage::Consumed { pid, wait_duration }) => {
                handle_consumed_pid(pid, wait_duration, &mut pid_info, state, &mut cache_metrics);
            }
            Ok(CacheManagerMessage::RegisterClient(reply_tx)) => {
                let id = next_client_id;
                next_client_id += 1;
                let cache = Arc::new(Mutex::new(ClientCache::new()));
                clients.insert(id, cache.clone());
                let _ = reply_tx.send((id, cache));
                debug!("Registered client {id}");
            }
            Ok(CacheManagerMessage::UnregisterClient(id)) => {
                clients.remove(&id);
                debug!("Unregistered client {id}");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Normal timeout, continue to maintenance
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                error!("Cache manager channel disconnected, exiting");
                break;
            }
        }

        // Periodic maintenance: check promotion/demotion/removal
        if last_maintenance.elapsed() >= MAINTENANCE_INTERVAL {
            run_maintenance(
                &mut pid_info,
                state,
                &mut cache_metrics,
                last_maintenance.elapsed(),
            );
            last_maintenance = Instant::now();
        }
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Try to connect to the dongle and initialize it
fn try_connect(
    dongle_ip: &str,
    dongle_port: u16,
    timeout: Duration,
    watchdog: &WatchdogHandle,
    state: &Arc<State>,
) -> Option<(DongleState, SocketAddr, SocketAddr)> {
    info!(
        "Connecting to OBD2 dongle at {dongle_ip}:{dongle_port} (timeout: {}ms)",
        timeout.as_millis()
    );

    let addr: SocketAddr = format!("{dongle_ip}:{dongle_port}").parse().ok()?;
    let stream = match TcpStream::connect_timeout(&addr, timeout) {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to connect to dongle: {e}");
            return None;
        }
    };

    let local_addr = stream.local_addr().ok()?;
    let remote_addr = stream.peer_addr().ok()?;

    watchdog.feed();

    if let Err(e) = stream.set_read_timeout(Some(timeout)) {
        error!("Failed to set read timeout: {e}");
        return None;
    }
    if let Err(e) = stream.set_write_timeout(Some(timeout)) {
        error!("Failed to set write timeout: {e}");
        return None;
    }

    let capture_start = Instant::now();
    let cfg = state.config.lock().unwrap();
    let capture_buffer_size = cfg.obd2.capture_buffer_size;
    let use_framing = cfg.obd2.use_framing;
    let use_repeat = cfg.obd2.use_repeat;
    let repeat_cmd: Obd2Buffer = cfg.obd2.repeat_string.as_bytes().into();
    drop(cfg);

    // Create DongleState for init and queries
    let mut dongle_state = DongleState {
        stream,
        state: state.clone(),
        capture_buffer_size,
        capture_start,
        supports_repeat: if use_repeat {
            FeatureSupport::Unknown
        } else {
            FeatureSupport::No
        },
        repeat_cmd,
        last_command: None,
        response_counts: HashMap::new(),
        pid_lengths: MODE01_PID_DATA_LENGTHS,
        supports_multi_pid: FeatureSupport::Unknown,
        framing_enabled: false, // Will be set after ATH command
    };

    if let Err(e) = dongle_state.run_init_commands(timeout, use_framing) {
        error!("Dongle init failed: {e}");
        return None;
    }

    info!("Connected to OBD2 dongle, querying supported PIDs...");
    query_supported_pids(&mut dongle_state, state, timeout, watchdog);
    info!("Connected to OBD2 dongle");

    Some((dongle_state, local_addr, remote_addr))
}

/// Query the 8 standard supported-PID ranges and store results in `State`.
fn query_supported_pids(
    dongle: &mut DongleState,
    state: &Arc<State>,
    timeout: Duration,
    watchdog: &WatchdogHandle,
) {
    let mut guard = state.supported_pids.lock().unwrap();
    *guard = SupportedPidsCache::default();
    for (idx, query) in SUPPORTED_PID_QUERIES.iter().enumerate() {
        watchdog.feed();
        match dongle.raw_execute(query, timeout) {
            Ok(response) => {
                debug!(
                    "Supported PIDs query {:?}: {:?}",
                    String::from_utf8_lossy(query),
                    String::from_utf8_lossy(&response)
                );
                guard.entries[idx] = Some(response);
            }
            Err(e) => {
                debug!(
                    "Supported PIDs query {:?} failed: {e}",
                    String::from_utf8_lossy(query)
                );
                guard.entries[idx] = None;
            }
        }
    }
    // All 8 queries attempted — mark capabilities as ready
    guard.ready = true;
}

// ============================================================================
// Proxy Server
// ============================================================================

pub struct Obd2Proxy {
    state: Arc<State>,
}

/// RAII guard to decrement client count and remove TCP info on drop
struct ClientCountGuard<'a> {
    state: &'a State,
    tcp_info: (SocketAddr, SocketAddr),
    client_id: ClientId,
}

impl Drop for ClientCountGuard<'_> {
    fn drop(&mut self) {
        self.state.obd2_client_count.fetch_sub(1, Ordering::Relaxed);
        self.state
            .client_tcp_info
            .lock()
            .unwrap()
            .retain(|&x| x != self.tcp_info);
        let _ = self
            .state
            .cache_manager_tx
            .send(CacheManagerMessage::UnregisterClient(self.client_id));
    }
}

impl Obd2Proxy {
    pub fn new(state: Arc<State>) -> Self {
        Self { state }
    }

    pub fn run(self) -> Result<()> {
        info!("OBD2 proxy starting...");

        let listen_port = self.state.config.lock().unwrap().obd2.listen_port;
        let listener = TcpListener::bind(format!("0.0.0.0:{listen_port}"))?;
        let watchdog = WatchdogHandle::register(c"obd2_proxy");

        listener.set_nonblocking(true)?;

        info!("OBD2 proxy started on port {listen_port}");

        loop {
            watchdog.feed();

            match listener.accept() {
                Ok((stream, _)) => {
                    let state = self.state.clone();

                    crate::thread_util::spawn_named(
                        c"obd2_client",
                        6144,
                        crate::thread_util::StackMemory::SpiRam,
                        move || {
                            if let Err(e) = Self::handle_client(stream, &state) {
                                error!("Error handling client: {e:?}");
                            }
                        },
                    );
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => {
                    error!("Error accepting connection: {e:?}");
                }
            }
        }
    }

    // TcpStream is moved into this function for exclusive ownership of the connection
    #[allow(clippy::needless_pass_by_value)]
    fn handle_client(client_stream: TcpStream, state: &Arc<State>) -> Result<()> {
        let peer = client_stream.peer_addr()?;
        let local = client_stream.local_addr()?;
        info!("OBD2 client connected: {peer}");

        // Register with cache manager
        let (reply_tx, reply_rx) = oneshot::channel();
        state
            .cache_manager_tx
            .send(CacheManagerMessage::RegisterClient(reply_tx))
            .map_err(|_| anyhow::anyhow!("Cache manager channel closed"))?;
        let (client_id, client_cache) = reply_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("Failed to register with cache manager"))?;

        // Track connection info
        let tcp_info = (local, peer);
        state.client_tcp_info.lock().unwrap().push(tcp_info);
        state.obd2_client_count.fetch_add(1, Ordering::Relaxed);

        let _guard = ClientCountGuard {
            state,
            tcp_info,
            client_id,
        };

        let timeout = Duration::from_millis(state.config.lock().unwrap().obd2_timeout_ms);
        client_stream.set_read_timeout(Some(timeout))?;
        client_stream.set_write_timeout(Some(timeout))?;

        let watchdog = WatchdogHandle::register(c"obd2_client");

        let mut reader = BufReader::new(&client_stream);
        let mut writer = &client_stream;
        let mut client_state = ClientState::default();
        let mut cmd_buffer = Vec::with_capacity(64);

        loop {
            watchdog.feed();

            let mut byte = [0u8; 1];
            match reader.read(&mut byte) {
                Ok(0) => {
                    info!("OBD2 client disconnected: {peer}");
                    break;
                }
                Ok(_) => {
                    let ch = byte[0];

                    if client_state.echo_enabled {
                        writer.write_all(&byte)?;
                    }

                    if ch == b'\r' {
                        let command = String::from_utf8_lossy(&cmd_buffer);
                        let command = command.trim();

                        if !command.is_empty() {
                            debug!("OBD2 client command: {command:?}");
                            Self::process_command(
                                command,
                                &cmd_buffer,
                                &mut writer,
                                &mut client_state,
                                state,
                                &client_cache,
                            )?;
                        }

                        cmd_buffer.clear();
                    } else if ch != b'\n' {
                        cmd_buffer.push(ch);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => {
                    error!("Error reading from client: {e:?}");
                    break;
                }
            }
        }

        Ok(())
    }

    fn process_command(
        command: &str,
        raw_command: &[u8],
        writer: &mut &TcpStream,
        client_state: &mut ClientState,
        state: &Arc<State>,
        client_cache: &Arc<Mutex<ClientCache>>,
    ) -> Result<()> {
        // Handle AT commands locally
        if command.to_uppercase().starts_with("AT") {
            debug!("Handling AT command locally");

            if let Ok(mut log) = state.at_command_log.lock() {
                log.insert(command.to_uppercase());
            }

            let response = client_state.handle_at_command(command);
            writer
                .write_all(response.as_bytes())
                .context("Failed to write AT response")?;
            return Ok(());
        }

        // If capability queries haven't completed yet, reject OBD2 commands.
        // Clients will see "UNABLE TO CONNECT" and should retry, which is the
        // same response they'd get from a real ELM327 before protocol detection.
        if !state.supported_pids.lock().unwrap().ready {
            let le = client_state.line_ending();
            let error_response = format!("{le}UNABLE TO CONNECT{le}>");
            writer.write_all(error_response.as_bytes())?;
            return Ok(());
        }

        // Handle "1" repeat command
        let (effective_command, effective_raw): (String, Obd2Buffer) = if command == "1" {
            if let Some(last) = &client_state.last_obd_command {
                debug!("Expanding repeat command to: {last}");
                (last.clone(), last.as_bytes().into())
            } else {
                let le = client_state.line_ending();
                let error_response = format!("{le}?{le}>");
                writer.write_all(error_response.as_bytes())?;
                return Ok(());
            }
        } else {
            (command.to_string(), raw_command.into())
        };

        // Record unique OBD2 PIDs
        if let Ok(mut log) = state.pid_log.lock() {
            log.insert(effective_command.to_uppercase());
        }

        // Normalize command for cache key (strips trailing " 1" response count)
        let normalized = normalize_obd_command(effective_raw.as_ref());

        // Check if this is a supported PIDs query - return from State cache
        if let Some(idx) = supported_pids_index(&normalized) {
            let supported_pids_guard = state.supported_pids.lock().unwrap();
            if let Some(ref cached_response) = supported_pids_guard.entries[idx] {
                client_state.last_obd_command = Some(effective_command);
                client_state
                    .write_response(cached_response, writer)
                    .context("Failed to write supported PIDs response")?;
                drop(supported_pids_guard);
                return Ok(());
            }
            drop(supported_pids_guard);
            // Not cached (dongle not connected yet?) - return error
            let le = client_state.line_ending();
            let error_response = format!("{le}UNABLE TO CONNECT{le}>");
            writer.write_all(error_response.as_bytes())?;
            return Ok(());
        }

        // Convert normalized command to PID
        let Some(pid) = wire_command_to_pid(&normalized) else {
            // Invalid command format - return error
            let le = client_state.line_ending();
            let error_response = format!("{le}?{le}>");
            writer.write_all(error_response.as_bytes())?;
            return Ok(());
        };

        // Get value from cache
        let timeout = Duration::from_millis(state.config.lock().unwrap().obd2_timeout_ms);
        match Self::get_cached_value(pid, client_cache, state, timeout) {
            Ok(response) => {
                client_state.last_obd_command = Some(effective_command);

                let formatted = format_cached_for_client(pid, &response, client_state);
                writer
                    .write_all(&formatted)
                    .context("Failed to write response")?;
            }
            Err(e) => {
                error!("Cache error: {e}");
                let le = client_state.line_ending();
                let error_msg = e.to_elm327_error();
                let error_response = format!("{le}{error_msg}{le}>");
                writer.write_all(error_response.as_bytes())?;
            }
        }

        Ok(())
    }

    fn get_cached_value(
        pid: Pid,
        client_cache: &Arc<Mutex<ClientCache>>,
        state: &Arc<State>,
        timeout: Duration,
    ) -> Result<CachedResponse, DongleError> {
        let mut cache_guard = client_cache.lock().unwrap();

        // Swap entry with Empty, check what was there
        match cache_guard.entries.insert(pid, CacheEntry::Empty) {
            Some(CacheEntry::Fresh(v)) => {
                state
                    .cache_manager_tx
                    .send(CacheManagerMessage::Consumed {
                        pid,
                        wait_duration: None,
                    })
                    .expect("cache manager task dead");
                Ok(v)
            }
            Some(CacheEntry::Empty) | None => {
                // Cache miss - signal that we need this PID (may promote to fast)
                state
                    .cache_manager_tx
                    .send(CacheManagerMessage::Waiting(pid))
                    .expect("cache manager task dead");

                // Register waiter
                let (tx, rx) = oneshot::channel();
                cache_guard.entries.insert(pid, CacheEntry::Waiting(tx));
                let wait_start = Instant::now();
                drop(cache_guard);

                // Wait for notification
                if rx.recv_timeout(timeout).is_err() {
                    // Timeout - clean up waiter
                    let mut cache_guard = client_cache.lock().unwrap();
                    if let Some(CacheEntry::Waiting(_)) = cache_guard.entries.get(&pid) {
                        cache_guard.entries.insert(pid, CacheEntry::Empty);
                    }
                    return Err(DongleError::Timeout);
                }

                let wait_duration = wait_start.elapsed();

                // Re-lock and take the value
                let mut cache_guard = client_cache.lock().unwrap();
                let Some(CacheEntry::Fresh(v)) = cache_guard.entries.insert(pid, CacheEntry::Empty)
                else {
                    // Not Fresh after notification - logic error or race
                    return Err(DongleError::NotConnected);
                };
                state
                    .cache_manager_tx
                    .send(CacheManagerMessage::Consumed {
                        pid,
                        wait_duration: Some(wait_duration),
                    })
                    .expect("cache manager task dead");
                Ok(v)
            }
            Some(CacheEntry::Waiting(_)) => {
                panic!("Cache entry already in Waiting state - logic error");
            }
        }
    }
}
