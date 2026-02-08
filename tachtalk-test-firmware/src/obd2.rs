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

/// Build a [`CaptureHeader`] from the current device state.
#[allow(clippy::cast_possible_truncation)]
pub fn build_capture_header(state: &State) -> [u8; HEADER_SIZE] {
    let mut header = CaptureHeader::new();

    header.record_count = state.metrics.records_captured.load(Ordering::Relaxed);
    header.data_length = state.metrics.bytes_captured.load(Ordering::Relaxed);
    header.uptime_ms = state.capture_start_uptime_ms.load(Ordering::Relaxed);

    // Dongle IP and port from config
    {
        let cfg_guard = state.config.lock().unwrap();
        let ip: std::net::Ipv4Addr = cfg_guard
            .test
            .dongle_ip
            .parse()
            .unwrap_or(std::net::Ipv4Addr::UNSPECIFIED);
        header.dongle_ip = ip.octets();
        header.dongle_port = cfg_guard.test.dongle_port;
    }

    if state.metrics.capture_overflow.load(Ordering::Relaxed) {
        header.flags |= FLAG_OVERFLOW;
    }

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
#[allow(clippy::cast_possible_truncation)]
fn count_responses(response: &[u8], method: ResponseCountMethod) -> u8 {
    let response_str = String::from_utf8_lossy(response);

    match method {
        ResponseCountMethod::CountResponseHeaders => {
            // Count occurrences of "41" (Mode 01 response header)
            response_str.matches("41").count() as u8
        }
        ResponseCountMethod::CountLines => {
            // Count non-empty lines before ">"
            response_str
                .lines()
                .filter(|line| {
                    let trimmed = line.trim();
                    !trimmed.is_empty() && !trimmed.contains('>')
                })
                .count() as u8
        }
    }
}

/// Main test task — handles all query modes
pub fn test_task(state: &Arc<State>, control_rx: &std::sync::mpsc::Receiver<TestControlMessage>) {
    let watchdog = WatchdogHandle::register("test_task");

    info!("Test task started, waiting for commands...");

    loop {
        watchdog.feed();

        // Wait for start command
        match control_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(TestControlMessage::Start) => {
                info!("Test start command received");
                run_test(state, control_rx, &watchdog);
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
fn run_test(
    state: &Arc<State>,
    control_rx: &std::sync::mpsc::Receiver<TestControlMessage>,
    watchdog: &WatchdogHandle,
) {
    // Get config snapshot
    let (query_mode, dongle_ip, dongle_port, timeout, fast_pids, slow_pids, pipeline_bytes, response_count_method, listen_port, capture_buffer_size, capture_overflow) = {
        let cfg_guard = state.config.lock().unwrap();
        (
            cfg_guard.test.query_mode,
            cfg_guard.test.dongle_ip.clone(),
            cfg_guard.test.dongle_port,
            Duration::from_millis(cfg_guard.test.obd2_timeout_ms),
            cfg_guard.test.get_fast_pids(),
            cfg_guard.test.get_slow_pids(),
            cfg_guard.test.pipeline_bytes,
            cfg_guard.test.response_count_method,
            cfg_guard.test.listen_port,
            cfg_guard.test.capture_buffer_size,
            cfg_guard.test.capture_overflow,
        )
    };

    // Reset metrics
    state.metrics.reset();
    state.metrics.test_running.store(true, Ordering::Relaxed);

    // Record uptime at start (ms since boot)
    // Use esp_timer_get_time for absolute uptime since boot
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let uptime_ms = unsafe { esp_idf_svc::sys::esp_timer_get_time() / 1000 } as u32;
    state
        .capture_start_uptime_ms
        .store(uptime_ms, Ordering::Relaxed);

    info!("Starting test with mode {query_mode:?}");
    info!("Fast PIDs: {fast_pids:?}, Slow PIDs: {slow_pids:?}");

    let result = match query_mode {
        QueryMode::NoCount | QueryMode::AlwaysOne | QueryMode::AdaptiveCount => {
            run_polling_test(
                state,
                control_rx,
                watchdog,
                query_mode,
                &dongle_ip,
                dongle_port,
                timeout,
                &fast_pids,
                &slow_pids,
                response_count_method,
            )
        }
        QueryMode::Pipelined => run_pipelined_test(
            state,
            control_rx,
            watchdog,
            &dongle_ip,
            dongle_port,
            timeout,
            &fast_pids,
            &slow_pids,
            pipeline_bytes,
        ),
        QueryMode::RawCapture => run_capture_test(
            state,
            control_rx,
            watchdog,
            &dongle_ip,
            dongle_port,
            listen_port,
            capture_buffer_size,
            capture_overflow,
        ),
    };

    state.metrics.test_running.store(false, Ordering::Relaxed);
    state.dongle_connected.store(false, Ordering::Relaxed);

    match result {
        Ok(()) => info!("Test completed normally"),
        Err(e) => warn!("Test ended: {e}"),
    }
}

/// Run polling test (modes 1-3)
#[allow(clippy::too_many_arguments)]
fn run_polling_test(
    state: &Arc<State>,
    control_rx: &std::sync::mpsc::Receiver<TestControlMessage>,
    watchdog: &WatchdogHandle,
    query_mode: QueryMode,
    dongle_ip: &str,
    dongle_port: u16,
    timeout: Duration,
    fast_pids: &[String],
    slow_pids: &[String],
    response_count_method: ResponseCountMethod,
) -> Result<(), String> {
    // Connect to dongle
    let addr = format!("{dongle_ip}:{dongle_port}");
    info!("Connecting to dongle at {addr}...");

    let mut stream = TcpStream::connect(&addr).map_err(|e| format!("Connect failed: {e}"))?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_nodelay(true).ok();

    state.dongle_connected.store(true, Ordering::Relaxed);
    info!("Connected to dongle");

    // Initialize dongle
    init_dongle(&mut stream, timeout)?;
    info!("Dongle initialized");

    // For `AdaptiveCount` mode, track response counts per PID
    let mut pid_response_counts: std::collections::HashMap<String, u8> =
        std::collections::HashMap::new();

    let mut fast_index = 0usize;
    let mut slow_index = 0usize;
    let mut fast_count = 0u32;

    let mut last_second = Instant::now();
    let mut requests_this_second = 0u32;

    loop {
        watchdog.feed();

        // Check for stop command (non-blocking)
        match control_rx.try_recv() {
            Ok(TestControlMessage::Stop) => {
                info!("Stop command received");
                return Ok(());
            }
            Ok(TestControlMessage::Start) => {
                debug!("Ignoring start command while running");
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return Err("Control channel disconnected".to_string());
            }
        }

        // Update requests/sec metric
        if last_second.elapsed() >= Duration::from_secs(1) {
            state
                .metrics
                .requests_per_sec
                .store(requests_this_second, Ordering::Relaxed);
            requests_this_second = 0;
            last_second = Instant::now();
        }

        // Select PID to poll (6:1 fast:slow ratio)
        let (pid, is_fast) = if fast_count < FAST_SLOW_RATIO && !fast_pids.is_empty() {
            fast_count += 1;
            let pid = &fast_pids[fast_index % fast_pids.len()];
            fast_index += 1;
            (pid.clone(), true)
        } else if !slow_pids.is_empty() {
            fast_count = 0;
            let pid = &slow_pids[slow_index % slow_pids.len()];
            slow_index += 1;
            (pid.clone(), false)
        } else if !fast_pids.is_empty() {
            fast_count = 0;
            let pid = &fast_pids[fast_index % fast_pids.len()];
            fast_index += 1;
            (pid.clone(), true)
        } else {
            // No PIDs configured, sleep and continue
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };

        // Build command based on mode
        let command = match query_mode {
            QueryMode::NoCount => pid.clone(),
            QueryMode::AlwaysOne => format!("{pid} 1"),
            QueryMode::AdaptiveCount => {
                if let Some(&count) = pid_response_counts.get(&pid) {
                    format!("{pid} {count}")
                } else {
                    // First request for this PID — send without count to detect
                    pid.clone()
                }
            }
            _ => unreachable!(),
        };

        // Execute command
        let result = execute_command(&mut stream, command.as_bytes(), timeout);

        match result {
            Ok(response) => {
                // For `AdaptiveCount`, learn the response count on first request
                if query_mode == QueryMode::AdaptiveCount
                    && !pid_response_counts.contains_key(&pid)
                {
                    let count = count_responses(&response, response_count_method);
                    if count > 0 {
                        info!("Learned response count for {pid}: {count}");
                        pid_response_counts.insert(pid.clone(), count);
                    }
                }

                state
                    .metrics
                    .total_requests
                    .fetch_add(1, Ordering::Relaxed);
                requests_this_second += 1;

                debug!("Got response for {pid} (fast={is_fast})");
            }
            Err(e) => {
                warn!("Request failed for {pid}: {e}");
                state.metrics.total_errors.fetch_add(1, Ordering::Relaxed);

                // On disconnect, try to reconnect
                if e.contains("Disconnect") {
                    state.dongle_connected.store(false, Ordering::Relaxed);
                    return Err(e);
                }
            }
        }
    }
}

/// Run pipelined test (mode 4)
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
fn run_pipelined_test(
    state: &Arc<State>,
    control_rx: &std::sync::mpsc::Receiver<TestControlMessage>,
    watchdog: &WatchdogHandle,
    dongle_ip: &str,
    dongle_port: u16,
    timeout: Duration,
    fast_pids: &[String],
    slow_pids: &[String],
    pipeline_bytes: u16,
) -> Result<(), String> {
    // Connect to dongle
    let addr = format!("{dongle_ip}:{dongle_port}");
    info!("Connecting to dongle at {addr}...");

    let mut stream = TcpStream::connect(&addr).map_err(|e| format!("Connect failed: {e}"))?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_nodelay(true).ok();

    state.dongle_connected.store(true, Ordering::Relaxed);
    info!("Connected to dongle");

    // Initialize dongle
    init_dongle(&mut stream, timeout)?;
    info!("Dongle initialized");

    let mut fast_index = 0usize;
    let mut slow_index = 0usize;
    let mut fast_count = 0u32;

    let mut last_second = Instant::now();
    let mut requests_this_second = 0u32;

    // Pipeline state
    let mut pending_commands: Vec<String> = Vec::new();
    let mut bytes_on_wire = 0u16;

    loop {
        watchdog.feed();

        // Check for stop command
        match control_rx.try_recv() {
            Ok(TestControlMessage::Stop) => {
                info!("Stop command received");
                return Ok(());
            }
            Ok(TestControlMessage::Start) | Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return Err("Control channel disconnected".to_string());
            }
        }

        // Update requests/sec
        if last_second.elapsed() >= Duration::from_secs(1) {
            state
                .metrics
                .requests_per_sec
                .store(requests_this_second, Ordering::Relaxed);
            requests_this_second = 0;
            last_second = Instant::now();
        }

        // Send commands until we hit the pipeline limit
        while bytes_on_wire < pipeline_bytes {
            // Select next PID
            let pid = if fast_count < FAST_SLOW_RATIO && !fast_pids.is_empty() {
                fast_count += 1;
                let pid = &fast_pids[fast_index % fast_pids.len()];
                fast_index += 1;
                pid.clone()
            } else if !slow_pids.is_empty() {
                fast_count = 0;
                let pid = &slow_pids[slow_index % slow_pids.len()];
                slow_index += 1;
                pid.clone()
            } else if !fast_pids.is_empty() {
                fast_count = 0;
                let pid = &fast_pids[fast_index % fast_pids.len()];
                fast_index += 1;
                pid.clone()
            } else {
                break;
            };

            // Send command with ` 1` for fast response
            let command = format!("{pid} 1\r");
            #[allow(clippy::cast_possible_truncation)]
            let cmd_len = command.len() as u16;

            if let Err(e) = stream.write_all(command.as_bytes()) {
                state.dongle_connected.store(false, Ordering::Relaxed);
                return Err(format!("Write error: {e}"));
            }

            pending_commands.push(pid);
            bytes_on_wire += cmd_len;
        }

        // Read responses for pending commands
        if !pending_commands.is_empty() {
            let mut buffer = [0u8; 256];
            let mut response_buf = Vec::new();

            // Read until we have responses for all pending commands
            let start = Instant::now();
            #[allow(clippy::naive_bytecount)]
            while response_buf.iter().filter(|&&b| b == b'>').count() < pending_commands.len() {
                if start.elapsed() > timeout {
                    #[allow(clippy::cast_possible_truncation)]
                    let pending = pending_commands.len() as u32;
                    state.metrics.total_errors.fetch_add(
                        pending,
                        Ordering::Relaxed,
                    );
                    pending_commands.clear();
                    break;
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

            // Count successful responses
            #[allow(clippy::naive_bytecount, clippy::cast_possible_truncation)]
            let response_count = response_buf.iter().filter(|&&b| b == b'>').count() as u32;
            state
                .metrics
                .total_requests
                .fetch_add(response_count, Ordering::Relaxed);
            requests_this_second += response_count;

            pending_commands.clear();
            bytes_on_wire = 0;
        }
    }
}

/// Run capture test (mode 5) — pure TCP proxy with PSRAM recording.
///
/// The capture buffer lives in `state.capture_buffer` so the web server
/// can read it for download and clear it.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
fn run_capture_test(
    state: &Arc<State>,
    control_rx: &std::sync::mpsc::Receiver<TestControlMessage>,
    watchdog: &WatchdogHandle,
    dongle_ip: &str,
    dongle_port: u16,
    listen_port: u16,
    capture_buffer_size: u32,
    capture_overflow: CaptureOverflow,
) -> Result<(), String> {
    info!("Starting capture mode, listening on port {listen_port}...");

    // Pre-allocate the shared capture buffer (large alloc → PSRAM via CONFIG_SPIRAM_USE_MALLOC)
    {
        let mut buf_guard = state.capture_buffer.lock().unwrap();
        buf_guard.clear();
        buf_guard.reserve(capture_buffer_size as usize);
    }

    let capture_start = Instant::now();

    // Start listening for client connections
    let listener = TcpListener::bind(format!("0.0.0.0:{listen_port}"))
        .map_err(|e| format!("Failed to bind listener: {e}"))?;
    listener.set_nonblocking(true).ok();

    info!("Listening for proxy clients on port {listen_port}");

    let mut last_second = Instant::now();
    let mut bytes_this_second = 0u32;

    loop {
        watchdog.feed();

        // Check for stop command
        match control_rx.try_recv() {
            Ok(TestControlMessage::Stop) => {
                info!("Stop command received");
                return Ok(());
            }
            Ok(TestControlMessage::Start) | Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return Err("Control channel disconnected".to_string());
            }
        }

        // Update bytes/sec
        if last_second.elapsed() >= Duration::from_secs(1) {
            state
                .metrics
                .requests_per_sec
                .store(bytes_this_second, Ordering::Relaxed);
            bytes_this_second = 0;
            last_second = Instant::now();

            // Update buffer usage
            let buf_len = state.capture_buffer.lock().unwrap().len() as u64;
            #[allow(clippy::cast_possible_truncation)]
            let usage_pct = (buf_len * 100 / u64::from(capture_buffer_size)) as u32;
            state
                .metrics
                .buffer_usage_pct
                .store(usage_pct, Ordering::Relaxed);
        }

        // Accept client connection
        match listener.accept() {
            Ok((client_stream, client_addr)) => {
                if state.metrics.client_connected.load(Ordering::Relaxed) {
                    // Already have a client, reject
                    warn!("Rejecting connection from {client_addr}: already have a client");
                    drop(client_stream);
                    continue;
                }

                info!("Client connected from {client_addr}");
                state
                    .metrics
                    .client_connected
                    .store(true, Ordering::Relaxed);

                // Record connect event
                record_event(
                    state,
                    capture_start.elapsed(),
                    RecordType::Connect,
                    &[],
                    capture_buffer_size,
                    capture_overflow,
                );

                // Connect to dongle
                let dongle_addr = format!("{dongle_ip}:{dongle_port}");
                match TcpStream::connect(&dongle_addr) {
                    Ok(dongle_stream) => {
                        state.dongle_connected.store(true, Ordering::Relaxed);
                        info!("Connected to dongle at {dongle_addr}");

                        // Run proxy loop
                        let result = proxy_loop(
                            state,
                            control_rx,
                            watchdog,
                            client_stream,
                            dongle_stream,
                            capture_start,
                            capture_buffer_size,
                            capture_overflow,
                            &mut bytes_this_second,
                        );

                        state.dongle_connected.store(false, Ordering::Relaxed);

                        if let Err(e) = result {
                            warn!("Proxy loop ended: {e}");
                        }
                    }
                    Err(e) => {
                        warn!("Failed to connect to dongle: {e}");
                    }
                }

                // Record disconnect event
                record_event(
                    state,
                    capture_start.elapsed(),
                    RecordType::Disconnect,
                    &[],
                    capture_buffer_size,
                    capture_overflow,
                );
                state
                    .metrics
                    .client_connected
                    .store(false, Ordering::Relaxed);
                info!("Client disconnected");
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

/// Record an event to the shared capture buffer in `State`.
///
/// Acquires the capture buffer lock briefly to append a single record.
fn record_event(
    state: &State,
    elapsed: Duration,
    record_type: RecordType,
    data: &[u8],
    max_size: u32,
    overflow: CaptureOverflow,
) {
    let record_size = RECORD_HEADER_SIZE + data.len();

    let mut buf_guard = state.capture_buffer.lock().unwrap();

    // Check for overflow
    if buf_guard.len() + record_size > max_size as usize {
        match overflow {
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
    #[allow(clippy::cast_possible_truncation)]
    let timestamp_ms = elapsed.as_millis() as u32;
    buf_guard.extend_from_slice(&timestamp_ms.to_le_bytes());
    buf_guard.push(record_type as u8);
    #[allow(clippy::cast_possible_truncation)]
    let len = data.len() as u16;
    buf_guard.extend_from_slice(&len.to_le_bytes());
    buf_guard.extend_from_slice(data);

    state
        .metrics
        .records_captured
        .fetch_add(1, Ordering::Relaxed);
    #[allow(clippy::cast_possible_truncation)]
    let buf_len = buf_guard.len() as u32;
    state.metrics.bytes_captured.store(buf_len, Ordering::Relaxed);
}

/// Proxy loop between client and dongle
#[allow(clippy::too_many_arguments)]
fn proxy_loop(
    state: &State,
    control_rx: &std::sync::mpsc::Receiver<TestControlMessage>,
    watchdog: &WatchdogHandle,
    mut client: TcpStream,
    mut dongle: TcpStream,
    capture_start: Instant,
    capture_buffer_size: u32,
    capture_overflow: CaptureOverflow,
    bytes_this_second: &mut u32,
) -> Result<(), String> {
    client.set_nonblocking(true).ok();
    dongle.set_nonblocking(true).ok();

    let mut client_buf = [0u8; 1024];
    let mut dongle_buf = [0u8; 1024];

    loop {
        watchdog.feed();

        // Check for stop command
        match control_rx.try_recv() {
            Ok(TestControlMessage::Stop) => {
                return Ok(());
            }
            Ok(TestControlMessage::Start) | Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return Err("Control channel disconnected".to_string());
            }
        }

        let mut activity = false;

        // Read from client, forward to dongle
        match client.read(&mut client_buf) {
            Ok(0) => return Ok(()), // Client disconnected
            Ok(n) => {
                activity = true;
                let data = &client_buf[..n];

                // Record to capture buffer
                record_event(
                    state,
                    capture_start.elapsed(),
                    RecordType::ClientToDongle,
                    data,
                    capture_buffer_size,
                    capture_overflow,
                );

                // Forward to dongle
                if let Err(e) = dongle.write_all(data) {
                    return Err(format!("Dongle write error: {e}"));
                }

                #[allow(clippy::cast_possible_truncation)]
                { *bytes_this_second += n as u32; }
                state
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
                    state,
                    capture_start.elapsed(),
                    RecordType::DongleToClient,
                    data,
                    capture_buffer_size,
                    capture_overflow,
                );

                // Forward to client
                if let Err(e) = client.write_all(data) {
                    return Err(format!("Client write error: {e}"));
                }

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
