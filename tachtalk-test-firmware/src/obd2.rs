//! OBD2 test module for benchmarking query strategies.
//!
//! Modes:
//! 1. `NoCount`: Send PID as-is (baseline)
//! 2. `AlwaysOne`: Append ` 1` to all requests
//! 3. `AdaptiveCount`: Detect ECU count on first request, use that count
//! 4. `Pipelined`: Send multiple requests before waiting for responses
//! 5. `RawCapture`: Pure TCP proxy with traffic recording to PSRAM

use crate::config::{CaptureOverflow, QueryMode, ResponseCountMethod};
use crate::watchdog::WatchdogHandle;
use crate::{State, TestControlMessage};
use log::{debug, error, info, warn};
use smallvec::SmallVec;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tachtalk_capture_format::{CaptureHeader, RecordType, FLAG_OVERFLOW, HEADER_SIZE, RECORD_HEADER_SIZE};

/// Type alias for small OBD2 command/response buffers
pub type Obd2Buffer = SmallVec<[u8; 32]>;

/// Fixed fast:slow polling ratio
const FAST_SLOW_RATIO: u32 = 6;

/// Firmware version string for capture header
const FIRMWARE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Snapshot of test configuration taken at test start.
struct TestConfig {
    query_mode: QueryMode,
    dongle_ip: String,
    dongle_port: u16,
    timeout: Duration,
    fast_pids: Vec<String>,
    slow_pids: Vec<String>,
    pipeline_bytes: u16,
    response_count_method: ResponseCountMethod,
    listen_port: u16,
    capture: CaptureConfig,
}

