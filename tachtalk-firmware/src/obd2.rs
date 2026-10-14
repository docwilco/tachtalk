//! OBD2 proxy with proactive PID caching.
//!
//! Architecture:
//! - Dongle task: owns the TCP connection, polls PIDs based on fast/slow queues
//! - Cache manager task: receives responses, updates per-client caches, manages PID priority
//! - Proxy server: accepts client connections, reads from cache
//! - AT commands (ATE0, ATZ, etc.) are handled locally per connection using `tachtalk_elm327`

use crate::error::{Error, Result};
use indexmap::IndexSet;
use log::{debug, error, info, warn};
use smallvec::SmallVec;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tachtalk_capture_format::{
    CaptureHeader, RecordType, FLAG_OVERFLOW, HEADER_SIZE, RECORD_HEADER_SIZE,
};
use tachtalk_elm327_lib::{
    ClientConfig, ClientSession, Command, FeatureSupport, Identity, Lines, ObdCommand, Pid,
    PidGroup, ServerSession,
};

use crate::config::{PidPriority, SlowPollMode};
use crate::led_display::LedTaskMessage;
use crate::status_leds::StatusLedMessage;
use crate::watchdog::WatchdogHandle;
use crate::State;
use atomic_enum::atomic_enum;

// ---------------------------------------------------------------------------
// DongleTcpState (shared across tasks)
// ---------------------------------------------------------------------------

/// OBD2 dongle TCP connection state, stored atomically on [`State`](crate::State).
#[atomic_enum]
#[derive(Default, PartialEq, Eq)]
pub enum DongleTcpState {
    /// No TCP connection
    #[default]
    Disconnected = 0,
    /// TCP connected, running AT init commands / querying supported PIDs
    Initializing = 1,
    /// Fully initialized and processing requests
    Ready = 2,
}

impl Default for AtomicDongleTcpState {
    fn default() -> Self {
        Self::new(DongleTcpState::default())
    }
}

const RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// Maintenance interval for checking promotion/demotion/removal
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(2);

/// Cached OBD2 response: one reply line per ECU (`41 {pid} {data}`), with
/// the ECU's id when the dongle sends headers. Most PIDs get exactly 1.
type CachedResponse = Lines;

// ============================================================================
// PID/Wire Conversion Helpers
// ============================================================================

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

/// Cached supported-PID query responses plus a readiness flag.
///
/// The `ready` flag is `false` while connecting or after a disconnect, and
/// `true` once all 8 supported-PID queries have been attempted.
#[derive(Default)]
pub struct SupportedPidsCache {
    pub entries: [Option<CachedResponse>; 8],
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

/// A canonical command (uppercase, no spaces) without its response count,
/// for caching and lookup.
///
/// A Mode 01 query is `01` and PID pairs; a single digit after them is the
/// client's response-count hint (`010C1`), which the cache key leaves out,
/// whatever the count.
fn strip_response_count(cmd: &[u8]) -> &[u8] {
    if cmd.len() % 2 == 1 {
        &cmd[..cmd.len() - 1]
    } else {
        cmd
    }
}

// ============================================================================
// Response Parsing (Framing Layer)
// ============================================================================

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
    /// Pinned by an active profile — never demoted or removed
    pinned: bool,
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
    /// Profile PIDs have changed — re-register immediately
    ProfilePidsChanged,
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

/// Why a client's wait for a PID value failed.
#[derive(Debug, Clone)]
pub enum DongleError {
    NotConnected,
    Timeout,
}

impl std::fmt::Display for DongleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConnected => write!(f, "Not connected to dongle"),
            Self::Timeout => write!(f, "Dongle timeout"),
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
        }
    }
}

// ============================================================================
// Dongle Connection
// ============================================================================

/// The dongle's TCP stream, recording every write and read into the capture
/// buffer while capture is switched on.
struct CapturingStream {
    stream: TcpStream,
    /// Shared state: the capture switch and buffer.
    state: Arc<State>,
    /// Capture buffer size snapshot from config.
    capture_buffer_size: u32,
    /// Connection start, for relative capture timestamps.
    capture_start: Instant,
}

