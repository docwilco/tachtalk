//! OBD2 proxy with dedicated dongle task.
//!
//! Architecture:
//! - Dongle task: owns the single TCP connection to the OBD2 dongle, handles
//!   connection setup, reconnection, and processes OBD2 data commands
//! - Proxy server: accepts client connections, forwards OBD2 commands via channel
//! - AT commands (ATE0, ATZ, etc.) are handled locally per connection

use anyhow::{Context, Result};
use log::{debug, error, info, warn};
use smallvec::SmallVec;
use std::collections::HashSet;
use std::io::{BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::leds::LedController;
use crate::watchdog::WatchdogHandle;
use crate::sse_server::{SseMessage, SseSender};

const IDLE_POLL_INTERVAL_MS: u64 = 100;
/// How long after a client RPM update before background poller resumes
const CLIENT_ACTIVITY_BACKOFF: Duration = Duration::from_millis(500);
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// Shared log of unique AT commands received from clients (for debugging)
pub type AtCommandLog = Arc<Mutex<HashSet<String>>>;

/// Shared log of unique OBD2 PIDs requested by clients (for debugging)
pub type PidLog = Arc<Mutex<HashSet<String>>>;

/// Channel sender for RPM updates to the LED task
pub type RpmSender = Sender<u32>;

/// Type alias for small OBD2 command/response buffers
pub type Obd2Buffer = SmallVec<[u8; 12]>;

/// Request to the dongle task
pub struct DongleRequest {
    /// The OBD2 command to send (without terminator)
    pub command: Obd2Buffer,
    /// Channel to send the response back
    pub response_tx: oneshot::Sender<Result<Obd2Buffer, DongleError>>,
}

/// Per-connection client state (ELM327 settings)
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)] // These are independent ELM327 protocol flags
struct ClientState {
    /// Echo received characters back (ATE0/ATE1)
    echo_enabled: bool,
    /// Add linefeeds after carriage returns (ATL0/ATL1)
    linefeeds_enabled: bool,
    /// Print spaces between response bytes (ATS0/ATS1)
    spaces_enabled: bool,
    /// Show header bytes in responses (ATH0/ATH1)
    // TODO: Actually use this when formatting OBD2 responses
    headers_enabled: bool,
}

/// Shared context for processing OBD2 commands (reduces parameter count)
struct CommandContext<'a> {
    config: &'a Arc<Mutex<Config>>,
    rpm_tx: &'a RpmSender,
    dongle_tx: &'a DongleSender,
    at_command_log: &'a AtCommandLog,
    pid_log: &'a PidLog,
}

impl Default for ClientState {
    fn default() -> Self {
        Self {
            echo_enabled: true,
            linefeeds_enabled: true,
            spaces_enabled: true,
            headers_enabled: false,
        }
    }
}

impl ClientState {
    /// Format a line ending based on current settings
    fn line_ending(&self) -> &'static str {
        if self.linefeeds_enabled { "\r\n" } else { "\r" }
    }

    /// Format a dongle response according to client settings
    /// The dongle sends compact hex (no spaces), so we add spaces if enabled
    fn format_response(&self, response: &[u8]) -> Vec<u8> {
        if !self.spaces_enabled {
            // No formatting needed, return as-is
            return response.to_vec();
        }

        let mut result = Vec::with_capacity(response.len() * 3 / 2);
        let mut hex_count = 0;

        for &byte in response {
            // Check if this is a hex digit
            let is_hex = byte.is_ascii_hexdigit();

            if is_hex {
                // Add space before every pair of hex digits (except the first)
                if hex_count > 0 && hex_count % 2 == 0 {
                    result.push(b' ');
                }
                hex_count += 1;
            } else {
                // Reset hex count on non-hex (line endings, prompt, etc.)
                hex_count = 0;
            }

            result.push(byte);
        }

        result
    }
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
    fn to_elm327_error(&self) -> &'static str {
        match self {
            Self::NotConnected => "UNABLE TO CONNECT",
            Self::Timeout => "NO DATA",
            Self::Disconnected => "CAN ERROR",
            Self::IoError(_) => "BUS ERROR",
        }
    }
}

pub type DongleSender = Sender<DongleRequest>;

/// Start the dongle task and return a sender for requests
pub fn start_dongle_task(config: Arc<Mutex<Config>>) -> DongleSender {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        dongle_task(&rx, &config);
    });
    tx
}

