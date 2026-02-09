//! OBD2 test module for benchmarking query strategies.
//!
//! Modes:
//! 1. `NoCount`: Send PID as-is (baseline)
//! 2. `AlwaysOne`: Append ` 1` to all requests
//! 3. `AdaptiveCount`: Detect ECU count on first request, use that count
//! 4. `Pipelined`: Send multiple requests before waiting for responses
//! 5. `RawCapture`: Pure TCP proxy with traffic recording to PSRAM

use crate::config::{pid_data_length, CaptureOverflow, QueryMode, StartOptions};
use crate::watchdog::WatchdogHandle;
use crate::{State, TestControlMessage};
use log::{debug, error, info, warn};
use smallvec::SmallVec;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tachtalk_capture_format::{
    CaptureHeader, RecordType, FLAG_OVERFLOW, HEADER_SIZE, RECORD_HEADER_SIZE,
};

/// Type alias for small OBD2 command/response buffers
pub type Obd2Buffer = SmallVec<[u8; 32]>;

/// Fixed fast:slow polling ratio
const FAST_SLOW_RATIO: u32 = 6;

/// Firmware version string for capture header
const FIRMWARE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Snapshot of test configuration taken at test start.
struct TestConfig {
    start_options: StartOptions,
    dongle_ip: String,
    dongle_port: u16,
    timeout: Duration,
    fast_pids: SmallVec<[u8; 8]>,
    slow_pids: SmallVec<[u8; 8]>,
    pipeline_bytes: u16,
    listen_port: u16,
    capture: CaptureConfig,
}

impl TestConfig {
    /// Take a snapshot of the current test configuration.
    fn snapshot(state: &State, start_options: StartOptions) -> Self {
        let cfg_guard = state.config.lock().unwrap();
        Self {
            start_options,
            dongle_ip: cfg_guard.test.dongle_ip.clone(),
            dongle_port: cfg_guard.test.dongle_port,
            timeout: Duration::from_millis(cfg_guard.test.obd2_timeout_ms),
            fast_pids: cfg_guard.test.get_fast_pids(),
            slow_pids: cfg_guard.test.get_slow_pids(),
            pipeline_bytes: cfg_guard.test.pipeline_bytes,
            listen_port: cfg_guard.test.listen_port,
            capture: CaptureConfig {
                buffer_size: cfg_guard.test.capture_buffer_size,
                overflow: cfg_guard.test.capture_overflow,
            },
        }
    }

    /// Format the dongle address as `ip:port`.
    fn dongle_addr(&self) -> String {
        format!("{}:{}", self.dongle_ip, self.dongle_port)
    }
}

/// Capture buffer configuration.
#[derive(Clone, Copy)]
struct CaptureConfig {
    buffer_size: u32,
    overflow: CaptureOverflow,
}

/// Round-robin PID selector with configurable fast:slow ratio.
///
/// Used by the legacy pipelined test mode.
struct PidSelector {
    fast_index: usize,
    slow_index: usize,
    fast_count: u32,
}

impl PidSelector {
    fn new() -> Self {
        Self {
            fast_index: 0,
            slow_index: 0,
            fast_count: 0,
        }
    }

    /// Select the next PID using the fast:slow ratio.
    ///
    /// Returns `None` when both PID lists are empty.
    fn next(&mut self, fast_pids: &[u8], slow_pids: &[u8]) -> Option<u8> {
        if self.fast_count < FAST_SLOW_RATIO && !fast_pids.is_empty() {
            self.fast_count += 1;
            let pid = fast_pids[self.fast_index % fast_pids.len()];
            self.fast_index += 1;
            Some(pid)
        } else if !slow_pids.is_empty() {
            self.fast_count = 0;
            let pid = slow_pids[self.slow_index % slow_pids.len()];
            self.slow_index += 1;
            Some(pid)
        } else if !fast_pids.is_empty() {
            self.fast_count = 0;
            let pid = fast_pids[self.fast_index % fast_pids.len()];
            self.fast_index += 1;
            Some(pid)
        } else {
            None
        }
    }
}

// ============================================================================
// Layer 1: Base — TCP send/receive with `\r` and `>` prompt detection
// ============================================================================

/// Lowest layer: sends bytes with `\r` suffix, reads until `>` prompt.
///
/// Every command and response is recorded to the shared capture buffer
/// for later download.
struct Base {
    stream: TcpStream,
    timeout: Duration,
    /// Capture state for recording traffic.
    capture_state: CaptureState,
}

/// State needed to record traffic to the shared capture buffer.
struct CaptureState {
    state: Arc<State>,
    config: CaptureConfig,
    start: Instant,
}

impl Base {
    /// Connect to dongle, configure socket, and run general AT init commands.
    ///
    /// Does **not** send `ATH` — that is the [`FramingLayer`]'s responsibility.
    ///
    /// All sent commands and received responses are recorded to the shared
    /// capture buffer via `capture_state`.
    fn connect_and_init(
        dongle_ip: &str,
        dongle_port: u16,
        timeout: Duration,
        capture_state: CaptureState,
    ) -> Result<Self, String> {
        let addr = format!("{dongle_ip}:{dongle_port}");
        info!("Connecting to dongle at {addr}...");

        let stream = TcpStream::connect(&addr).map_err(|e| format!("Connect failed: {e}"))?;
        stream.set_read_timeout(Some(timeout)).ok();
        stream.set_nodelay(true).ok();

        let mut base = Self {
            stream,
            timeout,
            capture_state,
        };

        // General AT init (no ATH — that belongs to FramingLayer)
        base.execute(b"ATZ")?;
        base.execute(b"ATE0")?;
        base.execute(b"ATS0")?;
        base.execute(b"ATL0")?;

        info!("Dongle initialized (base layer)");
        Ok(base)
    }

