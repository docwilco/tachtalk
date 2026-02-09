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

use tachtalk_elm327_lib::ClientState;

use crate::config::SlowPollMode;
use crate::rpm_leds::RpmTaskMessage;
use crate::watchdog::WatchdogHandle;
use crate::State;

const RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// Maintenance interval for checking promotion/demotion/removal
const MAINTENANCE_INTERVAL: Duration = Duration::from_millis(2000);
/// RPM PID command (always polled)
const RPM_PID: &[u8] = b"010C";

/// Type alias for small OBD2 command/response buffers
pub type Obd2Buffer = SmallVec<[u8; 12]>;

/// Cached OBD2 response: one entry per ECU response. Most PIDs get exactly 1.
pub type CachedResponse = SmallVec<[Obd2Buffer; 1]>;

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

/// Check if a command is a supported PIDs query, returns index (0-7) if so
fn supported_pids_index(cmd: &[u8]) -> Option<usize> {
    SUPPORTED_PID_QUERIES
        .iter()
        .position(|&q| cmd.eq_ignore_ascii_case(q))
}

/// Check if a specific PID is supported based on a supported PIDs response.
///
/// The response format is "41XX YYYYYYYY" where XX is the base PID (00, 20, 40, ...)
/// and the 4 bytes form a bitmask for PIDs (base+1) through (base+32).
///
/// For example, response to 0100 is "4100..." covering PIDs 01-20,
/// response to 0120 is "4120..." covering PIDs 21-40, etc.
///
/// Returns `false` if the response cannot be parsed or the PID is out of range.
fn is_pid_supported_in_response(response: &[u8], pid: u8) -> bool {
    // Parse response to find header and data (format: "41XXYYYYYYYY" - spaces disabled via ATS0)
    let response_str = String::from_utf8_lossy(response).to_ascii_uppercase();

    // Find "41" and extract the base PID from the next 2 hex chars
    let Some(header_pos) = response_str.find("41") else {
        return false;
    };

    // Need at least 4 chars for header (41XX)
    if response_str.len() < header_pos + 4 {
        return false;
    }

    let base_str = &response_str[header_pos + 2..header_pos + 4];
    let Ok(base_pid) = u8::from_str_radix(base_str, 16) else {
        return false;
    };

    // Check if pid is in range for this response (base+1 to base+0x20)
    if pid <= base_pid || pid > base_pid.saturating_add(0x20) {
        return false;
    }

    // Parse the 4 data bytes after the header
    let data_bytes = parse_hex_bytes(&response_str[header_pos + 4..]);

    if data_bytes.len() < 4 {
        return false;
    }

    // Calculate bit position: PID (base+1) is bit 7 of byte 0, (base+8) is bit 0 of byte 0
    // (base+9) is bit 7 of byte 1, etc.
    let offset = pid - base_pid - 1; // 0-indexed offset from base+1
    let byte_index = offset / 8;
    let bit_index = 7 - (offset % 8);

    (data_bytes[byte_index as usize] >> bit_index) & 1 == 1
}

/// Parse up to 4 hex bytes from a string, ignoring any non-hex characters
fn parse_hex_bytes(s: &str) -> Vec<u8> {
    let hex_chars: String = s.chars().filter(char::is_ascii_hexdigit).collect();
    hex_chars
        .as_bytes()
        .chunks(2)
        .take(4)
        .filter_map(|chunk| {
            if chunk.len() == 2 {
                let s = std::str::from_utf8(chunk).ok()?;
                u8::from_str_radix(s, 16).ok()
            } else {
                None
            }
        })
        .collect()
}

/// Normalize an OBD2 command for caching purposes.
///
/// Strips trailing " 1" (single response count) since it's equivalent to no count.
/// Commands with other response counts (e.g., " 2") are kept as-is since they
/// expect multiple ECU responses.
fn normalize_obd_command(cmd: &[u8]) -> Obd2Buffer {
    if cmd.len() >= 2 && cmd.ends_with(b" 1") {
        cmd[..cmd.len() - 2].into()
    } else {
        cmd.into()
    }
}