impl TestConfig {
    /// Take a snapshot of the current test configuration.
    fn snapshot(state: &State) -> Self {
        let cfg_guard = state.config.lock().unwrap();
        Self {
            query_mode: cfg_guard.test.query_mode,
            dongle_ip: cfg_guard.test.dongle_ip.clone(),
            dongle_port: cfg_guard.test.dongle_port,
            timeout: Duration::from_millis(cfg_guard.test.obd2_timeout_ms),
            fast_pids: cfg_guard.test.get_fast_pids(),
            slow_pids: cfg_guard.test.get_slow_pids(),
            pipeline_bytes: cfg_guard.test.pipeline_bytes,
            response_count_method: cfg_guard.test.response_count_method,
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
struct PidSelector {
    fast_index: usize,
    slow_index: usize,
    fast_count: u32,
}

/// Result of selecting the next PID.
struct SelectedPid {
    pid: String,
    is_fast: bool,
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
    fn next(&mut self, fast_pids: &[String], slow_pids: &[String]) -> Option<SelectedPid> {
        if self.fast_count < FAST_SLOW_RATIO && !fast_pids.is_empty() {
            self.fast_count += 1;
            let pid = fast_pids[self.fast_index % fast_pids.len()].clone();
            self.fast_index += 1;
            Some(SelectedPid { pid, is_fast: true })
        } else if !slow_pids.is_empty() {
            self.fast_count = 0;
            let pid = slow_pids[self.slow_index % slow_pids.len()].clone();
            self.slow_index += 1;
            Some(SelectedPid { pid, is_fast: false })
        } else if !fast_pids.is_empty() {
            self.fast_count = 0;
            let pid = fast_pids[self.fast_index % fast_pids.len()].clone();
            self.fast_index += 1;
            Some(SelectedPid { pid, is_fast: true })
        } else {
            None
        }
    }
}

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
            Ok(TestControlMessage::Start) => {
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

/// Connect to the dongle, configure socket options, and run AT init commands.
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

/// Execute a command on the dongle and return the response
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

/// Initialize dongle connection with standard AT commands
fn init_dongle(stream: &mut TcpStream, timeout: Duration) -> Result<(), String> {
    // Reset
    execute_command(stream, b"ATZ", timeout)?;
    // Echo off
    execute_command(stream, b"ATE0", timeout)?;
    // Spaces off
    execute_command(stream, b"ATS0", timeout)?;
    // Linefeeds off
    execute_command(stream, b"ATL0", timeout)?;
    // Headers off
    execute_command(stream, b"ATH0", timeout)?;

    Ok(())
}

/// Count responses in a dongle response using the configured method
fn count_responses(response: &[u8], method: ResponseCountMethod) -> u8 {
    let response_str = String::from_utf8_lossy(response);

    let count = match method {
        ResponseCountMethod::CountResponseHeaders => {
            // Count occurrences of "41" (Mode 01 response header)
            response_str.matches("41").count()
        }
        ResponseCountMethod::CountLines => {
            // Count non-empty lines before ">"
            response_str
                .lines()
                .filter(|line| {
                    let trimmed = line.trim();
                    !trimmed.is_empty() && !trimmed.contains('>')
                })
                .count()
        }
    };
    u8::try_from(count).expect("response count fits in u8")
}

/// Main test task — handles all query modes
pub fn test_task(state: &Arc<State>, control_rx: &std::sync::mpsc::Receiver<TestControlMessage>) {
    let watchdog = WatchdogHandle::register(c"test_task");
    let ctx = TestContext { state, control_rx, watchdog: &watchdog };

    info!("Test task started, waiting for commands...");

    loop {
        watchdog.feed();

        // Wait for start command
        match control_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(TestControlMessage::Start) => {
                info!("Test start command received");
                run_test(&ctx);
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

/// Run a test with the current configuration
fn run_test(ctx: &TestContext) {
    let config = TestConfig::snapshot(ctx.state);

    // Reset metrics
    ctx.state.metrics.reset();
    ctx.state.metrics.test_running.store(true, Ordering::Relaxed);

    info!("Starting test with mode {:?}", config.query_mode);
    info!("Fast PIDs: {:?}, Slow PIDs: {:?}", config.fast_pids, config.slow_pids);

    let result = match config.query_mode {
        QueryMode::NoCount | QueryMode::AlwaysOne | QueryMode::AdaptiveCount => {
            run_polling_test(ctx, &config)
        }
        QueryMode::Pipelined => {
            run_pipelined_test(ctx, &config)
        }
        QueryMode::RawCapture => {
            run_capture_test(ctx, &config)
        }
    };

    ctx.state.metrics.test_running.store(false, Ordering::Relaxed);
    ctx.state.dongle_connected.store(false, Ordering::Relaxed);

    match result {
        Ok(()) => info!("Test completed normally"),
        Err(e) => warn!("Test ended: {e}"),
    }
}

/// Run polling test (modes 1-3)
fn run_polling_test(
    ctx: &TestContext,
    config: &TestConfig,
) -> Result<(), String> {
    let mut stream = connect_dongle(config)?;
    ctx.state.dongle_connected.store(true, Ordering::Relaxed);

    // For `AdaptiveCount` mode, track response counts per PID
    let mut pid_response_counts: std::collections::HashMap<String, u8> =
        std::collections::HashMap::new();

    let mut pid_selector = PidSelector::new();
    let mut last_second = Instant::now();
    let mut requests_this_second = 0u32;

    loop {
        ctx.watchdog.feed();

        if ctx.check_stop()? {
            return Ok(());
        }

        // Update requests/sec metric
        if last_second.elapsed() >= Duration::from_secs(1) {
            ctx.state
                .metrics
                .requests_per_sec
                .store(requests_this_second, Ordering::Relaxed);
            requests_this_second = 0;
            last_second = Instant::now();
        }

        // Select PID to poll (6:1 fast:slow ratio)
        let Some(selected) = pid_selector.next(&config.fast_pids, &config.slow_pids) else {
            // No PIDs configured, sleep and continue
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };

        // Build command based on mode
        let command = match config.query_mode {
            QueryMode::NoCount => selected.pid.clone(),
            QueryMode::AlwaysOne => format!("{} 1", selected.pid),
            QueryMode::AdaptiveCount => {
                if let Some(&count) = pid_response_counts.get(&selected.pid) {
                    format!("{} {count}", selected.pid)
                } else {
                    // First request for this PID — send without count to detect
                    selected.pid.clone()
                }
            }
            _ => unreachable!(),
        };

        // Execute command
        let result = execute_command(&mut stream, command.as_bytes(), config.timeout);

        match result {
            Ok(response) => {
                // For `AdaptiveCount`, learn the response count on first request
                if config.query_mode == QueryMode::AdaptiveCount
                    && !pid_response_counts.contains_key(&selected.pid)
                {
                    let count = count_responses(&response, config.response_count_method);
                    if count > 0 {
                        info!("Learned response count for {}: {count}", selected.pid);
                        pid_response_counts.insert(selected.pid.clone(), count);
                    }
                }

                ctx.state
                    .metrics
                    .total_requests
                    .fetch_add(1, Ordering::Relaxed);
                requests_this_second += 1;

                debug!("Got response for {} (fast={})", selected.pid, selected.is_fast);
            }
            Err(e) => {
                warn!("Request failed for {}: {e}", selected.pid);
                ctx.state.metrics.total_errors.fetch_add(1, Ordering::Relaxed);

                // On disconnect, try to reconnect
                if e.contains("Disconnect") {
                    ctx.state.dongle_connected.store(false, Ordering::Relaxed);
                    return Err(e);
                }
            }
        }
    }
}

/// Run pipelined test (mode 4)
fn run_pipelined_test(
    ctx: &TestContext,
    config: &TestConfig,
) -> Result<(), String> {
    let mut stream = connect_dongle(config)?;
    ctx.state.dongle_connected.store(true, Ordering::Relaxed);

    let mut pid_selector = PidSelector::new();
    let mut last_second = Instant::now();
    let mut requests_this_second = 0u32;

    // Pipeline state
    let mut pending_commands: Vec<String> = Vec::new();
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
            let Some(selected) = pid_selector.next(&config.fast_pids, &config.slow_pids) else {
                break;
            };

            // Send command with ` 1` for fast response
            let command = format!("{} 1\r", selected.pid);
            // Hot path: OBD2 commands are short strings, always fits u16
            #[allow(clippy::cast_possible_truncation)]
            let cmd_len = command.len() as u16;

            if let Err(e) = stream.write_all(command.as_bytes()) {
                ctx.state.dongle_connected.store(false, Ordering::Relaxed);
                return Err(format!("Write error: {e}"));
            }

            pending_commands.push(selected.pid);
            bytes_on_wire += cmd_len;
        }

        // Read responses for pending commands
        if !pending_commands.is_empty() {
            let response_count = read_pipelined_responses(
                &mut stream,
                ctx.state,
                &pending_commands,
                config.timeout,
            )?;
            ctx.state
                .metrics
                .total_requests
                .fetch_add(response_count, Ordering::Relaxed);
            requests_this_second += response_count;

            pending_commands.clear();
            bytes_on_wire = 0;
        }
    }
}

/// Read responses for a batch of pipelined commands.
///
/// Returns the number of successful responses (prompt characters received).
fn read_pipelined_responses(
    stream: &mut TcpStream,
    state: &State,
    pending_commands: &[String],
    timeout: Duration,
) -> Result<u32, String> {
    let mut buffer = [0u8; 256];
    let mut response_buf = Vec::new();

    let start = Instant::now();
    // bytecount crate has no Xtensa support, would fall back to same loop
    #[allow(clippy::naive_bytecount)]
    while response_buf.iter().filter(|&&b| b == b'>').count() < pending_commands.len() {
        if start.elapsed() > timeout {
            // Hot path: pending_commands is small, always fits u32
            #[allow(clippy::cast_possible_truncation)]
            let pending = pending_commands.len() as u32;
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
fn run_capture_test(
    ctx: &TestContext,
    config: &TestConfig,
) -> Result<(), String> {
    let capture = config.capture;
    info!("Starting capture mode, listening on port {}...", config.listen_port);

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
                handle_capture_client(
                    ctx,
                    config,
                    client_stream,
                    client_addr,
                    capture_start,
                    &mut bytes_this_second,
                );
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
fn handle_capture_client(
    ctx: &TestContext,
    config: &TestConfig,
    client_stream: TcpStream,
    client_addr: std::net::SocketAddr,
    capture_start: Instant,
    bytes_this_second: &mut u32,
) {
    let capture = config.capture;

    if ctx.state.metrics.client_connected.load(Ordering::Relaxed) {
        warn!("Rejecting connection from {client_addr}: already have a client");
        drop(client_stream);
        return;
    }

    info!("Client connected from {client_addr}");
    ctx.state
        .metrics
        .client_connected
        .store(true, Ordering::Relaxed);

    // Record connect event
    record_event(ctx.state, capture_start.elapsed(), RecordType::Connect, &[], capture);

    // Connect to dongle
    let dongle_addr = config.dongle_addr();
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

            if let Err(e) = result {
                warn!("Proxy loop ended: {e}");
            }
        }
        Err(e) => {
            warn!("Failed to connect to dongle: {e}");
        }
    }

    // Record disconnect event
    record_event(ctx.state, capture_start.elapsed(), RecordType::Disconnect, &[], capture);
    ctx.state
        .metrics
        .client_connected
        .store(false, Ordering::Relaxed);
    info!("Client disconnected");
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
    state.metrics.bytes_captured.store(buf_len, Ordering::Relaxed);
}

/// Proxy loop between client and dongle
fn proxy_loop(
    ctx: &TestContext,
    mut client: TcpStream,
    mut dongle: TcpStream,
    capture_start: Instant,
    capture: CaptureConfig,
    bytes_this_second: &mut u32,
) -> Result<(), String> {
    client.set_nonblocking(true).ok();
    dongle.set_nonblocking(true).ok();

    let mut client_buf = [0u8; 1024];
    let mut dongle_buf = [0u8; 1024];

    loop {
        ctx.watchdog.feed();

        if ctx.check_stop()? {
            return Ok(());
        }

        let mut activity = false;

        // Read from client, forward to dongle
        match client.read(&mut client_buf) {
            Ok(0) => return Ok(()), // Client disconnected
            Ok(n) => {
                activity = true;
                let data = &client_buf[..n];

                // Record to capture buffer
                record_event(ctx.state, capture_start.elapsed(), RecordType::ClientToDongle, data, capture);

                // Forward to dongle
                if let Err(e) = dongle.write_all(data) {
                    return Err(format!("Dongle write error: {e}"));
                }

                // Hot path: n ≤ 1024 (read buffer size), always fits u32
                #[allow(clippy::cast_possible_truncation)]
                { *bytes_this_second += n as u32; }
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
                record_event(ctx.state, capture_start.elapsed(), RecordType::DongleToClient, data, capture);

                // Forward to client
                if let Err(e) = client.write_all(data) {
                    return Err(format!("Client write error: {e}"));
                }

                // Hot path: n ≤ 1024 (read buffer size), always fits u32
                #[allow(clippy::cast_possible_truncation)]
                { *bytes_this_second += n as u32; }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(format!("Dongle read error: {e}")),
        }

        if !activity {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