impl CapturingStream {
    fn record(&self, record_type: RecordType, data: &[u8]) {
        if !data.is_empty() && self.state.capture_active.load(Ordering::Relaxed) {
            record_event(
                &self.state,
                self.capture_start.elapsed(),
                record_type,
                data,
                self.capture_buffer_size,
            );
        }
    }
}

impl Read for CapturingStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.stream.read(buf)?;
        self.record(RecordType::DongleToClient, &buf[..n]);
        Ok(n)
    }
}

impl Write for CapturingStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.stream.write(buf)?;
        self.record(RecordType::ClientToDongle, &buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

/// A connected, initialized dongle.
type DongleSession = ClientSession<CapturingStream>;

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
    /// ECU counts per PID, learned from the dongle's replies. Kept across
    /// reconnects, where the session's own knowledge starts afresh.
    ecu_counts: HashMap<Pid, u8>,
}

impl Default for PollingState {
    fn default() -> Self {
        // Start empty — profile PIDs are registered by cache_manager on startup
        Self {
            fast_pids: IndexSet::new(),
            slow_pids: IndexSet::new(),
            fast_index: 0,
            slow_index: 0,
            last_slow_poll: Instant::now(),
            fast_requests_since_slow: 0,
            ecu_counts: HashMap::new(),
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
        dongle: &DongleSession,
    ) -> SmallVec<(Option<u8>, PidGroup), 4> {
        let mut groups: HashMap<Option<u8>, PidGroup> = HashMap::new();

        for &pid in self.fast_pids.iter().chain(self.slow_pids.iter()) {
            let has_data_len = dongle.server_state().data_length(pid).is_some();
            let key = if has_data_len {
                self.ecu_counts.get(&pid).copied()
            } else {
                None
            };
            groups.entry(key).or_default().push(pid);
        }

        groups.into_iter().collect()
    }

    /// Query one PID with the ECU count learned for it.
    fn query_single(
        &mut self,
        dongle: &mut DongleSession,
        pid: Pid,
    ) -> std::io::Result<HashMap<Pid, CachedResponse>> {
        let result = dongle.query_pid(pid, self.ecu_counts.get(&pid).copied());
        self.keep_ecu_counts(dongle, &[pid]);
        result
    }

    /// Keep the ECU counts the session has learned for `pids`; a count,
    /// once learned, is not replaced.
    fn keep_ecu_counts(&mut self, dongle: &DongleSession, pids: &[Pid]) {
        for &pid in pids {
            if let Some(count) = dongle.server_state().ecu_count(pid) {
                self.ecu_counts.entry(pid).or_insert(count);
            }
        }
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
                .map(|k| format!("01{k:02X}"))
                .collect();
        }
        if let Ok(mut slow) = state.polling_metrics.slow_pids.lock() {
            *slow = self
                .slow_pids
                .iter()
                .map(|k| format!("01{k:02X}"))
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
    fn poll_pids(&mut self, connection: &mut Option<DongleSession>, state: &Arc<State>) -> u32 {
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
                .is_some_and(|ds| ds.server_state().multi_pid() != FeatureSupport::No);

        if try_multi {
            self.poll_pids_multi(connection, state, max_pids_per_query)
        } else {
            self.poll_pids_single(
                connection,
                state,
                slow_poll_mode,
                slow_poll_interval,
                slow_poll_ratio,
            )
        }
    }

    /// Single-PID polling: one fast PID round-robin, plus one slow PID when due.
    fn poll_pids_single(
        &mut self,
        connection: &mut Option<DongleSession>,
        state: &Arc<State>,
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
            let result = self.query_single(dongle_state, pid);
            handle_dongle_result(result, connection, state);

            requests += 1;
            self.fast_requests_since_slow += 1;
            state
                .polling_metrics
                .dongle_requests_total
                .fetch_add(1, Ordering::Relaxed);
            let _ = state.status_led_tx.send(StatusLedMessage::DongleActivity);
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
                    let result = self.query_single(dongle_state, pid);
                    handle_dongle_result(result, connection, state);

                    requests += 1;
                    state
                        .polling_metrics
                        .dongle_requests_total
                        .fetch_add(1, Ordering::Relaxed);
                    let _ = state.status_led_tx.send(StatusLedMessage::DongleActivity);
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
        connection: &mut Option<DongleSession>,
        state: &Arc<State>,
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
                            let result = self.query_single(ds, chunk[0]);
                            handle_dongle_result(result, connection, state);
                        } else {
                            let result = ds.query_pids(chunk, Some(*count));
                            self.keep_ecu_counts(ds, chunk);
                            handle_dongle_result(result, connection, state);

                            // If multi-PID was just rejected, stop sending more chunks —
                            // the next poll_pids call will use single-PID mode.
                            if connection.as_ref().map_or(true, |ds| {
                                ds.server_state().multi_pid() == FeatureSupport::No
                            }) {
                                requests += 1;
                                state
                                    .polling_metrics
                                    .dongle_requests_total
                                    .fetch_add(1, Ordering::Relaxed);
                                let _ = state.status_led_tx.send(StatusLedMessage::DongleActivity);
                                return requests;
                            }
                        }

                        requests += 1;
                        state
                            .polling_metrics
                            .dongle_requests_total
                            .fetch_add(1, Ordering::Relaxed);
                        let _ = state.status_led_tx.send(StatusLedMessage::DongleActivity);
                    }
                }
                // Unknown ECU count or data length: query individually to learn
                None => {
                    for &pid in group {
                        let Some(ref mut ds) = connection else {
                            break;
                        };
                        let result = self.query_single(ds, pid);
                        handle_dongle_result(result, connection, state);

                        requests += 1;
                        state
                            .polling_metrics
                            .dongle_requests_total
                            .fetch_add(1, Ordering::Relaxed);
                        let _ = state.status_led_tx.send(StatusLedMessage::DongleActivity);
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

    let mut connection: Option<DongleSession> = None;
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
            requests_this_second += polling.poll_pids(&mut connection, state);
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
    connection: &mut Option<DongleSession>,
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
            state
                .dongle_tcp_state
                .store(DongleTcpState::Ready, Ordering::Relaxed);
            let _ = state
                .status_led_tx
                .send(StatusLedMessage::DongleState(DongleTcpState::Ready));
            *state.dongle_tcp_info.lock().unwrap() = Some((local_addr, remote_addr));
        } else {
            state
                .dongle_tcp_state
                .store(DongleTcpState::Disconnected, Ordering::Relaxed);
            let _ = state
                .status_led_tx
                .send(StatusLedMessage::DongleState(DongleTcpState::Disconnected));
            *state.dongle_tcp_info.lock().unwrap() = None;
            *state.supported_pids.lock().unwrap() = SupportedPidsCache::default();
        }
    }
}