/// Count OBD2 response headers (`"41"`) in a raw dongle response to determine
/// how many ECUs responded.
fn count_response_headers(response: &[u8]) -> u8 {
    let response_str = String::from_utf8_lossy(response);
    let count = response_str.to_ascii_uppercase().matches("41").count();
    // Response counts are small; truncate to u8 if something goes wrong
    u8::try_from(count).unwrap_or(u8::MAX)
}

/// Parse a raw dongle response into individual ECU response lines.
///
/// The dongle returns responses like `b"410C1AF8\r410C1B00\r\r>"`.
/// This splits on `\r`, filters out empty strings and the `>` prompt,
/// and returns each ECU response as a separate [`Obd2Buffer`].
fn parse_response_lines(raw: &Obd2Buffer) -> CachedResponse {
    raw.split(|&b| b == b'\r')
        .filter(|line| !line.is_empty() && line != b">")
        .map(Obd2Buffer::from_slice)
        .collect()
}

/// Reconstruct an ELM327 wire-format response from cached per-ECU response lines.
///
/// Produces the format clients expect: `{le}{line1}{le}{line2}...{le}>`
/// where each line gets space-insertion via [`ClientState::format_response`].
fn format_cached_for_client(values: &CachedResponse, client_state: &ClientState) -> Vec<u8> {
    let le = client_state.line_ending();
    let mut result = Vec::new();
    for val in values {
        let formatted = client_state.format_response(val);
        result.extend_from_slice(le.as_bytes());
        result.extend_from_slice(&formatted);
    }
    result.extend_from_slice(le.as_bytes());
    result.push(b'>');
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
    entries: HashMap<Obd2Buffer, CacheEntry>,
}

impl ClientCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }
}

/// Unique identifier for a client
type ClientId = u64;

/// Priority level for PID polling
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PidPriority {
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
    /// Response from dongle task
    DongleResponse {
        /// The PID that was polled
        pid: Obd2Buffer,
        /// The parsed response lines (one per ECU), or error
        response: Result<CachedResponse, DongleError>,
    },
    /// Client registered a waiter for this PID (promote to fast)
    Waiting(Obd2Buffer),
    /// Client consumed this PID (None = cache hit, Some = cache miss with wait duration)
    Consumed {
        pid: Obd2Buffer,
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
    SetPidPriority(Obd2Buffer, bool), // (pid, is_fast)
    /// Remove a PID from polling
    RemovePid(Obd2Buffer),
}

pub type DongleSender = Sender<DongleMessage>;
pub type DongleReceiver = Receiver<DongleMessage>;

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
    /// Whether the dongle supports the "1" repeat command (None = untested)
    supports_repeat: Option<bool>,
    /// Last wire command sent to the dongle (includes response count suffix if learned)
    last_command: Option<Obd2Buffer>,
    /// Learned response counts per PID (how many ECUs respond).
    /// On first request for a PID, we send without a count to learn it.
    /// Subsequent requests append the count for faster dongle response.
    response_counts: HashMap<Obd2Buffer, u8>,
}

impl DongleState {
    /// Build the wire command for a PID, appending the learned response count if known.
    fn build_wire_command(&self, base_command: &Obd2Buffer) -> Obd2Buffer {
        if let Some(&count) = self.response_counts.get(base_command) {
            let mut cmd = base_command.clone();
            cmd.push(b' ');
            // Response counts are small (1-9 in practice)
            if count >= 10 {
                cmd.push(b'0' + count / 10);
            }
            cmd.push(b'0' + count % 10);
            cmd
        } else {
            base_command.clone()
        }
    }

    /// Learn the response count from a dongle response by counting `"41"` headers.
    fn learn_response_count(&mut self, command: &Obd2Buffer, response: &Obd2Buffer) {
        if self.response_counts.contains_key(command) {
            return;
        }
        let count = count_response_headers(response);
        if count > 0 {
            info!(
                "Learned response count for {:?}: {count}",
                String::from_utf8_lossy(command)
            );
            self.response_counts.insert(command.clone(), count);
        }
    }

