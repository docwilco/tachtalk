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
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::leds::LedController;
use crate::watchdog::WatchdogHandle;
use crate::web_server::{SseMessage, SseSender};

pub const OBD2_PORT: u16 = 35000;
const DONGLE_IP: &str = "192.168.0.10";
const DONGLE_PORT: u16 = 35000;
const IDLE_POLL_INTERVAL_MS: u64 = 100;
const DONGLE_TIMEOUT: Duration = Duration::from_secs(5);
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// Type alias for small OBD2 command/response buffers
pub type Obd2Buffer = SmallVec<[u8; 12]>;

/// Request to the dongle task
pub struct DongleRequest {
    /// The OBD2 command to send (without terminator)
    pub command: Obd2Buffer,
    /// Channel to send the response back
    pub response_tx: oneshot::Sender<Result<Obd2Buffer, DongleError>>,
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

pub type DongleSender = Sender<DongleRequest>;

/// Start the dongle task and return a sender for requests
pub fn start_dongle_task() -> DongleSender {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        dongle_task(rx);
    });
    tx
}

/// The dongle task - owns the connection and processes requests
fn dongle_task(rx: Receiver<DongleRequest>) {
    info!("OBD2 dongle task starting...");
    
    let mut connection: Option<TcpStream> = None;
    
    let watchdog = WatchdogHandle::register("obd2_dongle");
    
    info!("OBD2 dongle task started");

    loop {
        if let Some(ref wd) = watchdog {
            wd.feed();
        }
        
        // Try to ensure we have a connection
        if connection.is_none() {
            connection = try_connect();
        }

        // Process requests with a timeout so we can check connection health
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(request) => {
                debug!("Dongle request: {:?}", String::from_utf8_lossy(&request.command));
                let result = if let Some(ref mut stream) = connection {
                    execute_command(stream, &request.command)
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
fn try_connect() -> Option<TcpStream> {
    info!("Connecting to OBD2 dongle at {DONGLE_IP}:{DONGLE_PORT}");

    let stream = match TcpStream::connect(format!("{DONGLE_IP}:{DONGLE_PORT}")) {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to connect to dongle: {e}");
            std::thread::sleep(RECONNECT_DELAY);
            return None;
        }
    };

    if let Err(e) = stream.set_read_timeout(Some(DONGLE_TIMEOUT)) {
        error!("Failed to set read timeout: {e}");
        return None;
    }
    if let Err(e) = stream.set_write_timeout(Some(DONGLE_TIMEOUT)) {
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
fn execute_command(stream: &mut TcpStream, command: &[u8]) -> Result<Obd2Buffer, DongleError> {
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
                if start.elapsed() > DONGLE_TIMEOUT {
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
pub fn send_command(dongle_tx: &DongleSender, command: &[u8]) -> Result<Obd2Buffer, DongleError> {
    let (response_tx, response_rx) = oneshot::channel();
    let request = DongleRequest {
        command: command.into(),
        response_tx,
    };

    dongle_tx
        .send(request)
        .map_err(|_| DongleError::NotConnected)?;

    response_rx
        .recv_timeout(DONGLE_TIMEOUT)
        .map_err(|_| DongleError::Timeout)?
}

pub struct Obd2Proxy {
    config: Arc<Mutex<Config>>,
    led_controller: Arc<Mutex<LedController>>,
    sse_tx: SseSender,
    dongle_tx: DongleSender,
}

impl Obd2Proxy {
    pub fn new(
        config: Arc<Mutex<Config>>,
        led_controller: Arc<Mutex<LedController>>,
        sse_tx: SseSender,
        dongle_tx: DongleSender,
    ) -> Self {
        Self {
            config,
            led_controller,
            sse_tx,
            dongle_tx,
        }
    }

    pub fn run(self) -> Result<()> {
        info!("OBD2 proxy starting...");
        
        // Start background RPM poller thread
        let config_clone = self.config.clone();
        let led_clone = self.led_controller.clone();
        let sse_tx_clone = self.sse_tx.clone();
        let dongle_tx_clone = self.dongle_tx.clone();

        std::thread::spawn(move || {
            Self::background_poller(config_clone, led_clone, sse_tx_clone, dongle_tx_clone);
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

                    std::thread::spawn(move || {
                        if let Err(e) =
                            Self::handle_client(stream, config, led_controller, sse_tx, dongle_tx)
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
    ) {
        info!("Background RPM poller starting...");
        
        let watchdog = WatchdogHandle::register("rpm_poller");
        
        info!("Background RPM poller started");
        
        loop {
            if let Some(ref wd) = watchdog {
                wd.feed();
            }
            
            std::thread::sleep(Duration::from_millis(IDLE_POLL_INTERVAL_MS));

            // Request RPM from dongle
            if let Ok(response) = send_command(&dongle_tx, b"010C") {
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
        mut client_stream: TcpStream,
        config: Arc<Mutex<Config>>,
        led_controller: Arc<Mutex<LedController>>,
        sse_tx: SseSender,
        dongle_tx: DongleSender,
    ) -> Result<()> {
        let peer = client_stream.peer_addr()?;
        info!("OBD2 client connected: {peer}");

        client_stream.set_read_timeout(Some(Duration::from_secs(30)))?;

        // Track client's echo setting (local to this connection)
        let mut echo_enabled = true;

        let mut buffer = [0u8; 1024];

        loop {
            match client_stream.read(&mut buffer) {
                Ok(0) => {
                    info!("OBD2 client disconnected: {peer}");
                    break;
                }
                Ok(n) => {
                    let request = &buffer[..n];
                    let request_str = String::from_utf8_lossy(request);
                    let command = request_str.trim();
                    debug!("OBD2 client command: {command:?}");

                    // Check if this is an AT command (handle locally)
                    if command.to_uppercase().starts_with("AT") {
                        debug!("Handling AT command locally");
                        let response = Self::handle_at_command(command, &mut echo_enabled);
                        client_stream
                            .write_all(response.as_bytes())
                            .context("Failed to write AT response")?;
                        continue;
                    }

                    // Forward OBD2 command to dongle task
                    match send_command(&dongle_tx, request) {
                        Ok(response) => {
                            // Extract RPM if this was an RPM request
                            if let Some(rpm) = Self::extract_rpm_from_response(&response) {
                                debug!("Client RPM request: {rpm}");
                                let _ = sse_tx.send(SseMessage::RpmUpdate(rpm));

                                if let Ok(mut led) = led_controller.lock() {
                                    if let Ok(cfg) = config.lock() {
                                        let _ = led.update(rpm, &cfg);
                                    }
                                }
                            }

                            // Build response for client
                            let mut client_response = Vec::new();
                            if echo_enabled {
                                client_response.extend_from_slice(request);
                            }
                            client_response.extend_from_slice(&response);

                            client_stream
                                .write_all(&client_response)
                                .context("Failed to write response")?;
                        }
                        Err(e) => {
                            error!("Dongle error: {e}");
                            // Send error response to client
                            let error_response = if echo_enabled {
                                format!("{command}\r\n?\r\n>")
                            } else {
                                "?\r\n>".to_string()
                            };
                            client_stream.write_all(error_response.as_bytes())?;
                        }
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

    /// Handle AT commands locally (per-connection state)
    fn handle_at_command(command: &str, echo_enabled: &mut bool) -> String {
        let cmd = command.to_uppercase();
        let response = match cmd.as_str() {
            "ATZ" => "ELM327 v1.5\r\n",
            "ATE0" => {
                *echo_enabled = false;
                "OK\r\n"
            }
            "ATE1" => {
                *echo_enabled = true;
                "OK\r\n"
            }
            "ATL0" | "ATL1" | "ATS0" | "ATS1" | "ATH0" | "ATH1" | "ATSP0" | "ATAT1" | "ATAT2" => {
                "OK\r\n"
            }
            _ if cmd.starts_with("ATSP") => "OK\r\n",
            _ if cmd.starts_with("ATST") => "OK\r\n",
            _ if cmd.starts_with("ATAT") => "OK\r\n",
            "ATI" => "ELM327 v1.5\r\n",
            "AT@1" => "TachTalk OBD2 Proxy\r\n",
            _ => "?\r\n",
        };

        if *echo_enabled {
            format!("{command}\r\n{response}>")
        } else {
            format!("{response}>")
        }
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