/// Handle a dongle result: forward parsed data to the cache manager on
/// success, tear down the connection on fatal errors.
fn handle_dongle_result(
    result: std::io::Result<HashMap<Pid, CachedResponse>>,
    connection: &mut Option<DongleSession>,
    state: &Arc<State>,
) {
    match result {
        Ok(responses) => {
            state
                .cache_manager_tx
                .send(CacheManagerMessage::DongleResponse { responses })
                .expect("cache manager task dead");
        }
        // A timeout leaves the connection up; anything else (closed by the
        // dongle, an I/O error) drops it.
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
        Err(e) => {
            warn!("Dongle connection lost ({e}), will reconnect");
            *connection = None;
            state
                .dongle_tcp_state
                .store(DongleTcpState::Disconnected, Ordering::Relaxed);
            let _ = state
                .status_led_tx
                .send(StatusLedMessage::DongleState(DongleTcpState::Disconnected));
            *state.dongle_tcp_info.lock().unwrap() = None;
            // Clear stale capability data so a reconnect to a different vehicle
            // doesn't serve outdated supported-PID responses.
            *state.supported_pids.lock().unwrap() = SupportedPidsCache::default();
        }
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
        // Skip pinned PIDs — required by active profiles
        if info.pinned {
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
            let pid_str = format!("01{pid:02X}");
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

/// Auto-disable triggered profiles when a PID value exceeds `auto_disable_above`.
fn check_triggered_auto_disable(pid: Pid, raw_value: u32, state: &Arc<State>) {
    let mut notify = false;
    {
        let cfg = state.config.lock().unwrap();
        let mut triggered_enabled = state.triggered_enabled.lock().unwrap();

        for (idx, profile) in cfg.triggered_profiles() {
            if profile.pid != pid {
                continue;
            }
            let Some(threshold) = profile.auto_disable_above else {
                continue;
            };
            if !triggered_enabled.get(idx).copied().unwrap_or(false) {
                continue;
            }
            if raw_value > threshold {
                triggered_enabled[idx] = false;
                info!(
                    "Auto-disabled triggered profile '{}' [{}]: raw value {raw_value} > threshold {threshold}",
                    profile.name, idx
                );
                notify = true;
            }
        }
    }
    if notify {
        state.notify_profile_change();
    }
}

/// Register (or reconcile) PIDs needed by active profiles.
///
/// Reads the current config, computes which PIDs are required by the active
/// normal profile and all overlay profiles, and updates `pid_info` accordingly:
/// - New profile PIDs are added as pinned with their configured priority.
/// - Existing profile PIDs are pinned (in case they were previously unpinned).
/// - PIDs that are no longer needed by any profile are unpinned (subject to
///   normal demotion/removal in maintenance).
fn register_profile_pids(pid_info: &mut HashMap<Pid, PidInfo>, state: &Arc<State>) {
    // Collect PIDs needed by active profiles with their highest priority
    let mut needed: HashMap<Pid, PidPriority> = HashMap::new();
    {
        let cfg = state.config.lock().unwrap();
        let triggered_enabled = state.triggered_enabled.lock().unwrap();

        // Active normal profile
        if let Some(profile) = cfg.active_normal_profile() {
            needed
                .entry(profile.pid)
                .and_modify(|p| {
                    if profile.pid_priority == PidPriority::Fast {
                        *p = PidPriority::Fast;
                    }
                })
                .or_insert(profile.pid_priority);
        }

        // All overlay profiles (always active) + enabled triggered profiles
        for (_, profile) in cfg
            .overlay_profiles()
            .chain(cfg.enabled_triggered_profiles(&triggered_enabled))
        {
            needed
                .entry(profile.pid)
                .and_modify(|p| {
                    if profile.pid_priority == PidPriority::Fast {
                        *p = PidPriority::Fast;
                    }
                })
                .or_insert(profile.pid_priority);
        }
    }

    // Unpin PIDs no longer needed by any profile
    for (pid, info) in &mut *pid_info {
        if info.pinned && !needed.contains_key(pid) {
            info.pinned = false;
            info!("PID {pid:02X}: unpinned (no longer needed by profiles)");
        }
    }

    // Pin (and add/upgrade) PIDs needed by profiles
    for (pid, priority) in &needed {
        let info = pid_info.entry(*pid).or_insert_with(|| {
            info!("PID {pid:02X}: added as pinned ({priority:?})");
            state
                .dongle_control_tx
                .send(DongleMessage::SetPidPriority(*pid, *priority))
                .expect("dongle task dead");
            PidInfo {
                last_waiter_time: None,
                last_consumed_time: Some(Instant::now()),
                priority: *priority,
                request_count: 0,
                pinned: true,
            }
        });
        info.pinned = true;

        // Upgrade priority if profile needs Fast but PID is currently Slow
        if *priority == PidPriority::Fast && info.priority == PidPriority::Slow {
            info.priority = PidPriority::Fast;
            info!("PID {pid:02X}: upgraded to fast (profile requirement)");
            state
                .dongle_control_tx
                .send(DongleMessage::SetPidPriority(*pid, PidPriority::Fast))
                .expect("dongle task dead");
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
        pinned: false,
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

    // Register PIDs needed by active profiles
    register_profile_pids(&mut pid_info, state);

    info!("Cache manager task started");

    loop {
        watchdog.feed();

        // Process messages with timeout (wakes immediately on message, or after maintenance interval)
        match manager_rx.recv_timeout(MAINTENANCE_INTERVAL) {
            Ok(CacheManagerMessage::DongleResponse { responses }) => {
                // Update all PIDs from the response (may be multi-PID)
                for (pid, data) in &responses {
                    update_client_caches(&clients, *pid, data);

                    // Convert raw response bytes to u32 and send to LED task
                    let raw = data.first().map_or(0, |line| {
                        line.data
                            .iter()
                            .skip(2) // mode and PID bytes
                            .take(4)
                            .fold(0u32, |acc, &b| (acc << 8) | u32::from(b))
                    });
                    debug!("Cache manager raw PID 0x{pid:02X}: {raw}");
                    let _ = state.led_tx.send(LedTaskMessage::PidValue {
                        pid: *pid,
                        value: raw,
                    });
                    // Keep shared_rpm updated for backward compatibility (decoded RPM)
                    if *pid == 0x0C {
                        *state.shared_rpm.lock().unwrap() = Some(raw / 4);
                    }

                    // Auto-disable triggered profiles whose threshold is exceeded
                    check_triggered_auto_disable(*pid, raw, state);
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
            Ok(CacheManagerMessage::ProfilePidsChanged) => {
                register_profile_pids(&mut pid_info, state);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Normal timeout, continue to maintenance
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                error!("Cache manager channel disconnected, exiting");
                break;
            }
        }

        // Periodic maintenance: check promotion/demotion/removal and reconcile profile PIDs
        if last_maintenance.elapsed() >= MAINTENANCE_INTERVAL {
            register_profile_pids(&mut pid_info, state);
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
) -> Option<(DongleSession, SocketAddr, SocketAddr)> {
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

    // Signal that TCP is connected and we're starting initialization
    state
        .dongle_tcp_state
        .store(DongleTcpState::Initializing, Ordering::Relaxed);
    let _ = state
        .status_led_tx
        .send(StatusLedMessage::DongleState(DongleTcpState::Initializing));

    watchdog.feed();

    if let Err(e) = stream.set_read_timeout(Some(timeout)) {
        error!("Failed to set read timeout: {e}");
        return None;
    }
    if let Err(e) = stream.set_write_timeout(Some(timeout)) {
        error!("Failed to set write timeout: {e}");
        return None;
    }

    let cfg = state.config.lock().unwrap();
    let stream = CapturingStream {
        stream,
        state: state.clone(),
        capture_buffer_size: cfg.obd2.capture_buffer_size,
        capture_start: Instant::now(),
    };
    let config = ClientConfig {
        framing: cfg.obd2.use_framing,
        repeat: cfg.obd2.use_repeat,
        timeout,
        ..ClientConfig::default()
    };
    drop(cfg);

    let mut dongle = ClientSession::new(stream, config);
    if let Err(e) = dongle.init() {
        error!("Dongle init failed: {e}");
        return None;
    }

    info!("Connected to OBD2 dongle, querying supported PIDs...");
    query_supported_pids(&mut dongle, state, watchdog);
    info!("Connected to OBD2 dongle");

    Some((dongle, local_addr, remote_addr))
}

/// Query the 8 standard supported-PID ranges and store results in `State`.
fn query_supported_pids(dongle: &mut DongleSession, state: &Arc<State>, watchdog: &WatchdogHandle) {
    let mut guard = state.supported_pids.lock().unwrap();
    *guard = SupportedPidsCache::default();
    for (idx, pid) in (0..=0xE0).step_by(0x20).enumerate() {
        watchdog.feed();
        match dongle.query_pid(pid, None) {
            Ok(mut parsed) => {
                debug!("Supported PIDs query 01{pid:02X}: {parsed:?}");
                // No Mode 01 line (NO DATA) stores an empty answer.
                guard.entries[idx] = Some(parsed.remove(&pid).unwrap_or_default());
            }
            Err(e) => {
                debug!("Supported PIDs query 01{pid:02X} failed: {e}");
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
        let count = self.state.obd2_client_count.fetch_sub(1, Ordering::Relaxed) - 1;
        let _ = self
            .state
            .status_led_tx
            .send(StatusLedMessage::ClientCount(count));
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
            .map_err(|_| Error::CacheManagerChannelClosed)?;
        let (client_id, client_cache) = reply_rx
            .recv()
            .map_err(|_| Error::CacheManagerRegistrationFailed)?;

        // Track connection info
        let tcp_info = (local, peer);
        state.client_tcp_info.lock().unwrap().push(tcp_info);
        let count = state.obd2_client_count.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = state
            .status_led_tx
            .send(StatusLedMessage::ClientCount(count));

        let _guard = ClientCountGuard {
            state,
            tcp_info,
            client_id,
        };

        let timeout = Duration::from_millis(state.config.lock().unwrap().obd2_timeout_ms);
        client_stream.set_read_timeout(Some(timeout))?;
        client_stream.set_write_timeout(Some(timeout))?;

        let watchdog = WatchdogHandle::register(c"obd2_client");

        let mut session = ServerSession::new(&client_stream, Identity::default());

        loop {
            watchdog.feed();

            match session.next_command() {
                Ok(Command::At(at)) => {
                    debug!("OBD2 client command: {:?}", at.command());
                    debug!("Handling AT command locally");
                    if let Ok(mut log) = state.at_command_log.lock() {
                        log.insert(at.command().to_uppercase());
                    }
                    at.answer()?;
                }
                Ok(Command::Obd(obd)) => {
                    debug!("OBD2 client command: {:?}", obd.command());
                    Self::answer_obd(obd, state, &client_cache)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    info!("OBD2 client disconnected: {peer}");
                    break;
                }
                Err(e) => {
                    error!("Error reading from client: {e:?}");
                    break;
                }
            }
        }

        Ok(())
    }

    /// Answer a non-AT command: readiness gate, supported-PID cache, then
    /// the client cache. The session has already expanded a bare-CR repeat.
    fn answer_obd(
        obd: ObdCommand<'_, &TcpStream>,
        state: &Arc<State>,
        client_cache: &Arc<Mutex<ClientCache>>,
    ) -> Result<()> {
        // Signal client activity for status LED (non-AT OBD2 command)
        let _ = state.status_led_tx.send(StatusLedMessage::ClientActivity);

        // If capability queries haven't completed yet, reject OBD2 commands.
        // Clients will see "UNABLE TO CONNECT" and should retry, which is the
        // same response they'd get from a real ELM327 before protocol detection.
        if !state.supported_pids.lock().unwrap().ready {
            obd.reject("UNABLE TO CONNECT")?;
            return Ok(());
        }

        // Record unique OBD2 PIDs
        if let Ok(mut log) = state.pid_log.lock() {
            log.insert(obd.command().to_uppercase());
        }

        // The cache key is the command without its response count
        let normalized = strip_response_count(obd.command().as_bytes());

        // Check if this is a supported PIDs query - return from State cache
        if let Some(idx) = supported_pids_index(normalized) {
            let supported_pids_guard = state.supported_pids.lock().unwrap();
            if let Some(ref lines) = supported_pids_guard.entries[idx] {
                // The dongle answered without a Mode 01 line.
                if lines.is_empty() {
                    obd.reject("NO DATA")?;
                } else {
                    obd.reply(lines)?;
                }
                return Ok(());
            }
            drop(supported_pids_guard);
            // Not cached (dongle not connected yet?) - return error
            obd.reject("UNABLE TO CONNECT")?;
            return Ok(());
        }

        // Convert normalized command to PID
        let Some(pid) = wire_command_to_pid(normalized) else {
            // Invalid command format - return error
            obd.reject("?")?;
            return Ok(());
        };

        // Get value from cache
        let timeout = Duration::from_millis(state.config.lock().unwrap().obd2_timeout_ms);
        match Self::get_cached_value(pid, client_cache, state, timeout) {
            Ok(response) => obd.reply(&response)?,
            Err(e) => {
                error!("Cache error: {e}");
                obd.reject(e.to_elm327_error())?;
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