    /// Execute a command, using the "1" repeat optimization when possible.
    ///
    /// On the first request for a PID, sends without a response count to learn
    /// how many ECUs respond. Subsequent requests append the learned count.
    /// The repeat comparison uses the full wire command (including any count
    /// suffix), so learning a count naturally forces a full resend.
    fn execute_with_repeat(
        &mut self,
        command: &Obd2Buffer,
        timeout: Duration,
    ) -> Result<Obd2Buffer, DongleError> {
        let is_at = command.starts_with(b"AT");

        // Build the wire command (may include learned response count)
        let wire_cmd = if is_at {
            command.clone()
        } else {
            self.build_wire_command(command)
        };

        // Check if we can try repeat command optimization
        // Compare against the full wire command so that learning a count
        // (e.g., "010C" → "010C 1") naturally forces a full resend.
        let can_try_repeat = self.supports_repeat != Some(false)
            && self.last_command.as_ref() == Some(&wire_cmd)
            && !is_at;

        if can_try_repeat {
            debug!("Trying repeat command");
            let repeat_result = execute_command(&mut self.stream, b"1", timeout);

            // Check if repeat worked
            if let Ok(response) = &repeat_result {
                let response_str = String::from_utf8_lossy(response);
                if response_str.contains('?') {
                    // Repeat not supported, mark and resend full command
                    info!("Dongle does not support repeat command");
                    self.supports_repeat = Some(false);
                    self.last_command = Some(wire_cmd.clone());
                    let result = execute_command(&mut self.stream, &wire_cmd, timeout);
                    if let Ok(ref resp) = result {
                        self.learn_response_count(command, resp);
                    } else {
                        self.last_command = None;
                    }
                    result
                } else {
                    // Repeat worked! last_command stays the same
                    if self.supports_repeat.is_none() {
                        info!("Dongle supports repeat command");
                        self.supports_repeat = Some(true);
                    }
                    repeat_result
                }
            } else {
                // Repeat failed with error - clear last_command
                self.last_command = None;
                repeat_result
            }
        } else {
            // Update last_command before sending
            if !is_at {
                self.last_command = Some(wire_cmd.clone());
            }
            let result = execute_command(&mut self.stream, &wire_cmd, timeout);
            match &result {
                Ok(resp) => self.learn_response_count(command, resp),
                Err(_) => {
                    self.last_command = None;
                }
            }
            result
        }
    }
}

// ============================================================================
// Dongle Task
// ============================================================================

/// Polling state for the dongle task
struct PollingState {
    fast_pids: IndexSet<Obd2Buffer>,
    slow_pids: IndexSet<Obd2Buffer>,
    fast_index: usize,
    slow_index: usize,
    last_slow_poll: Instant,
    fast_requests_since_slow: u32,
}

impl Default for PollingState {
    fn default() -> Self {
        // Always start with RPM in fast queue
        Self {
            fast_pids: IndexSet::from([RPM_PID.into()]),
            slow_pids: IndexSet::new(),
            fast_index: 0,
            slow_index: 0,
            last_slow_poll: Instant::now(),
            fast_requests_since_slow: 0,
        }
    }
}

impl PollingState {
    fn next_fast_pid(&mut self) -> Option<Obd2Buffer> {
        let pid = self.fast_pids.get_index(self.fast_index)?.clone();
        self.fast_index = (self.fast_index + 1) % self.fast_pids.len();
        Some(pid)
    }

    fn next_slow_pid(&mut self) -> Option<Obd2Buffer> {
        let pid = self.slow_pids.get_index(self.slow_index)?.clone();
        self.slow_index = (self.slow_index + 1) % self.slow_pids.len();
        Some(pid)
    }

    fn set_pid_priority(&mut self, pid: Obd2Buffer, is_fast: bool) {
        // Remove from both sets first
        self.fast_pids.swap_remove(&pid);
        self.slow_pids.swap_remove(&pid);

        // Add to appropriate set
        if is_fast {
            self.fast_pids.insert(pid);
        } else {
            self.slow_pids.insert(pid);
        }

        self.fix_indices();
    }