    /// Send `command` + `\r`, read until `>` prompt, return raw response.
    ///
    /// When capture is enabled, records the sent command as `ClientToDongle`
    /// and the raw response as `DongleToClient`.
    fn execute(&mut self, command: &[u8]) -> Result<Obd2Buffer, String> {
        let mut cmd_with_cr: Obd2Buffer = command.into();
        if !cmd_with_cr.ends_with(b"\r") {
            cmd_with_cr.push(b'\r');
        }

        debug!(
            "Sending to dongle: {:?}",
            String::from_utf8_lossy(&cmd_with_cr)
        );

        // Record sent command
        record_event(
            &self.capture_state.state,
            self.capture_state.start.elapsed(),
            RecordType::ClientToDongle,
            &cmd_with_cr,
            self.capture_state.config,
        );

        self.stream
            .write_all(&cmd_with_cr)
            .map_err(|e| format!("Write error: {e}"))?;

        let mut buffer = [0u8; 128];
        let mut response = Obd2Buffer::new();
        let start = Instant::now();

        loop {
            if start.elapsed() > self.timeout {
                return Err("Timeout".to_string());
            }

            match self.stream.read(&mut buffer) {
                Ok(0) => return Err("Disconnected".to_string()),
                Ok(n) => {
                    response.extend_from_slice(&buffer[..n]);
                    if response.contains(&b'>') {
                        debug!(
                            "Complete response: {:?}",
                            String::from_utf8_lossy(&response)
                        );
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    return Err("Timeout".to_string());
                }
                Err(e) => return Err(format!("Read error: {e}")),
            }
        }

        // Record received response
        record_event(
            &self.capture_state.state,
            self.capture_state.start.elapsed(),
            RecordType::DongleToClient,
            &response,
            self.capture_state.config,
        );

        Ok(response)
    }
}

// ============================================================================
// Layer 2: Repeat — reuse last command via configurable repeat string
// ============================================================================

/// Tracks the last command sent and, when the same command is requested again,
/// sends the configured repeat string instead of the full command.
struct RepeatLayer {
    base: Base,
    enabled: bool,
    /// Repeat command bytes (empty = bare CR per ELM327 spec, `b"1"` = WiFi dongle convention).
    repeat_string: SmallVec<[u8; 2]>,
    last_command: Option<SmallVec<[u8; 16]>>,
    supports_repeat: Option<bool>,
}

impl RepeatLayer {
    fn new(base: Base, enabled: bool, repeat_string: &[u8]) -> Self {
        Self {
            base,
            enabled,
            repeat_string: repeat_string.into(),
            last_command: None,
            supports_repeat: None,
        }
    }

    fn execute(&mut self, command: &[u8]) -> Result<Obd2Buffer, String> {
        if self.enabled && self.supports_repeat != Some(false) {
            if let Some(ref last) = self.last_command {
                if last.as_slice() == command {
                    // Same command — send repeat string
                    let response = self.base.execute(&self.repeat_string)?;
                    let response_str = String::from_utf8_lossy(&response);
                    if response_str.contains('?') {
                        info!("Repeat not supported by dongle, falling back to full command");
                        self.supports_repeat = Some(false);
                        return self.base.execute(command);
                    }
                    return Ok(response);
                }
            }
        }

        // Different command or repeat disabled — send full command
        let response = self.base.execute(command)?;
        if self.enabled {
            self.last_command = Some(command.into());
        }
        Ok(response)
    }
}

// ============================================================================
// Layer 3: Framing — ATH1 header parsing and PCI byte verification
// ============================================================================

/// A single parsed response line, optionally with ECU CAN ID.
struct ParsedLine {
    /// 3-char CAN ID when framing is enabled (e.g., `b"7E8"`).
    ecu_id: Option<SmallVec<[u8; 3]>>,
    /// OBD data bytes after the PCI byte (when framing on) or raw line bytes.
    data: SmallVec<[u8; 8]>,
}

/// Handles `ATH1`/`ATH0` init and response line parsing.
struct FramingLayer {
    repeat: RepeatLayer,
    enabled: bool,
}

impl FramingLayer {
    fn new(repeat: RepeatLayer, enabled: bool) -> Self {
        Self { repeat, enabled }
    }

    /// Send `ATH1` or `ATH0` through the layer stack.
    /// Must be called after construction and before any OBD queries.
    fn init(&mut self) -> Result<(), String> {
        let cmd = if self.enabled { b"ATH1" } else { b"ATH0" };
        self.repeat.execute(cmd.as_slice())?;
        info!("Framing layer initialized: ATH{}", u8::from(self.enabled));
        Ok(())
    }

    /// Send a command through the stack, return raw response.
    fn execute(&mut self, command: &[u8]) -> Result<Obd2Buffer, String> {
        self.repeat.execute(command)
    }