/// The dongle task - owns the connection and processes requests
fn dongle_task(rx: &Receiver<DongleRequest>, config: &Arc<Mutex<Config>>) {
    info!("OBD2 dongle task starting...");
    
    let mut connection: Option<TcpStream> = None;
    let mut last_connect_attempt: Option<Instant> = None;
    
    let watchdog = WatchdogHandle::register("obd2_dongle");
    
    info!("OBD2 dongle task started");

    loop {
        watchdog.feed();
        
        // Get config values
        let (timeout, dongle_ip, dongle_port) = {
            let cfg = config.lock().unwrap();
            (
                Duration::from_millis(cfg.obd2_timeout_ms),
                cfg.obd2.dongle_ip.clone(),
                cfg.obd2.dongle_port,
            )
        };
        
        // Try to ensure we have a connection (with reconnect delay)
        if connection.is_none() {
            let should_try = match last_connect_attempt {
                Some(t) => t.elapsed() >= RECONNECT_DELAY,
                None => true,
            };
            if should_try {
                last_connect_attempt = Some(Instant::now());
                connection = try_connect(&dongle_ip, dongle_port, timeout, &watchdog);
            }
        }

        // Process requests with a timeout so we can check connection health
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(request) => {
                debug!("Dongle request: {:?}", String::from_utf8_lossy(&request.command));
                let result = if let Some(ref mut stream) = connection {
                    execute_command(stream, &request.command, timeout)
                } else {
                    Err(DongleError::NotConnected)
                };

                // If we got a disconnect error, drop the connection
                if matches!(result, Err(DongleError::Disconnected | DongleError::IoError(_))) {
                    warn!("Dongle connection lost, will reconnect");
                    connection = None;
                }

                if let Err(ref e) = result {
                    debug!("Dongle response error: {e}");
                } else {
                    debug!("Dongle response: {} bytes", result.as_ref().unwrap().len());
                }

                // Send response (ignore if receiver dropped)
                let _ = request.response_tx.send(result);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // No request, just keep the loop alive
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                info!("Dongle task channel closed, shutting down");
                break;
            }
        }
    }
}

/// Try to connect to the dongle and initialize it
fn try_connect(dongle_ip: &str, dongle_port: u16, timeout: Duration, watchdog: &WatchdogHandle) -> Option<TcpStream> {
    info!("Connecting to OBD2 dongle at {dongle_ip}:{dongle_port} (timeout: {}ms)", timeout.as_millis());

    let addr: SocketAddr = format!("{dongle_ip}:{dongle_port}").parse().ok()?;
    let stream = match TcpStream::connect_timeout(&addr, timeout) {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to connect to dongle: {e}");
            return None;
        }
    };
    
    // Feed watchdog after potentially long connect timeout
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

    // Initialize the dongle - disable echo, set protocol auto
    let init_commands = [
        b"ATZ\r".as_slice(),   // Reset
        b"ATE0\r".as_slice(),  // Echo off
        b"ATL0\r".as_slice(),  // Linefeeds off
        b"ATS0\r".as_slice(),  // Spaces off (compact responses)
        b"ATSP0\r".as_slice(), // Protocol auto
    ];

    for cmd in init_commands {
        debug!("Sending init command: {:?}", String::from_utf8_lossy(cmd));
        if let Err(e) = stream.write_all(cmd) {
            error!("Failed to send init command: {e}");
            return None;
        }
        // Read and discard the response
        let mut buf = [0u8; 256];
        std::thread::sleep(Duration::from_millis(100));
        let _ = stream.read(&mut buf);
    }

    info!("Connected to OBD2 dongle");
    Some(stream)
}