    fn remove_pid(&mut self, pid: &Obd2Buffer) {
        self.fast_pids.swap_remove(pid);
        self.slow_pids.swap_remove(pid);
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
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect();
        }
        if let Ok(mut slow) = state.polling_metrics.slow_pids.lock() {
            *slow = self
                .slow_pids
                .iter()
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect();
        }
    }

    /// Poll one fast PID and optionally one slow PID, sending results to the cache manager.
    ///
    /// Returns the number of requests made this iteration.
    fn poll_pids(
        &mut self,
        connection: &mut Option<DongleState>,
        state: &Arc<State>,
        timeout: Duration,
    ) -> u32 {
        let Some(dongle_state) = connection.as_mut() else {
            return 0;
        };

        let (slow_poll_mode, slow_poll_interval, slow_poll_ratio) = {
            let cfg = state.config.lock().unwrap();
            (
                cfg.obd2.slow_poll_mode,
                Duration::from_millis(cfg.obd2.slow_poll_interval_ms),
                cfg.obd2.slow_poll_ratio,
            )
        };

        let mut requests = 0;

        // Always try to poll a fast PID
        if let Some(pid) = self.next_fast_pid() {
            let result = dongle_state.execute_with_repeat(&pid, timeout);
            handle_connection_error(&result, connection, state);

            let _ = state
                .cache_manager_tx
                .send(CacheManagerMessage::DongleResponse {
                    pid: pid.clone(),
                    response: result.map(|raw| parse_response_lines(&raw)),
                });

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
                    let result = dongle_state.execute_with_repeat(&pid, timeout);
                    handle_connection_error(&result, connection, state);

                    let _ = state
                        .cache_manager_tx
                        .send(CacheManagerMessage::DongleResponse {
                            pid: pid.clone(),
                            response: result.map(|raw| parse_response_lines(&raw)),
                        });

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
                DongleMessage::SetPidPriority(pid, is_fast) => {
                    polling.set_pid_priority(pid, is_fast);
                }
                DongleMessage::RemovePid(pid) => {
                    polling.remove_pid(&pid);
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

fn handle_connection_error(
    result: &Result<Obd2Buffer, DongleError>,
    connection: &mut Option<DongleState>,
    state: &Arc<State>,
) {
    if matches!(
        result,
        Err(DongleError::Disconnected | DongleError::IoError(_))
    ) {
        warn!("Dongle connection lost, will reconnect");
        *connection = None;
        state.dongle_connected.store(false, Ordering::Relaxed);
        *state.dongle_tcp_info.lock().unwrap() = None;
        // Clear stale capability data so a reconnect to a different vehicle
        // doesn't serve outdated supported-PID responses.
        *state.supported_pids.lock().unwrap() = SupportedPidsCache::default();
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
    pid_info: &mut HashMap<Obd2Buffer, PidInfo>,
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
        if pid.as_slice() == RPM_PID {
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
            pids_to_remove.push(pid.clone());
            continue;
        }

        // Check demotion (no waiters recently)
        if info.priority == PidPriority::Fast {
            let should_demote = info
                .last_waiter_time
                .map_or(true, |t| t.elapsed() > fast_demotion_ms);
            if should_demote {
                pids_to_demote.push(pid.clone());
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
        info!("PID {:?}: demoted to slow", String::from_utf8_lossy(&pid));
        let _ = state
            .dongle_control_tx
            .send(DongleMessage::SetPidPriority(pid, false));
    }

    // Remove inactive PIDs
    for pid in pids_to_remove {
        pid_info.remove(&pid);
        state
            .polling_metrics
            .removals
            .fetch_add(1, Ordering::Relaxed);
        info!(
            "PID {:?}: removed (inactive)",
            String::from_utf8_lossy(&pid)
        );
        let _ = state.dongle_control_tx.send(DongleMessage::RemovePid(pid));
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
            let pid_str = String::from_utf8_lossy(pid).to_string();
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
    pid: &Obd2Buffer,
    data: &CachedResponse,
) {
    for client_cache in clients.values() {
        let mut cache_guard = client_cache.lock().unwrap();
        match cache_guard.entries.get_mut(pid) {
            Some(CacheEntry::Waiting(_)) => {
                // Take the sender, replace with Fresh, then send wake
                let old = std::mem::replace(
                    cache_guard.entries.get_mut(pid).unwrap(),
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
                    .insert(pid.clone(), CacheEntry::Fresh(data.clone()));
            }
        }
    }
}

/// Handle a PID entering the Waiting state: create or promote PID info as appropriate.
fn handle_waiting_pid(
    pid: &Obd2Buffer,
    pid_info: &mut HashMap<Obd2Buffer, PidInfo>,
    state: &Arc<State>,
) {
    let slow_poll_mode = {
        let cfg = state.config.lock().unwrap();
        cfg.obd2.slow_poll_mode
    };

    let is_new = !pid_info.contains_key(pid);
    let info = pid_info.entry(pid.clone()).or_insert_with(|| PidInfo {
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
        log::info!("PID {:?}: promoted to fast", String::from_utf8_lossy(pid));
        let _ = state
            .dongle_control_tx
            .send(DongleMessage::SetPidPriority(pid.clone(), true));
    } else if is_new {
        log::info!(
            "PID {:?}: added to fast queue",
            String::from_utf8_lossy(pid)
        );
        let _ = state
            .dongle_control_tx
            .send(DongleMessage::SetPidPriority(pid.clone(), true));
    }
}

/// Handle a PID being consumed by a client: track metrics and promote slow PIDs if wait time
/// exceeds the configured threshold (in ratio mode).
fn handle_consumed_pid(
    pid: &Obd2Buffer,
    wait_duration: Option<Duration>,
    pid_info: &mut HashMap<Obd2Buffer, PidInfo>,
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

    if let Some(info) = pid_info.get_mut(pid) {
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
                "PID {:?}: promoted to fast (wait {}ms >= {}ms threshold)",
                String::from_utf8_lossy(pid),
                wait_duration.unwrap().as_millis(),
                promotion_threshold.as_millis()
            );
            let _ = state
                .dongle_control_tx
                .send(DongleMessage::SetPidPriority(pid.clone(), true));
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
    let mut pid_info: HashMap<Obd2Buffer, PidInfo> = HashMap::new();
    let mut next_client_id: ClientId = 1;
    let mut last_maintenance = Instant::now();

    let mut cache_metrics = CacheMetrics {
        hits: 0,
        misses: 0,
        total_wait_time: Duration::ZERO,
    };

    // Initialize RPM PID info
    pid_info.insert(
        RPM_PID.into(),
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
            Ok(CacheManagerMessage::DongleResponse { pid, response }) => {
                if let Ok(ref data) = response {
                    update_client_caches(&clients, &pid, data);

                    // Extract RPM from the first ECU response line
                    if pid.as_slice() == RPM_PID {
                        if let Some(first) = data.first() {
                            if let Some(rpm) = tachtalk_elm327_lib::extract_rpm_from_response(first)
                            {
                                debug!("Cache manager extracted RPM: {rpm}");
                                let _ = state.rpm_tx.send(RpmTaskMessage::Rpm(rpm));
                                *state.shared_rpm.lock().unwrap() = Some(rpm);
                            }
                        }
                    }
                }
            }
            Ok(CacheManagerMessage::Waiting(pid)) => {
                handle_waiting_pid(&pid, &mut pid_info, state);
            }
            Ok(CacheManagerMessage::Consumed { pid, wait_duration }) => {
                handle_consumed_pid(
                    &pid,
                    wait_duration,
                    &mut pid_info,
                    state,
                    &mut cache_metrics,
                );
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

/// Check if a multi-PID response contains both RPM (0C) and speed (0D) data.
///
/// Multi-PID responses can be:
/// 1. Separate headers: `"410CXXXX410DYY"` (each PID has its own 41 prefix)
/// 2. Concatenated: `"410CXXXX0DYY"` (only first PID has header)
///
/// Either way, a successful response will have `410C` followed by RPM data
/// (2 bytes = 4 hex chars), then `0D` data somewhere after.
fn is_valid_multi_pid_response(response_str: &str) -> bool {
    let has_rpm = response_str.contains("410C");
    let has_speed = if let Some(pos) = response_str.find("410C") {
        let after_rpm = &response_str[pos + 4..]; // skip "410C"
                                                  // RPM is 2 bytes = 4 hex chars, then look for 0D or 410D
        after_rpm.len() >= 4 && {
            let after_rpm_data = &after_rpm[4..]; // skip RPM data
            after_rpm_data.starts_with("410D") || after_rpm_data.starts_with("0D")
        }
    } else {
        false
    };
    has_rpm && has_speed
}

/// Test whether the ECU supports multi-PID queries by requesting RPM (0C) and speed (0D) together.
///
/// Only tests if enabled in config and both PIDs are supported according to the 0100 response.
fn test_multi_pid_support(
    stream: &mut TcpStream,
    timeout: Duration,
    watchdog: &WatchdogHandle,
    state: &Arc<State>,
) -> bool {
    if !state.config.lock().unwrap().obd2.test_multi_pid {
        return false;
    }

    info!("Testing multi-PID query support...");
    watchdog.feed();

    let supported_0100 = if let Some(response) = &state.supported_pids.lock().unwrap().entries[0] {
        response.clone()
    } else {
        info!("No 0100 response available, skipping multi-PID test");
        return false;
    };

    // Check if PIDs 0C and 0D are supported in the 0100 bitmask
    if !is_pid_supported_in_response(&supported_0100, 0x0C)
        || !is_pid_supported_in_response(&supported_0100, 0x0D)
    {
        info!("PIDs 0C and/or 0D not supported, skipping multi-PID test");
        return false;
    }

    let response = match execute_command(stream, b"010C0D", timeout) {
        Ok(r) => r,
        Err(e) => {
            info!("Multi-PID query test failed: {e}, assuming not supported");
            return false;
        }
    };

    let response_str = String::from_utf8_lossy(&response).to_ascii_uppercase();
    if !is_valid_multi_pid_response(&response_str) {
        info!("ECU does not support multi-PID queries (response: {response_str:?})");
        return false;
    }

    info!("ECU supports multi-PID queries");

    // Test if repeat command works with multi-PID queries
    match execute_command(stream, b"1", timeout) {
        Ok(repeat_response) => {
            let repeat_str = String::from_utf8_lossy(&repeat_response).to_ascii_uppercase();
            if is_valid_multi_pid_response(&repeat_str) {
                info!("Repeat command works with multi-PID queries");
            } else if repeat_str.contains('?') {
                info!("Repeat command not supported");
            } else {
                info!("Repeat command gave unexpected response for multi-PID: {repeat_str:?}");
            }
        }
        Err(e) => {
            info!("Repeat command test failed: {e}");
        }
    }

    true
}

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

    let mut stream = stream;

    // Initialize the dongle
    let init_commands = [
        b"ATZ\r".as_slice(),
        b"ATE0\r".as_slice(),
        b"ATL0\r".as_slice(),
        b"ATS0\r".as_slice(),
        b"ATSP0\r".as_slice(),
    ];

    for cmd in init_commands {
        debug!("Sending init command: {:?}", String::from_utf8_lossy(cmd));
        if let Err(e) = stream.write_all(cmd) {
            error!("Failed to send init command: {e}");
            return None;
        }
        let mut buf = [0u8; 256];
        std::thread::sleep(Duration::from_millis(100));
        let _ = stream.read(&mut buf);
    }

    info!("Connected to OBD2 dongle, querying supported PIDs...");

    // Clear stale data and mark not-ready before re-querying
    let mut supported_pids_guard = state.supported_pids.lock().unwrap();
    *supported_pids_guard = SupportedPidsCache::default();
    for (idx, query) in SUPPORTED_PID_QUERIES.iter().enumerate() {
        watchdog.feed();
        match execute_command(&mut stream, query, timeout) {
            Ok(response) => {
                debug!(
                    "Supported PIDs query {:?}: {:?}",
                    String::from_utf8_lossy(query),
                    String::from_utf8_lossy(&response)
                );
                supported_pids_guard.entries[idx] = Some(response);
            }
            Err(e) => {
                debug!(
                    "Supported PIDs query {:?} failed: {e}",
                    String::from_utf8_lossy(query)
                );
                supported_pids_guard.entries[idx] = None;
            }
        }
    }
    // All 8 queries attempted — mark capabilities as ready
    supported_pids_guard.ready = true;
    drop(supported_pids_guard);

    // Test multi-PID query support
    let supports_multi_pid = test_multi_pid_support(&mut stream, timeout, watchdog, state);
    state
        .supports_multi_pid
        .store(supports_multi_pid, Ordering::Relaxed);

    info!("Connected to OBD2 dongle");

    Some((
        DongleState {
            stream,
            supports_repeat: None,
            last_command: None,
            response_counts: HashMap::new(),
        },
        local_addr,
        remote_addr,
    ))
}

/// Execute a command on the dongle and return the response
fn execute_command(
    stream: &mut TcpStream,
    command: &[u8],
    timeout: Duration,
) -> Result<Obd2Buffer, DongleError> {
    let mut cmd_with_cr: Obd2Buffer = command.into();
    if !cmd_with_cr.ends_with(b"\r") {
        cmd_with_cr.push(b'\r');
    }

    debug!(
        "Sending to dongle: {:?}",
        String::from_utf8_lossy(&cmd_with_cr)
    );

    stream
        .write_all(&cmd_with_cr)
        .map_err(|e| DongleError::IoError(e.to_string()))?;

    let mut buffer = [0u8; 64];
    let mut response = Obd2Buffer::new();
    let start = Instant::now();

    loop {
        match stream.read(&mut buffer) {
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

    Ok(response)
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

                    crate::thread_util::spawn_named(c"obd2_client", move || {
                        if let Err(e) = Self::handle_client(stream, &state) {
                            error!("Error handling client: {e:?}");
                        }
                    });
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
        let cache_key = normalize_obd_command(effective_raw.as_ref());

        // Check if this is a supported PIDs query - return from State cache
        if let Some(idx) = supported_pids_index(&cache_key) {
            let supported_pids_guard = state.supported_pids.lock().unwrap();
            if let Some(ref cached_response) = supported_pids_guard.entries[idx] {
                client_state.last_obd_command = Some(effective_command);
                let formatted = client_state.format_response(cached_response);
                drop(supported_pids_guard);
                writer
                    .write_all(&formatted)
                    .context("Failed to write supported PIDs response")?;
                return Ok(());
            }
            drop(supported_pids_guard);
            // Not cached (dongle not connected yet?) - return error
            let le = client_state.line_ending();
            let error_response = format!("{le}UNABLE TO CONNECT{le}>");
            writer.write_all(error_response.as_bytes())?;
            return Ok(());
        }

        // Get value from cache
        let timeout = Duration::from_millis(state.config.lock().unwrap().obd2_timeout_ms);
        match Self::get_cached_value(&cache_key, client_cache, state, timeout) {
            Ok(response) => {
                client_state.last_obd_command = Some(effective_command);

                let formatted = format_cached_for_client(&response, client_state);
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
        pid: &Obd2Buffer,
        client_cache: &Arc<Mutex<ClientCache>>,
        state: &Arc<State>,
        timeout: Duration,
    ) -> Result<CachedResponse, DongleError> {
        let mut cache_guard = client_cache.lock().unwrap();

        // Swap entry with Empty, check what was there
        match cache_guard.entries.insert(pid.clone(), CacheEntry::Empty) {
            Some(CacheEntry::Fresh(v)) => {
                let _ = state.cache_manager_tx.send(CacheManagerMessage::Consumed {
                    pid: pid.clone(),
                    wait_duration: None,
                });
                Ok(v)
            }
            Some(CacheEntry::Empty) | None => {
                // Cache miss - signal that we need this PID (may promote to fast)
                let _ = state
                    .cache_manager_tx
                    .send(CacheManagerMessage::Waiting(pid.clone()));

                // Register waiter
                let (tx, rx) = oneshot::channel();
                cache_guard
                    .entries
                    .insert(pid.clone(), CacheEntry::Waiting(tx));
                let wait_start = Instant::now();
                drop(cache_guard);

                // Wait for notification
                if rx.recv_timeout(timeout).is_err() {
                    // Timeout - clean up waiter
                    let mut cache_guard = client_cache.lock().unwrap();
                    if let Some(CacheEntry::Waiting(_)) = cache_guard.entries.get(pid) {
                        cache_guard.entries.insert(pid.clone(), CacheEntry::Empty);
                    }
                    return Err(DongleError::Timeout);
                }

                let wait_duration = wait_start.elapsed();

                // Re-lock and take the value
                let mut cache_guard = client_cache.lock().unwrap();
                let Some(CacheEntry::Fresh(v)) =
                    cache_guard.entries.insert(pid.clone(), CacheEntry::Empty)
                else {
                    // Not Fresh after notification - logic error or race
                    return Err(DongleError::NotConnected);
                };
                let _ = state.cache_manager_tx.send(CacheManagerMessage::Consumed {
                    pid: pid.clone(),
                    wait_duration: Some(wait_duration),
                });
                Ok(v)
            }
            Some(CacheEntry::Waiting(_)) => {
                panic!("Cache entry already in Waiting state - logic error");
            }
        }
    }
}
