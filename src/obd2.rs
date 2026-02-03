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

pub const OBD2_PORT: u16 = 35000;
const DONGLE_IP: &str = "192.168.0.10";
const DONGLE_PORT: u16 = 35000;
const IDLE_POLL_INTERVAL_MS: u64 = 100;
/// How long after a client RPM update before background poller resumes
const CLIENT_ACTIVITY_BACKOFF: Duration = Duration::from_millis(500);
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// Shared log of unique AT commands received from clients (for debugging)
pub type AtCommandLog = Arc<Mutex<HashSet<String>>>;

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
struct ClientState {
    /// Echo received characters back (ATE0/ATE1)
    echo_enabled: bool,
    /// Add linefeeds after carriage returns (ATL0/ATL1)
    linefeeds_enabled: bool,
    /// Print spaces between response bytes (ATS0/ATS1)
    spaces_enabled: bool,
    /// Show header bytes in responses (ATH0/ATH1)
    headers_enabled: bool,
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
        dongle_task(rx, config);
    });
    tx
}

/// The dongle task - owns the connection and processes requests
fn dongle_task(rx: Receiver<DongleRequest>, config: Arc<Mutex<Config>>) {
    info!("OBD2 dongle task starting...");
    
    let mut connection: Option<TcpStream> = None;
    let mut last_connect_attempt: Option<Instant> = None;
    
    let watchdog = WatchdogHandle::register("obd2_dongle");
    
    info!("OBD2 dongle task started");

    loop {
        if let Some(ref wd) = watchdog {
            wd.feed();
        }
        
        // Get timeout from config
        let timeout = Duration::from_millis(config.lock().unwrap().obd2_timeout_ms);
        
        // Try to ensure we have a connection (with reconnect delay)
        if connection.is_none() {
            let should_try = match last_connect_attempt {
                Some(t) => t.elapsed() >= RECONNECT_DELAY,
                None => true,
            };
            if should_try {
                last_connect_attempt = Some(Instant::now());
                connection = try_connect(timeout, &watchdog);
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
fn try_connect(timeout: Duration, watchdog: &Option<WatchdogHandle>) -> Option<TcpStream> {
    info!("Connecting to OBD2 dongle at {DONGLE_IP}:{DONGLE_PORT} (timeout: {}ms)", timeout.as_millis());

    let addr: SocketAddr = format!("{DONGLE_IP}:{DONGLE_PORT}").parse().ok()?;
    let stream = match TcpStream::connect_timeout(&addr, timeout) {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to connect to dongle: {e}");
            return None;
        }
    };
    
    // Feed watchdog after potentially long connect timeout
    if let Some(ref wd) = watchdog {
        wd.feed();
    }

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

pub struct Obd2Proxy {
    config: Arc<Mutex<Config>>,
    led_controller: Arc<Mutex<LedController>>,
    sse_tx: SseSender,
    dongle_tx: DongleSender,
    /// When the last RPM was proxied from a client (not background poller)
    last_proxied_rpm: Arc<Mutex<Option<Instant>>>,
    /// Unique AT commands seen from clients (for debugging/web UI)
    at_command_log: AtCommandLog,
}

impl Obd2Proxy {
    pub fn new(
        config: Arc<Mutex<Config>>,
        led_controller: Arc<Mutex<LedController>>,
        sse_tx: SseSender,
        dongle_tx: DongleSender,
        at_command_log: AtCommandLog,
    ) -> Self {
        Self {
            config,
            led_controller,
            sse_tx,
            dongle_tx,
            last_proxied_rpm: Arc::new(Mutex::new(None)),
            at_command_log,
        }
    }

    pub fn run(self) -> Result<()> {
        info!("OBD2 proxy starting...");
        
        // Start background RPM poller thread
        let config_clone = self.config.clone();
        let led_clone = self.led_controller.clone();
        let sse_tx_clone = self.sse_tx.clone();
        let dongle_tx_clone = self.dongle_tx.clone();
        let last_proxied_rpm_clone = self.last_proxied_rpm.clone();

        std::thread::spawn(move || {
            Self::background_poller(config_clone, led_clone, sse_tx_clone, dongle_tx_clone, last_proxied_rpm_clone);
        });

        // Start proxy server
        let listener = TcpListener::bind(format!("0.0.0.0:{OBD2_PORT}"))?;
        
        // Register watchdog for this thread (the proxy accept loop)
        let watchdog = WatchdogHandle::register("obd2_proxy");
        
        // Set non-blocking with timeout so we can feed the watchdog
        listener.set_nonblocking(true)?;
        
        info!("OBD2 proxy started on port {OBD2_PORT}");

        loop {
            if let Some(ref wd) = watchdog {
                wd.feed();
            }
            
            match listener.accept() {
                Ok((stream, _)) => {
                    let config = self.config.clone();
                    let led_controller = self.led_controller.clone();
                    let sse_tx = self.sse_tx.clone();
                    let dongle_tx = self.dongle_tx.clone();
                    let last_proxied_rpm = self.last_proxied_rpm.clone();
                    let at_command_log = self.at_command_log.clone();

                    std::thread::spawn(move || {
                        if let Err(e) =
                            Self::handle_client(stream, config, led_controller, sse_tx, dongle_tx, last_proxied_rpm, at_command_log)
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

    fn background_poller(
        config: Arc<Mutex<Config>>,
        led_controller: Arc<Mutex<LedController>>,
        sse_tx: SseSender,
        dongle_tx: DongleSender,
        last_proxied_rpm: Arc<Mutex<Option<Instant>>>,
    ) {
        info!("Background RPM poller starting...");
        
        let watchdog = WatchdogHandle::register("rpm_poller");
        
        info!("Background RPM poller started");
        
        loop {
            if let Some(ref wd) = watchdog {
                wd.feed();
            }
            
            std::thread::sleep(Duration::from_millis(IDLE_POLL_INTERVAL_MS));

            // Check if a client recently provided an RPM update
            if let Some(last_update) = *last_proxied_rpm.lock().unwrap() {
                if last_update.elapsed() < CLIENT_ACTIVITY_BACKOFF {
                    // Client is actively polling, skip background poll
                    continue;
                }
            }

            // Get timeout from config
            let timeout = Duration::from_millis(config.lock().unwrap().obd2_timeout_ms);

            // Request RPM from dongle
            if let Ok(response) = send_command(&dongle_tx, b"010C", timeout) {
                if let Some(rpm) = Self::extract_rpm_from_response(&response) {
                    debug!("Polled RPM: {rpm}");
                    
                    // Send to SSE clients
                    let _ = sse_tx.send(SseMessage::RpmUpdate(rpm));

                    if let Ok(mut led) = led_controller.lock() {
                        if let Ok(cfg) = config.lock() {
                            let _ = led.update(rpm, &cfg);
                        }
                    }
                }
            }
        }
    }

    fn handle_client(
        client_stream: TcpStream,
        config: Arc<Mutex<Config>>,
        led_controller: Arc<Mutex<LedController>>,
        sse_tx: SseSender,
        dongle_tx: DongleSender,
        last_proxied_rpm: Arc<Mutex<Option<Instant>>>,
        at_command_log: AtCommandLog,
    ) -> Result<()> {
        let peer = client_stream.peer_addr()?;
        info!("OBD2 client connected: {peer}");

        client_stream.set_read_timeout(Some(Duration::from_secs(30)))?;

        // Use BufReader for efficient reading, but keep reference to stream for writing
        let mut reader = BufReader::new(&client_stream);
        let mut writer = &client_stream;

        // Track per-connection ELM327 state
        let mut state = ClientState::default();

        // Command buffer for accumulating characters
        let mut cmd_buffer = Vec::with_capacity(64);

        loop {
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
                            Self::process_command(
                                command,
                                &cmd_buffer,
                                &mut writer,
                                &mut state,
                                &config,
                                &led_controller,
                                &sse_tx,
                                &dongle_tx,
                                &last_proxied_rpm,
                                &at_command_log,
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
                    continue;
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
        config: &Arc<Mutex<Config>>,
        led_controller: &Arc<Mutex<LedController>>,
        sse_tx: &SseSender,
        dongle_tx: &DongleSender,
        last_proxied_rpm: &Arc<Mutex<Option<Instant>>>,
        at_command_log: &AtCommandLog,
    ) -> Result<()> {
        // Check if this is an AT command (handle locally)
        if command.to_uppercase().starts_with("AT") {
            debug!("Handling AT command locally");
            
            // Record unique AT commands (store uppercase for dedup)
            if let Ok(mut log) = at_command_log.lock() {
                log.insert(command.to_uppercase());
            }
            
            let response = Self::handle_at_command(command, state);
            writer
                .write_all(response.as_bytes())
                .context("Failed to write AT response")?;
            return Ok(());
        }

        // Get timeout from config
        let timeout = Duration::from_millis(config.lock().unwrap().obd2_timeout_ms);

        // Forward OBD2 command to dongle task
        match send_command(dongle_tx, raw_command, timeout) {
            Ok(response) => {
                // Extract RPM if this was an RPM request
                if let Some(rpm) = Self::extract_rpm_from_response(&response) {
                    debug!("Client RPM request: {rpm}");
                    
                    // Record that client provided an RPM update
                    *last_proxied_rpm.lock().unwrap() = Some(Instant::now());
                    
                    let _ = sse_tx.send(SseMessage::RpmUpdate(rpm));

                    if let Ok(mut led) = led_controller.lock() {
                        if let Ok(cfg) = config.lock() {
                            let _ = led.update(rpm, &cfg);
                        }
                    }
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
            let hex_chars: String = after.chars().filter(|c| c.is_ascii_hexdigit()).collect();

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