/// Execute a command on the dongle and return the response
fn execute_command(stream: &mut TcpStream, command: &[u8], timeout: Duration) -> Result<Obd2Buffer, DongleError> {
    // Send command with carriage return
    let mut cmd_with_cr: Obd2Buffer = command.into();
    if !cmd_with_cr.ends_with(b"\r") {
        cmd_with_cr.push(b'\r');
    }

    debug!("Sending to dongle: {:?}", String::from_utf8_lossy(&cmd_with_cr));

    stream
        .write_all(&cmd_with_cr)
        .map_err(|e| DongleError::IoError(e.to_string()))?;

    // Read response
    let mut buffer = [0u8; 64];
    let mut response = Obd2Buffer::new();
    let start = Instant::now();

    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return Err(DongleError::Disconnected),
            Ok(n) => {
                response.extend_from_slice(&buffer[..n]);
                debug!("Read {} bytes from dongle, total: {}", n, response.len());
                // Check if we have a complete response (ends with >)
                if response.contains(&b'>') {
                    debug!("Complete response: {:?}", String::from_utf8_lossy(&response));
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
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

/// Send a command to the dongle and wait for response
pub fn send_command(dongle_tx: &DongleSender, command: &[u8], timeout: Duration) -> Result<Obd2Buffer, DongleError> {
    let (response_tx, response_rx) = oneshot::channel();
    let request = DongleRequest {
        command: command.into(),
        response_tx,
    };

    dongle_tx
        .send(request)
        .map_err(|_| DongleError::NotConnected)?;

    response_rx
        .recv_timeout(timeout)
        .map_err(|_| DongleError::Timeout)?
}

/// Start the combined RPM poller and LED update task.
/// 
/// This task:
/// - Receives RPM values from client handlers via channel
/// - Polls the dongle for RPM when no client activity
/// - Updates LEDs based on current RPM
/// - Sends RPM to SSE clients
/// 
/// Returns a sender for client handlers to report RPM values.
pub fn start_rpm_led_task(
    mut led_controller: LedController,
    config: Arc<Mutex<Config>>,
    sse_tx: SseSender,
    dongle_tx: DongleSender,
) -> RpmSender {
    let (rpm_tx, rpm_rx) = mpsc::channel::<u32>();

    std::thread::spawn(move || {
        // Boot animation: blink purple 3 times
        {
            let total_leds = config.lock().unwrap().total_leds;
            if let Err(e) = led_controller.boot_animation(total_leds) {
                warn!("Boot animation failed: {e}");
            }
        }

        let watchdog = WatchdogHandle::register("rpm_led_task");
        info!("RPM/LED task started");

        let mut current_rpm: Option<u32> = None;
        let mut last_client_rpm: Option<Instant> = None;
        let mut last_poll: Option<Instant> = None;

        loop {
            watchdog.feed();

            // Calculate timeout based on:
            // 1. Time until next background poll (if no client activity)
            // 2. Time until next blink toggle (if blinking)
            let timeout = {
                let cfg = config.lock().unwrap();
                
                // Time until we should poll again
                let poll_interval = Duration::from_millis(IDLE_POLL_INTERVAL_MS);
                let time_until_poll = last_poll
                    .map_or(Duration::ZERO, |t| poll_interval.saturating_sub(t.elapsed()));

                // Time until next blink (find active blinking threshold)
                let blink_interval = if let Some(rpm) = current_rpm {
                    cfg.thresholds
                        .iter()
                        .filter(|t| rpm >= t.rpm && t.blink)
                        .next_back()
                        .map(|t| Duration::from_millis(u64::from(t.blink_ms)))
                } else {
                    None
                };

                // Use minimum of poll interval and blink interval
                match blink_interval {
                    Some(blink) => time_until_poll.min(blink),
                    None => time_until_poll,
                }
            };

            // Wait for RPM from client or timeout
            match rpm_rx.recv_timeout(timeout) {
                Ok(rpm) => {
                    last_client_rpm = Some(Instant::now());
                    current_rpm = Some(rpm);
                    debug!("Received client RPM: {rpm}");
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // Check if we should poll the dongle
                    let client_idle = last_client_rpm
                        .map_or(true, |t| t.elapsed() >= CLIENT_ACTIVITY_BACKOFF);
                    let poll_due = last_poll
                        .map_or(true, |t| t.elapsed() >= Duration::from_millis(IDLE_POLL_INTERVAL_MS));
                    if client_idle && poll_due {
                        last_poll = Some(Instant::now());
                        let timeout = Duration::from_millis(
                            config.lock().unwrap().obd2_timeout_ms
                        );
                        
                        if let Ok(response) = send_command(&dongle_tx, b"010C", timeout) {
                            if let Some(rpm) = extract_rpm_from_response(&response) {
                                debug!("Polled RPM: {rpm}");
                                current_rpm = Some(rpm);
                            }
                        }
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    warn!("RPM channel disconnected, exiting task");
                    break;
                }
            }

            // Update LEDs and send to SSE (always, to handle blinking)
            if let Some(rpm) = current_rpm {
                let _ = sse_tx.send(SseMessage::RpmUpdate(rpm));
                
                if let Ok(cfg) = config.lock() {
                    let _ = led_controller.update(rpm, &cfg);
                }
            }
        }
    });

    rpm_tx
}

/// Extract RPM from an OBD2 response (standalone function for use by the task)
fn extract_rpm_from_response(response: &[u8]) -> Option<u32> {
    Obd2Proxy::extract_rpm_from_response(response)
}

pub struct Obd2Proxy {
    config: Arc<Mutex<Config>>,
    rpm_tx: RpmSender,
    dongle_tx: DongleSender,
    /// Unique AT commands seen from clients (for debugging/web UI)
    at_command_log: AtCommandLog,
    /// Unique OBD2 PIDs requested by clients (for debugging/web UI)
    pid_log: PidLog,
}

impl Obd2Proxy {
    pub fn new(
        config: Arc<Mutex<Config>>,
        rpm_tx: RpmSender,
        dongle_tx: DongleSender,
        at_command_log: AtCommandLog,
        pid_log: PidLog,
    ) -> Self {
        Self {
            config,
            rpm_tx,
            dongle_tx,
            at_command_log,
            pid_log,
        }
    }

    pub fn run(self) -> Result<()> {
        info!("OBD2 proxy starting...");

        // Get listen port from config
        let listen_port = self.config.lock().unwrap().obd2.listen_port;

        // Start proxy server
        let listener = TcpListener::bind(format!("0.0.0.0:{listen_port}"))?;
        
        // Register watchdog for this thread (the proxy accept loop)
        let watchdog = WatchdogHandle::register("obd2_proxy");
        
        // Set non-blocking with timeout so we can feed the watchdog
        listener.set_nonblocking(true)?;
        
        info!("OBD2 proxy started on port {listen_port}");

        loop {
            watchdog.feed();
            
            match listener.accept() {
                Ok((stream, _)) => {
                    let config = self.config.clone();
                    let rpm_tx = self.rpm_tx.clone();
                    let dongle_tx = self.dongle_tx.clone();
                    let at_command_log = self.at_command_log.clone();
                    let pid_log = self.pid_log.clone();

                    std::thread::spawn(move || {
                        if let Err(e) =
                            Self::handle_client(stream, &config, &rpm_tx, &dongle_tx, &at_command_log, &pid_log)
                        {
                            error!("Error handling client: {e:?}");
                        }
                    });
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // No connection waiting, sleep briefly and try again
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => {
                    error!("Error accepting connection: {e:?}");
                }
            }
        }
    }

    #[allow(clippy::needless_pass_by_value)] // TcpStream is consumed by BufReader
    fn handle_client(
        client_stream: TcpStream,
        config: &Arc<Mutex<Config>>,
        rpm_tx: &RpmSender,
        dongle_tx: &DongleSender,
        at_command_log: &AtCommandLog,
        pid_log: &PidLog,
    ) -> Result<()> {
        let peer = client_stream.peer_addr()?;
        info!("OBD2 client connected: {peer}");

        // Get timeout from config for socket operations
        let timeout = Duration::from_millis(config.lock().unwrap().obd2_timeout_ms);
        client_stream.set_read_timeout(Some(timeout))?;
        client_stream.set_write_timeout(Some(timeout))?;

        // Register watchdog for this client handler
        let watchdog = WatchdogHandle::register("obd2_client");

        // Use BufReader for efficient reading, but keep reference to stream for writing
        let mut reader = BufReader::new(&client_stream);
        let mut writer = &client_stream;

        // Track per-connection ELM327 state
        let mut state = ClientState::default();

        // Command buffer for accumulating characters
        let mut cmd_buffer = Vec::with_capacity(64);

        loop {
            watchdog.feed();

            // Read one byte at a time for character-by-character echo
            // BufReader batches actual reads internally for efficiency
            let mut byte = [0u8; 1];
            match reader.read(&mut byte) {
                Ok(0) => {
                    info!("OBD2 client disconnected: {peer}");
                    break;
                }
                Ok(_) => {
                    let ch = byte[0];
                    
                    // Echo character immediately if enabled
                    if state.echo_enabled {
                        writer.write_all(&byte)?;
                    }
                    
                    // CR is command terminator
                    if ch == b'\r' {
                        // Process the accumulated command
                        let command = String::from_utf8_lossy(&cmd_buffer);
                        let command = command.trim();
                        
                        if !command.is_empty() {
                            debug!("OBD2 client command: {command:?}");
                            let ctx = CommandContext {
                                config,
                                rpm_tx,
                                dongle_tx,
                                at_command_log,
                                pid_log,
                            };
                            Self::process_command(
                                command,
                                &cmd_buffer,
                                &mut writer,
                                &mut state,
                                &ctx,
                            )?;
                        }
                        
                        cmd_buffer.clear();
                    } else if ch != b'\n' {
                        // Accumulate non-LF characters (ignore LF per ELM327 spec)
                        cmd_buffer.push(ch);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    // Client timeout, keep connection alive
                }
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
        state: &mut ClientState,
        ctx: &CommandContext<'_>,
    ) -> Result<()> {
        // Check if this is an AT command (handle locally)
        if command.to_uppercase().starts_with("AT") {
            debug!("Handling AT command locally");
            
            // Record unique AT commands (store uppercase for dedup)
            if let Ok(mut log) = ctx.at_command_log.lock() {
                log.insert(command.to_uppercase());
            }
            
            let response = Self::handle_at_command(command, state);
            writer
                .write_all(response.as_bytes())
                .context("Failed to write AT response")?;
            return Ok(());
        }

        // Get timeout from config
        let timeout = Duration::from_millis(ctx.config.lock().unwrap().obd2_timeout_ms);

        // Record unique OBD2 PIDs (store uppercase for dedup)
        if let Ok(mut log) = ctx.pid_log.lock() {
            log.insert(command.to_uppercase());
        }

        // Forward OBD2 command to dongle task
        match send_command(ctx.dongle_tx, raw_command, timeout) {
            Ok(response) => {
                // Extract RPM if this was an RPM request - send to RPM/LED task
                if let Some(rpm) = Self::extract_rpm_from_response(&response) {
                    debug!("Client RPM request: {rpm}");
                    let _ = ctx.rpm_tx.send(rpm);
                }

                // Format and send dongle response (echo already sent character-by-character)
                let formatted = state.format_response(&response);
                writer
                    .write_all(&formatted)
                    .context("Failed to write response")?;
            }
            Err(e) => {
                error!("Dongle error: {e}");
                // Send proper ELM327 error response (echo already sent)
                let le = state.line_ending();
                let error_msg = e.to_elm327_error();
                let error_response = format!("{le}{error_msg}{le}>");
                writer.write_all(error_response.as_bytes())?;
            }
        }

        Ok(())
    }

    /// Handle AT commands locally (per-connection state)
    /// Echo has already been sent character-by-character, so responses don't include it
    fn handle_at_command(command: &str, state: &mut ClientState) -> String {
        let cmd = command.to_uppercase();
        let le = state.line_ending();
        
        // Determine response content (without line endings)
        let response_text = match cmd.as_str() {
            "ATZ" => {
                // Reset all settings to defaults
                *state = ClientState::default();
                // Use new state's line ending for response
                let le = state.line_ending();
                return format!("{le}ELM327 v1.5{le}>");
            }
            "ATE0" => {
                state.echo_enabled = false;
                "OK"
            }
            "ATE1" => {
                state.echo_enabled = true;
                "OK"
            }
            "ATL0" => {
                state.linefeeds_enabled = false;
                "OK"
            }
            "ATL1" => {
                state.linefeeds_enabled = true;
                "OK"
            }
            "ATS0" => {
                state.spaces_enabled = false;
                "OK"
            }
            "ATS1" => {
                state.spaces_enabled = true;
                "OK"
            }
            "ATH0" => {
                state.headers_enabled = false;
                "OK"
            }
            "ATH1" => {
                state.headers_enabled = true;
                "OK"
            }
            "ATSP0" | "ATAT1" | "ATAT2" => "OK",
            _ if cmd.starts_with("ATSP") => "OK",
            _ if cmd.starts_with("ATST") => "OK",
            _ if cmd.starts_with("ATAT") => "OK",
            "ATI" => "ELM327 v1.5",
            "AT@1" => "TachTalk OBD2 Proxy",
            _ => "?",
        };

        // Build response with proper line endings (echo already sent)
        // Note: for commands that change linefeed setting, we use the OLD setting
        // since le was captured before the match
        format!("{le}{response_text}{le}>")
    }

    fn extract_rpm_from_response(data: &[u8]) -> Option<u32> {
        // OBD2 response format for RPM (PID 0C): "41 0C XX XX" or "410CXX XX"
        // RPM = ((A * 256) + B) / 4
        let text = std::str::from_utf8(data).ok()?;

        // Look for "41 0C" or "410C" pattern
        let text_upper = text.to_uppercase();
        if let Some(pos) = text_upper.find("410C") {
            let after = &text_upper[pos + 4..];
            // Try to parse hex bytes (with or without spaces)
            let hex_chars: String = after.chars().filter(char::is_ascii_hexdigit).collect();

            if hex_chars.len() >= 4 {
                let a = u32::from_str_radix(&hex_chars[0..2], 16).ok()?;
                let b = u32::from_str_radix(&hex_chars[2..4], 16).ok()?;
                let rpm = ((a * 256) + b) / 4;
                return Some(rpm);
            }
        }

        None
    }
}