    /// Parse a raw response buffer into individual lines.
    fn parse_response(&self, response: &[u8]) -> SmallVec<[ParsedLine; 4]> {
        let response_str = String::from_utf8_lossy(response);
        let mut lines = SmallVec::new();

        // ELM327 with ATL0 uses bare '\r' as line terminator.
        // str::lines() only splits on '\n' and '\r\n', not bare '\r'.
        for line in response_str.split('\r') {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.contains('>') {
                continue;
            }

            if self.enabled {
                // Framing on: expect `{3-char CAN ID} {PCI byte} {OBD data...}`
                // With ATS0 (spaces off), format is `{3-char CAN ID}{PCI byte}{data}`
                // but ATH1 with ATS0 still uses spaces between CAN ID and rest
                // Actually with ATS0, there are no spaces at all: "7E806410C1AF8"
                // So we need to handle: first 3 chars = CAN ID, next 2 = PCI hex byte, rest = data
                let bytes: SmallVec<[u8; 16]> = trimmed.as_bytes().into();
                if bytes.len() < 5 {
                    // Too short for framing format
                    lines.push(ParsedLine {
                        ecu_id: None,
                        data: bytes.into_iter().collect(),
                    });
                    continue;
                }

                let ecu_id: SmallVec<[u8; 3]> = bytes[..3].into();

                // Parse PCI byte (2 hex chars after CAN ID)
                let pci_str = std::str::from_utf8(&bytes[3..5]).unwrap_or("00");
                let pci_byte = u8::from_str_radix(pci_str, 16).unwrap_or(0);

                // Remaining data after PCI (hex pairs, no spaces with ATS0)
                let data_hex = &bytes[5..];
                let actual_data_bytes = data_hex.len() / 2;

                if actual_data_bytes != pci_byte as usize {
                    warn!(
                        "PCI byte mismatch: PCI={pci_byte}, actual data bytes={actual_data_bytes}, line={trimmed}"
                    );
                }

                // Parse the data hex bytes
                let mut data: SmallVec<[u8; 8]> = SmallVec::new();
                let mut i = 0;
                while i + 1 < data_hex.len() {
                    if let Ok(b) = u8::from_str_radix(
                        std::str::from_utf8(&data_hex[i..i + 2]).unwrap_or("00"),
                        16,
                    ) {
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
                let mut data: SmallVec<[u8; 8]> = SmallVec::new();
                let bytes = hex_str.as_bytes();
                let mut i = 0;
                while i + 1 < bytes.len() {
                    if let Ok(b) = u8::from_str_radix(
                        std::str::from_utf8(&bytes[i..i + 2]).unwrap_or("00"),
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
}

// ============================================================================
// Layer 4: Count — ECU response count learning and appending
// ============================================================================

/// Learns ECU count from responses and appends the count suffix to commands.
struct CountLayer {
    framing: FramingLayer,
    mode: QueryMode,
    /// Learned ECU counts keyed by command string.
    response_counts: HashMap<SmallVec<[u8; 16]>, u8>,
    /// Number of PIDs in the current query (set by [`QueryBuilder`] before each call).
    pid_count: usize,
}

impl CountLayer {
    fn new(framing: FramingLayer, mode: QueryMode) -> Self {
        Self {
            framing,
            mode,
            response_counts: HashMap::new(),
            pid_count: 1,
        }
    }

    /// Execute a command, learning ECU count if in `AdaptiveCount` mode.
    ///
    /// Returns the raw response and parsed lines.
    fn execute(
        &mut self,
        command: &[u8],
    ) -> Result<(Obd2Buffer, SmallVec<[ParsedLine; 4]>), String> {
        let cmd_key: SmallVec<[u8; 16]> = command.into();

        let actual_command: SmallVec<[u8; 20]> = match self.mode {
            QueryMode::NoCount => command.into(),
            QueryMode::AlwaysOne => {
                let mut cmd: SmallVec<[u8; 20]> = command.into();
                cmd.extend_from_slice(b" 1");
                cmd
            }
            QueryMode::AdaptiveCount => {
                if let Some(&count) = self.response_counts.get(&cmd_key) {
                    let mut cmd: SmallVec<[u8; 20]> = command.into();
                    cmd.push(b' ');
                    // count is always 1-9 for OBD, single ASCII digit
                    cmd.push(b'0' + count);
                    cmd
                } else {
                    // First request — send without count to learn
                    command.into()
                }
            }
            // Pipelined and RawCapture don't use the layer stack
            QueryMode::Pipelined | QueryMode::RawCapture => unreachable!(),
        };

        let response = self.framing.execute(&actual_command)?;
        let parsed = self.framing.parse_response(&response);

        // Learn ECU count for AdaptiveCount on first request
        if self.mode == QueryMode::AdaptiveCount && !self.response_counts.contains_key(&cmd_key) {
            let ecu_count = self.count_ecus(&parsed);
            if ecu_count > 0 {
                info!(
                    "Learned ECU count for {:?}: {ecu_count}",
                    String::from_utf8_lossy(command)
                );
                self.response_counts.insert(cmd_key, ecu_count);
            }
        }

        Ok((response, parsed))
    }

    /// Count ECUs from parsed response lines.
    fn count_ecus(&self, parsed: &[ParsedLine]) -> u8 {
        if self.framing.enabled {
            // Framing on: count unique CAN IDs
            let mut seen: SmallVec<[&[u8]; 4]> = SmallVec::new();
            for line in parsed {
                if let Some(ref id) = line.ecu_id {
                    if !seen.contains(&id.as_slice()) {
                        seen.push(id.as_slice());
                    }
                }
            }
            // Small ECU counts, always fits u8
            #[allow(clippy::cast_possible_truncation)]
            let count = seen.len() as u8;
            count
        } else if self.pid_count <= 1 {
            // Framing off, single-PID: count non-empty data lines
            #[allow(clippy::cast_possible_truncation)]
            let count = parsed.len() as u8;
            count
        } else {
            // Framing off, multi-PID: walk each line counting PID responses
            let total_pid_responses = Self::count_pid_responses_in_lines(parsed);
            if total_pid_responses == 0 || self.pid_count == 0 {
                return 0;
            }
            // Total PID responses / queried PID count = ECU count
            // Always fits u8 for realistic ECU counts
            #[allow(clippy::cast_possible_truncation)]
            let count = (total_pid_responses / self.pid_count) as u8;
            count
        }
    }

    /// Walk parsed lines counting individual PID responses using known data lengths.
    ///
    /// Multi-PID CAN responses have ONE service byte (`0x41`) followed by
    /// concatenated PID + data pairs: `41 | 0C xx xx | 49 xx | 05 xx`.
    fn count_pid_responses_in_lines(parsed: &[ParsedLine]) -> usize {
        let mut total = 0;

        for line in parsed {
            let data = &line.data;
            let mut pos = 0;

            if pos >= data.len() {
                continue;
            }

            // Expect single service byte 0x41 (Mode 01 response) at the start
            if data[pos] != 0x41 {
                continue;
            }
            pos += 1;

            // Walk PID + data pairs
            while pos < data.len() {
                let pid = data[pos];
                pos += 1; // skip PID byte

                let data_len = pid_data_length(pid) as usize;
                if data_len == 0 {
                    warn!("Unknown PID 0x{pid:02X} in multi-PID response, stopping parse");
                    break;
                }

                if pos + data_len > data.len() {
                    // Incomplete PID response — might be split across lines
                    break;
                }

                pos += data_len;
                total += 1;
            }
        }

        total
    }
}

// ============================================================================
// Layer 5: QueryBuilder — PID combination and polling loop driver
// ============================================================================

/// Builds OBD commands (single or multi-PID) and drives the polling loop.
///
/// All PIDs are Mode 01 — stored as raw PID bytes (e.g. `0x0C` for RPM).
struct QueryBuilder {
    count: CountLayer,
    fast_pids: SmallVec<[u8; 8]>,
    slow_pids: SmallVec<[u8; 8]>,
    use_multi_pid: bool,
}

impl QueryBuilder {
    /// Create a new `QueryBuilder`, validating PID data lengths for multi-PID mode.
    fn new(
        count: CountLayer,
        fast_pids: SmallVec<[u8; 8]>,
        slow_pids: SmallVec<[u8; 8]>,
        use_multi_pid: bool,
    ) -> Result<Self, String> {
        if use_multi_pid {
            // Validate all PIDs have known data lengths
            for &pid in fast_pids.iter().chain(slow_pids.iter()) {
                if pid_data_length(pid) == 0 {
                    return Err(format!("Unknown PID data length for 0x{pid:02X}"));
                }
            }
        }

        Ok(Self {
            count,
            fast_pids,
            slow_pids,
            use_multi_pid,
        })
    }

    /// Build a command string from a set of PID bytes.
    ///
    /// Returns `(command_bytes, pid_count)`. All PIDs are Mode 01.
    /// Single: `[0x0C]` → `b"010C"`. Multi: `[0x0C, 0x49]` → `b"010C49"`.
    fn build_command(pids: &[u8]) -> (SmallVec<[u8; 16]>, usize) {
        if pids.is_empty() {
            return (SmallVec::new(), 0);
        }

        let mut cmd: SmallVec<[u8; 16]> = SmallVec::new();
        // Service byte prefix
        cmd.extend_from_slice(b"01");
        for &pid in pids {
            // Format each PID byte as two uppercase hex ASCII chars
            let hi = Self::nibble_to_hex(pid >> 4);
            let lo = Self::nibble_to_hex(pid & 0x0F);
            cmd.push(hi);
            cmd.push(lo);
        }
        let count = pids.len();
        (cmd, count)
    }

    /// Convert a nibble (0-15) to an uppercase hex ASCII byte.
    const fn nibble_to_hex(nibble: u8) -> u8 {
        if nibble < 10 {
            b'0' + nibble
        } else {
            b'A' + (nibble - 10)
        }
    }

    /// Run the polling loop with the configured layer stack.
    fn run_polling_loop(
        &mut self,
        ctx: &TestContext,
        capture_config: CaptureConfig,
    ) -> Result<(), String> {
        let mut fast_index: usize = 0;
        let mut slow_index: usize = 0;
        let mut fast_count: u32 = 0;
        let mut last_second = Instant::now();
        let mut requests_this_second = 0u32;

        loop {
            ctx.watchdog.feed();

            if ctx.check_stop()? {
                return Ok(());
            }

            // Update requests/sec and buffer usage metrics
            if last_second.elapsed() >= Duration::from_secs(1) {
                ctx.state
                    .metrics
                    .requests_per_sec
                    .store(requests_this_second, Ordering::Relaxed);
                requests_this_second = 0;
                last_second = Instant::now();

                let buf_len = ctx.state.capture_buffer.lock().unwrap().len() as u64;
                let usage_pct =
                    u32::try_from(buf_len * 100 / u64::from(capture_config.buffer_size))
                        .expect("percentage fits in u32");
                ctx.state
                    .metrics
                    .buffer_usage_pct
                    .store(usage_pct, Ordering::Relaxed);
            }

            if self.fast_pids.is_empty() && self.slow_pids.is_empty() {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }

            // Select PIDs and build command based on mode
            let command = if self.use_multi_pid {
                // Multi-PID mode: build combined commands
                let use_slow = fast_count >= FAST_SLOW_RATIO && !self.slow_pids.is_empty();
                if use_slow {
                    fast_count = 0;
                    // Combined fast + slow PIDs
                    let all_pids: SmallVec<[u8; 8]> = self
                        .fast_pids
                        .iter()
                        .chain(self.slow_pids.iter())
                        .copied()
                        .collect();
                    let (cmd, pid_count) = Self::build_command(&all_pids);
                    self.count.pid_count = pid_count;
                    cmd
                } else {
                    fast_count += 1;
                    // Fast PIDs only — copy to avoid borrow conflict with &mut self
                    let fast_copy: SmallVec<[u8; 8]> = self.fast_pids.clone();
                    let (cmd, pid_count) = Self::build_command(&fast_copy);
                    self.count.pid_count = pid_count;
                    cmd
                }
            } else {
                // Single-PID mode: round-robin with 6:1 fast:slow ratio
                let pid = if fast_count < FAST_SLOW_RATIO && !self.fast_pids.is_empty() {
                    fast_count += 1;
                    let p = self.fast_pids[fast_index % self.fast_pids.len()];
                    fast_index += 1;
                    p
                } else if !self.slow_pids.is_empty() {
                    fast_count = 0;
                    let p = self.slow_pids[slow_index % self.slow_pids.len()];
                    slow_index += 1;
                    p
                } else {
                    fast_count = 0;
                    let p = self.fast_pids[fast_index % self.fast_pids.len()];
                    fast_index += 1;
                    p
                };

                self.count.pid_count = 1;
                let (cmd, _) = Self::build_command(&[pid]);
                cmd
            };

            if command.is_empty() {
                continue;
            }

            // Execute through layer stack
            match self.count.execute(&command) {
                Ok((_response, _parsed)) => {
                    ctx.state
                        .metrics
                        .total_requests
                        .fetch_add(1, Ordering::Relaxed);
                    requests_this_second += 1;
                }
                Err(e) => {
                    warn!("Request failed: {e}");
                    ctx.state
                        .metrics
                        .total_errors
                        .fetch_add(1, Ordering::Relaxed);

                    if e.contains("Disconnect") {
                        ctx.state.dongle_connected.store(false, Ordering::Relaxed);
                        return Err(e);
                    }
                }
            }
        }
    }
}

// ============================================================================
// Test Task Entry Points
// ============================================================================

/// Runtime context passed to all test functions.
struct TestContext<'a> {
    state: &'a Arc<State>,
    control_rx: &'a std::sync::mpsc::Receiver<TestControlMessage>,
    watchdog: &'a WatchdogHandle,
}

impl TestContext<'_> {
    /// Check the control channel for a stop command.
    ///
    /// Returns `Ok(true)` if stop was received, `Ok(false)` if no message or
    /// start (ignored while running), and `Err` if the channel disconnected.
    fn check_stop(&self) -> Result<bool, String> {
        match self.control_rx.try_recv() {
            Ok(TestControlMessage::Stop) => {
                info!("Stop command received");
                Ok(true)
            }
            Ok(TestControlMessage::Start(_)) => {
                debug!("Ignoring start command while running");
                Ok(false)
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => Ok(false),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err("Control channel disconnected".to_string())
            }
        }
    }
}

/// Connect to the dongle using the Base layer (for non-layered modes).
fn connect_dongle(config: &TestConfig) -> Result<TcpStream, String> {
    let addr = config.dongle_addr();
    info!("Connecting to dongle at {addr}...");

    let mut stream = TcpStream::connect(&addr).map_err(|e| format!("Connect failed: {e}"))?;
    stream.set_read_timeout(Some(config.timeout)).ok();
    stream.set_nodelay(true).ok();

    init_dongle(&mut stream, config.timeout)?;
    info!("Dongle initialized");
    Ok(stream)
}

/// Build a [`CaptureHeader`] from the current device state.
pub fn build_capture_header(state: &State) -> [u8; HEADER_SIZE] {
    let record_count = state.metrics.records_captured.load(Ordering::Relaxed);
    let data_length = state.metrics.bytes_captured.load(Ordering::Relaxed);

    let (dongle_ip, dongle_port) = {
        let cfg_guard = state.config.lock().unwrap();
        let ip: std::net::Ipv4Addr = cfg_guard
            .test
            .dongle_ip
            .parse()
            .unwrap_or(std::net::Ipv4Addr::UNSPECIFIED);
        (ip.octets(), cfg_guard.test.dongle_port)
    };

    let flags = if state.metrics.capture_overflow.load(Ordering::Relaxed) {
        FLAG_OVERFLOW
    } else {
        0
    };

    let mut header = CaptureHeader {
        record_count,
        data_length,
        dongle_ip,
        dongle_port,
        flags,
        ..CaptureHeader::default()
    };

    header.set_firmware_version(FIRMWARE_VERSION);

    header.to_bytes()
}

/// Initialize dongle connection with standard AT commands (for non-layered modes).
fn init_dongle(stream: &mut TcpStream, timeout: Duration) -> Result<(), String> {
    execute_command(stream, b"ATZ", timeout)?;
    execute_command(stream, b"ATE0", timeout)?;
    execute_command(stream, b"ATS0", timeout)?;
    execute_command(stream, b"ATL0", timeout)?;
    execute_command(stream, b"ATH0", timeout)?;
    Ok(())
}

/// Execute a command on the dongle (for non-layered modes).
fn execute_command(
    stream: &mut TcpStream,
    command: &[u8],
    timeout: Duration,
) -> Result<Obd2Buffer, String> {
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
        .map_err(|e| format!("Write error: {e}"))?;

    let mut buffer = [0u8; 128];
    let mut response = Obd2Buffer::new();
    let start = Instant::now();

    loop {
        if start.elapsed() > timeout {
            return Err("Timeout".to_string());
        }

        match stream.read(&mut buffer) {
            Ok(0) => return Err("Disconnected".to_string()),
            Ok(n) => {
                response.extend_from_slice(&buffer[..n]);
                if response.contains(&b'>') {
                    debug!(
                        "Complete response: {:?}",
                        String::from_utf8_lossy(&response)
                    );
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                return Err("Timeout".to_string());
            }
            Err(e) => return Err(format!("Read error: {e}")),
        }
    }

    Ok(response)
}

/// Main test task — handles all query modes.
pub fn test_task(state: &Arc<State>, control_rx: &std::sync::mpsc::Receiver<TestControlMessage>) {
    let watchdog = WatchdogHandle::register(c"test_task");
    let ctx = TestContext {
        state,
        control_rx,
        watchdog: &watchdog,
    };

    info!("Test task started, waiting for commands...");

    loop {
        watchdog.feed();

        // Wait for start command
        match control_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(TestControlMessage::Start(start_options)) => {
                info!("Test start command received");
                run_test(&ctx, start_options);
            }
            Ok(TestControlMessage::Stop) => {
                debug!("Stop command received while idle");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Normal timeout, continue waiting
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                error!("Control channel disconnected");
                return;
            }
        }
    }
}

/// Run a test with the current configuration.
fn run_test(ctx: &TestContext, start_options: StartOptions) {
    let config = TestConfig::snapshot(ctx.state, start_options);

    // Reset metrics
    ctx.state.metrics.reset();
    ctx.state
        .metrics
        .test_running
        .store(true, Ordering::Relaxed);

    info!(
        "Starting test with mode {:?}",
        config.start_options.query_mode
    );
    info!(
        "Fast PIDs: [{}], Slow PIDs: [{}]",
        config
            .fast_pids
            .iter()
            .map(|p| format!("0x{p:02X}"))
            .collect::<SmallVec<[String; 4]>>()
            .join(", "),
        config
            .slow_pids
            .iter()
            .map(|p| format!("0x{p:02X}"))
            .collect::<SmallVec<[String; 4]>>()
            .join(", "),
    );
    info!(
        "Options: multi_pid={}, repeat={}, framing={}",
        config.start_options.use_multi_pid,
        config.start_options.use_repeat,
        config.start_options.use_framing
    );

    let result = match config.start_options.query_mode {
        QueryMode::NoCount | QueryMode::AlwaysOne | QueryMode::AdaptiveCount => {
            run_polling_test(ctx, &config)
        }
        QueryMode::Pipelined => run_pipelined_test(ctx, &config),
        QueryMode::RawCapture => run_capture_test(ctx, &config),
    };

    ctx.state
        .metrics
        .test_running
        .store(false, Ordering::Relaxed);
    ctx.state.dongle_connected.store(false, Ordering::Relaxed);

    match result {
        Ok(()) => info!("Test completed normally"),
        Err(e) => warn!("Test ended: {e}"),
    }
}

/// Run polling test (modes 1-3) using the layered architecture.
fn run_polling_test(ctx: &TestContext, config: &TestConfig) -> Result<(), String> {
    // Pre-allocate capture buffer for traffic recording
    {
        let mut buf_guard = ctx.state.capture_buffer.lock().unwrap();
        buf_guard.clear();
        buf_guard.reserve(config.capture.buffer_size as usize);
    }

    let capture_state = CaptureState {
        state: Arc::clone(ctx.state),
        config: config.capture,
        start: Instant::now(),
    };

    // Layer 1: Base — connect and general AT init
    let base = Base::connect_and_init(
        &config.dongle_ip,
        config.dongle_port,
        config.timeout,
        capture_state,
    )?;
    ctx.state.dongle_connected.store(true, Ordering::Relaxed);

    // Layer 2: Repeat
    let repeat = RepeatLayer::new(
        base,
        config.start_options.use_repeat,
        config.start_options.repeat_string.as_bytes(),
    );

    // Layer 3: Framing
    let mut framing = FramingLayer::new(repeat, config.start_options.use_framing);
    framing.init()?;

    // Layer 4: Count
    let count = CountLayer::new(framing, config.start_options.query_mode);

    // Layer 5: QueryBuilder
    let mut query_builder = QueryBuilder::new(
        count,
        config.fast_pids.clone(),
        config.slow_pids.clone(),
        config.start_options.use_multi_pid,
    )?;

    // Run the polling loop
    query_builder.run_polling_loop(ctx, config.capture)
}

/// Run pipelined test (mode 4)
fn run_pipelined_test(ctx: &TestContext, config: &TestConfig) -> Result<(), String> {
    let mut stream = connect_dongle(config)?;
    ctx.state.dongle_connected.store(true, Ordering::Relaxed);

    let mut pid_selector = PidSelector::new();
    let mut last_second = Instant::now();
    let mut requests_this_second = 0u32;

    // Pipeline state
    let mut pending_count: usize = 0;
    let mut bytes_on_wire = 0u16;

    loop {
        ctx.watchdog.feed();

        if ctx.check_stop()? {
            return Ok(());
        }

        // Update requests/sec
        if last_second.elapsed() >= Duration::from_secs(1) {
            ctx.state
                .metrics
                .requests_per_sec
                .store(requests_this_second, Ordering::Relaxed);
            requests_this_second = 0;
            last_second = Instant::now();
        }

        // Send commands until we hit the pipeline limit
        while bytes_on_wire < config.pipeline_bytes {
            let Some(pid) = pid_selector.next(&config.fast_pids, &config.slow_pids) else {
                break;
            };

            // Send command with ` 1` for fast response: "01XX 1\r"
            let mut cmd_buf = [0u8; 8];
            let cmd = format_pid_command(pid, &mut cmd_buf);
            // Hot path: OBD2 commands are short strings, always fits u16
            #[allow(clippy::cast_possible_truncation)]
            let cmd_len = cmd.len() as u16;

            if let Err(e) = stream.write_all(cmd) {
                ctx.state.dongle_connected.store(false, Ordering::Relaxed);
                return Err(format!("Write error: {e}"));
            }

            pending_count += 1;
            bytes_on_wire += cmd_len;
        }

        // Read responses for pending commands
        if pending_count > 0 {
            let response_count =
                read_pipelined_responses(&mut stream, ctx.state, pending_count, config.timeout)?;
            ctx.state
                .metrics
                .total_requests
                .fetch_add(response_count, Ordering::Relaxed);
            requests_this_second += response_count;

            pending_count = 0;
            bytes_on_wire = 0;
        }
    }
}

/// Format a Mode 01 PID byte into a pipelined command: `"01XX 1\r"`.
///
/// Writes into `buf` and returns the filled slice.
fn format_pid_command(pid: u8, buf: &mut [u8; 8]) -> &[u8] {
    buf[0] = b'0';
    buf[1] = b'1';
    buf[2] = QueryBuilder::nibble_to_hex(pid >> 4);
    buf[3] = QueryBuilder::nibble_to_hex(pid & 0x0F);
    buf[4] = b' ';
    buf[5] = b'1';
    buf[6] = b'\r';
    &buf[..7]
}

/// Read responses for a batch of pipelined commands.
///
/// Returns the number of successful responses (prompt characters received).
fn read_pipelined_responses(
    stream: &mut TcpStream,
    state: &State,
    expected_count: usize,
    timeout: Duration,
) -> Result<u32, String> {
    let mut buffer = [0u8; 256];
    let mut response_buf = Vec::new();

    let start = Instant::now();
    // bytecount crate has no Xtensa support, would fall back to same loop
    #[allow(clippy::naive_bytecount)]
    while response_buf.iter().filter(|&&b| b == b'>').count() < expected_count {
        if start.elapsed() > timeout {
            // Hot path: expected_count is small, always fits u32
            #[allow(clippy::cast_possible_truncation)]
            let pending = expected_count as u32;
            state
                .metrics
                .total_errors
                .fetch_add(pending, Ordering::Relaxed);
            return Ok(0);
        }

        match stream.read(&mut buffer) {
            Ok(0) => {
                state.dongle_connected.store(false, Ordering::Relaxed);
                return Err("Disconnected".to_string());
            }
            Ok(n) => {
                response_buf.extend_from_slice(&buffer[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) => {
                state.dongle_connected.store(false, Ordering::Relaxed);
                return Err(format!("Read error: {e}"));
            }
        }
    }

    // bytecount crate has no Xtensa support, would fall back to same loop
    // Hot path: response count per batch, always fits u32
    #[allow(clippy::naive_bytecount, clippy::cast_possible_truncation)]
    let response_count = response_buf.iter().filter(|&&b| b == b'>').count() as u32;
    Ok(response_count)
}

/// Run capture test (mode 5) — pure TCP proxy with PSRAM recording.
///
/// The capture buffer lives in `state.capture_buffer` so the web server
/// can read it for download and clear it.
fn run_capture_test(ctx: &TestContext, config: &TestConfig) -> Result<(), String> {
    let capture = config.capture;
    info!(
        "Starting capture mode, listening on port {}...",
        config.listen_port
    );

    // Pre-allocate the shared capture buffer (large alloc → PSRAM via CONFIG_SPIRAM_USE_MALLOC)
    {
        let mut buf_guard = ctx.state.capture_buffer.lock().unwrap();
        buf_guard.clear();
        buf_guard.reserve(capture.buffer_size as usize);
    }

    let capture_start = Instant::now();

    // Start listening for client connections
    let listener = TcpListener::bind(format!("0.0.0.0:{}", config.listen_port))
        .map_err(|e| format!("Failed to bind listener: {e}"))?;
    listener.set_nonblocking(true).ok();

    info!("Listening for proxy clients on port {}", config.listen_port);

    let mut last_second = Instant::now();
    let mut bytes_this_second = 0u32;

    loop {
        ctx.watchdog.feed();

        if ctx.check_stop()? {
            return Ok(());
        }

        // Update bytes/sec
        if last_second.elapsed() >= Duration::from_secs(1) {
            ctx.state
                .metrics
                .requests_per_sec
                .store(bytes_this_second, Ordering::Relaxed);
            bytes_this_second = 0;
            last_second = Instant::now();

            // Update buffer usage
            let buf_len = ctx.state.capture_buffer.lock().unwrap().len() as u64;
            let usage_pct = u32::try_from(buf_len * 100 / u64::from(capture.buffer_size))
                .expect("percentage fits in u32");
            ctx.state
                .metrics
                .buffer_usage_pct
                .store(usage_pct, Ordering::Relaxed);
        }

        match listener.accept() {
            Ok((client_stream, client_addr)) => {
                let stopped = handle_capture_client(
                    ctx,
                    config,
                    client_stream,
                    client_addr,
                    capture_start,
                    &mut bytes_this_second,
                );
                if stopped {
                    return Ok(());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                warn!("Accept error: {e}");
            }
        }
    }
}

/// Handle a single client connection in capture mode.
///
/// Returns `true` if stop was requested, `false` if client disconnected normally.
fn handle_capture_client(
    ctx: &TestContext,
    config: &TestConfig,
    client_stream: TcpStream,
    client_addr: std::net::SocketAddr,
    capture_start: Instant,
    bytes_this_second: &mut u32,
) -> bool {
    let capture = config.capture;

    if ctx.state.metrics.client_connected.load(Ordering::Relaxed) {
        warn!("Rejecting connection from {client_addr}: already have a client");
        drop(client_stream);
        return false;
    }

    info!("Client connected from {client_addr}");
    ctx.state
        .metrics
        .client_connected
        .store(true, Ordering::Relaxed);

    // Record connect event
    record_event(
        ctx.state,
        capture_start.elapsed(),
        RecordType::Connect,
        &[],
        capture,
    );

    // Connect to dongle
    let dongle_addr = config.dongle_addr();
    let mut stop_requested = false;
    match TcpStream::connect(&dongle_addr) {
        Ok(dongle_stream) => {
            ctx.state.dongle_connected.store(true, Ordering::Relaxed);
            info!("Connected to dongle at {dongle_addr}");

            let result = proxy_loop(
                ctx,
                client_stream,
                dongle_stream,
                capture_start,
                capture,
                bytes_this_second,
            );

            ctx.state.dongle_connected.store(false, Ordering::Relaxed);

            match result {
                Ok(stopped) => stop_requested = stopped,
                Err(e) => warn!("Proxy loop ended: {e}"),
            }
        }
        Err(e) => {
            warn!("Failed to connect to dongle: {e}");
        }
    }

    // Record disconnect event
    record_event(
        ctx.state,
        capture_start.elapsed(),
        RecordType::Disconnect,
        &[],
        capture,
    );
    ctx.state
        .metrics
        .client_connected
        .store(false, Ordering::Relaxed);
    info!("Client disconnected");

    stop_requested
}

/// Record an event to the shared capture buffer in `State`.
///
/// Acquires the capture buffer lock briefly to append a single record.
fn record_event(
    state: &State,
    elapsed: Duration,
    record_type: RecordType,
    data: &[u8],
    capture: CaptureConfig,
) {
    let record_size = RECORD_HEADER_SIZE + data.len();

    let mut buf_guard = state.capture_buffer.lock().unwrap();

    // Check for overflow
    if buf_guard.len() + record_size > capture.buffer_size as usize {
        match capture.overflow {
            CaptureOverflow::Stop => {
                if !state.metrics.capture_overflow.load(Ordering::Relaxed) {
                    warn!("Capture buffer full, stopping capture");
                    state
                        .metrics
                        .capture_overflow
                        .store(true, Ordering::Relaxed);
                }
                return;
            }
            CaptureOverflow::Wrap => {
                // Remove oldest records to make room
                let buf_len = buf_guard.len();
                let to_remove = record_size.max(buf_len / 4).min(buf_len);
                buf_guard.drain(..to_remove);
            }
        }
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
        .metrics
        .records_captured
        .fetch_add(1, Ordering::Relaxed);
    // Hot path: capture buffer capped at 6 MB by config validation, fits u32
    #[allow(clippy::cast_possible_truncation)]
    let buf_len = buf_guard.len() as u32;
    state
        .metrics
        .bytes_captured
        .store(buf_len, Ordering::Relaxed);
}

/// Proxy loop between client and dongle
fn proxy_loop(
    ctx: &TestContext,
    mut client: TcpStream,
    mut dongle: TcpStream,
    capture_start: Instant,
    capture: CaptureConfig,
    bytes_this_second: &mut u32,
) -> Result<bool, String> {
    client.set_nonblocking(true).ok();
    dongle.set_nonblocking(true).ok();

    let mut client_buf = [0u8; 1024];
    let mut dongle_buf = [0u8; 1024];

    loop {
        ctx.watchdog.feed();

        if ctx.check_stop()? {
            return Ok(true);
        }

        let mut activity = false;

        // Read from client, forward to dongle
        match client.read(&mut client_buf) {
            Ok(0) => return Ok(false), // Client disconnected
            Ok(n) => {
                activity = true;
                let data = &client_buf[..n];

                // Record to capture buffer
                record_event(
                    ctx.state,
                    capture_start.elapsed(),
                    RecordType::ClientToDongle,
                    data,
                    capture,
                );

                // Forward to dongle
                if let Err(e) = dongle.write_all(data) {
                    return Err(format!("Dongle write error: {e}"));
                }

                // Hot path: n ≤ 1024 (read buffer size), always fits u32
                #[allow(clippy::cast_possible_truncation)]
                {
                    *bytes_this_second += n as u32;
                }
                ctx.state
                    .metrics
                    .total_requests
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(format!("Client read error: {e}")),
        }

        // Read from dongle, forward to client
        match dongle.read(&mut dongle_buf) {
            Ok(0) => return Err("Dongle disconnected".to_string()),
            Ok(n) => {
                activity = true;
                let data = &dongle_buf[..n];

                // Record to capture buffer
                record_event(
                    ctx.state,
                    capture_start.elapsed(),
                    RecordType::DongleToClient,
                    data,
                    capture,
                );

                // Forward to client
                if let Err(e) = client.write_all(data) {
                    return Err(format!("Client write error: {e}"));
                }

                // Hot path: n ≤ 1024 (read buffer size), always fits u32
                #[allow(clippy::cast_possible_truncation)]
                {
                    *bytes_this_second += n as u32;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(format!("Dongle read error: {e}")),
        }

        if !activity {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
