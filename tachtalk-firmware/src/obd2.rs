//! OBD2 proxy with dedicated dongle task.
//!
//! Architecture:
//! - Dongle task: owns the single TCP connection to the OBD2 dongle, handles
//!   connection setup, reconnection, and processes OBD2 data commands
//! - Proxy server: accepts client connections, forwards OBD2 commands via channel
//! - AT commands (ATE0, ATZ, etc.) are handled locally per connection using `tachtalk_elm327`

use anyhow::{Context, Result};
use log::{debug, error, info, warn};
use smallvec::SmallVec;
use std::io::{BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

use crate::rpm_leds::RpmTaskMessage;
use std::time::{Duration, Instant};
use tachtalk_elm327_lib::ClientState;

use crate::watchdog::WatchdogHandle;
use crate::State;

pub const IDLE_POLL_INTERVAL_MS: u64 = 100;
/// How long after a client RPM update before background poller resumes
pub const CLIENT_ACTIVITY_BACKOFF: Duration = Duration::from_millis(250);
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// State for the dongle connection
struct DongleState {
    stream: TcpStream,
    /// Whether the dongle supports the "1" repeat command (None = untested)
    supports_repeat: Option<bool>,
    /// Last OBD2 command sent (for repeat optimization)
    last_command: Option<Obd2Buffer>,
}

impl DongleState {
    /// Execute a command, using the "1" repeat optimization when possible.
    fn execute_with_repeat(
        &mut self,
        command: &Obd2Buffer,
        timeout: Duration,
    ) -> Result<Obd2Buffer, DongleError> {
        // Check if we can try repeat command optimization
        // Only try if not proven unsupported, and same command as last
        let can_try_repeat = self.supports_repeat != Some(false)
            && self.last_command.as_ref() == Some(command)
            && !command.starts_with(b"AT");

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
                    // Update last_command before sending (dongle will have this as its last)
                    self.last_command = Some(command.clone());
                    execute_command(&mut self.stream, command, timeout)
                } else {
                    // Repeat worked! last_command stays the same (we repeated it)
                    if self.supports_repeat.is_none() {
                        info!("Dongle supports repeat command");
                        self.supports_repeat = Some(true);
                    }
                    repeat_result
                }
            } else {
                // Repeat failed with error - clear last_command since we don't
                // know if dongle processed it
                self.last_command = None;
                repeat_result
            }
        } else {
            // Update last_command before sending (dongle will have this as its last)
            if !command.starts_with(b"AT") {
                self.last_command = Some(command.clone());
            }
            let result = execute_command(&mut self.stream, command, timeout);
            // If command failed, clear last_command since we don't know if dongle processed it
            if result.is_err() {
                self.last_command = None;
            }
            result
        }
    }
}

/// Type alias for small OBD2 command/response buffers
pub type Obd2Buffer = SmallVec<[u8; 12]>;

/// Request to the dongle task
pub struct DongleRequest {
    /// The OBD2 command to send (without terminator)
    pub command: Obd2Buffer,
    /// Channel to send the response back (None = fire-and-forget)
    pub response_tx: Option<oneshot::Sender<Result<Obd2Buffer, DongleError>>>,
}

/// Shared context for processing OBD2 commands (reduces parameter count)
struct CommandContext<'a> {
    state: &'a Arc<State>,
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
pub type DongleReceiver = Receiver<DongleRequest>;

/// Run the dongle task - owns the connection and processes requests
pub fn dongle_task(state: &Arc<State>, rx: &DongleReceiver) {
    use std::sync::atomic::Ordering;
    
    info!("OBD2 dongle task starting...");

    let mut connection: Option<DongleState> = None;
    let mut last_connect_attempt: Option<Instant> = None;

    let watchdog = WatchdogHandle::register("obd2_dongle");

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

        // Try to ensure we have a connection (with reconnect delay)
        if connection.is_none() {
            let should_try = match last_connect_attempt {
                Some(t) => t.elapsed() >= RECONNECT_DELAY,
                None => true,
            };
            if should_try {
                last_connect_attempt = Some(Instant::now());
                if let Some((dongle_state, local_addr, remote_addr)) = 
                    try_connect(&dongle_ip, dongle_port, timeout, &watchdog) 
                {
                    connection = Some(dongle_state);
                    // Update shared connection status and TCP info
                    state.dongle_connected.store(true, Ordering::Relaxed);
                    *state.dongle_tcp_info.lock().unwrap() = Some((local_addr, remote_addr));
                } else {
                    state.dongle_connected.store(false, Ordering::Relaxed);
                    *state.dongle_tcp_info.lock().unwrap() = None;
                }
            }
        }

        // Process requests with a timeout so we can check connection health
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(request) => {
                debug!(
                    "Dongle request: {:?}",
                    String::from_utf8_lossy(&request.command)
                );
                let result = if let Some(ref mut dongle_state) = connection {
                    dongle_state.execute_with_repeat(&request.command, timeout)
                } else {
                    Err(DongleError::NotConnected)
                };

                // If we got a disconnect error, drop the connection
                if matches!(
                    result,
                    Err(DongleError::Disconnected | DongleError::IoError(_))
                ) {
                    warn!("Dongle connection lost, will reconnect");
                    connection = None;
                    state.dongle_connected.store(false, Ordering::Relaxed);
                    *state.dongle_tcp_info.lock().unwrap() = None;
                }

                if let Err(ref e) = result {
                    debug!("Dongle response error: {e}");
                } else {
                    debug!("Dongle response: {} bytes", result.as_ref().unwrap().len());

                    // Extract RPM from successful 010C responses and send to rpm_led task
                    if request.command.starts_with(b"010C") {
                        if let Some(rpm) =
                            tachtalk_elm327_lib::extract_rpm_from_response(result.as_ref().unwrap())
                        {
                            debug!("Dongle extracted RPM: {rpm}");
                            let _ = state.rpm_tx.send(RpmTaskMessage::Rpm(rpm));
                        }
                    }
                }

                // Send response if caller is waiting (fire-and-forget has None)
                if let Some(response_tx) = request.response_tx {
                    let _ = response_tx.send(result);
                }
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
/// Returns (`DongleState`, `local_addr`, `remote_addr`) on success
fn try_connect(
    dongle_ip: &str,
    dongle_port: u16,
    timeout: Duration,
    watchdog: &WatchdogHandle,
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

    // Get socket addresses before any potential failures
    let local_addr = stream.local_addr().ok()?;
    let remote_addr = stream.peer_addr().ok()?;

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

    Some((DongleState {
        stream,
        supports_repeat: None, // Will be tested lazily on first repeat opportunity
        last_command: None,
    }, local_addr, remote_addr))
}

/// Execute a command on the dongle and return the response
fn execute_command(
    stream: &mut TcpStream,
    command: &[u8],
    timeout: Duration,
) -> Result<Obd2Buffer, DongleError> {
    // Send command with carriage return
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
                    debug!(
                        "Complete response: {:?}",
                        String::from_utf8_lossy(&response)
                    );
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
pub fn send_command(
    dongle_tx: &DongleSender,
    command: &[u8],
    timeout: Duration,
) -> Result<Obd2Buffer, DongleError> {
    let (response_tx, response_rx) = oneshot::channel();
    let request = DongleRequest {
        command: command.into(),
        response_tx: Some(response_tx),
    };

    dongle_tx
        .send(request)
        .map_err(|_| DongleError::NotConnected)?;

    response_rx
        .recv_timeout(timeout)
        .map_err(|_| DongleError::Timeout)?
}

/// Send a command to the dongle without waiting for response (fire-and-forget)
/// RPM extraction happens in the dongle task and is sent via `rpm_tx`
pub fn send_command_async(dongle_tx: &DongleSender, command: &[u8]) {
    let request = DongleRequest {
        command: command.into(),
        response_tx: None,
    };
    let _ = dongle_tx.send(request);
}

pub struct Obd2Proxy {
    state: Arc<State>,
}

/// RAII guard to decrement client count and remove TCP info on drop
struct ClientCountGuard<'a> {
    state: &'a State,
    tcp_info: (SocketAddr, SocketAddr),
}

impl Drop for ClientCountGuard<'_> {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        self.state.obd2_client_count.fetch_sub(1, Ordering::Relaxed);
        self.state.client_tcp_info.lock().unwrap().retain(|&x| x != self.tcp_info);
    }
}

impl Obd2Proxy {
    pub fn new(state: Arc<State>) -> Self {
        Self { state }
    }

    pub fn run(self) -> Result<()> {
        info!("OBD2 proxy starting...");

        // Get listen port from config
        let listen_port = self.state.config.lock().unwrap().obd2.listen_port;

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
                    let state = self.state.clone();

                    crate::thread_util::spawn_named(c"obd2_client", move || {
                        if let Err(e) = Self::handle_client(stream, &state) {
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
    fn handle_client(client_stream: TcpStream, state: &Arc<State>) -> Result<()> {
        use std::sync::atomic::Ordering;
        
        let peer = client_stream.peer_addr()?;
        let local = client_stream.local_addr()?;
        info!("OBD2 client connected: {peer}");
        
        // Track connection info
        let tcp_info = (local, peer);
        state.client_tcp_info.lock().unwrap().push(tcp_info);
        
        // Increment client count
        state.obd2_client_count.fetch_add(1, Ordering::Relaxed);
        
        // Use a guard to ensure we decrement and remove TCP info on any exit path
        let _guard = ClientCountGuard { state, tcp_info };

        // Get timeout from config for socket operations
        let timeout = Duration::from_millis(state.config.lock().unwrap().obd2_timeout_ms);
        client_stream.set_read_timeout(Some(timeout))?;
        client_stream.set_write_timeout(Some(timeout))?;

        // Register watchdog for this client handler
        let watchdog = WatchdogHandle::register("obd2_client");

        // Use BufReader for efficient reading, but keep reference to stream for writing
        let mut reader = BufReader::new(&client_stream);
        let mut writer = &client_stream;

        // Track per-connection ELM327 state
        let mut client_state = ClientState::default();

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
                    if client_state.echo_enabled {
                        writer.write_all(&byte)?;
                    }

                    // CR is command terminator
                    if ch == b'\r' {
                        // Process the accumulated command
                        let command = String::from_utf8_lossy(&cmd_buffer);
                        let command = command.trim();

                        if !command.is_empty() {
                            debug!("OBD2 client command: {command:?}");
                            let ctx = CommandContext { state };
                            Self::process_command(
                                command,
                                &cmd_buffer,
                                &mut writer,
                                &mut client_state,
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
        client_state: &mut ClientState,
        ctx: &CommandContext<'_>,
    ) -> Result<()> {
        // Check if this is an AT command (handle locally)
        if command.to_uppercase().starts_with("AT") {
            debug!("Handling AT command locally");

            // Record unique AT commands (store uppercase for dedup)
            if let Ok(mut log) = ctx.state.at_command_log.lock() {
                log.insert(command.to_uppercase());
            }

            let response = client_state.handle_at_command(command);
            writer
                .write_all(response.as_bytes())
                .context("Failed to write AT response")?;
            return Ok(());
        }

        // Handle "1" repeat command - expand to this client's last command
        let (effective_command, effective_raw): (String, Obd2Buffer) = if command == "1" {
            if let Some(last) = &client_state.last_obd_command {
                debug!("Expanding repeat command to: {last}");
                (last.clone(), last.as_bytes().into())
            } else {
                // No previous command - return error
                let le = client_state.line_ending();
                let error_response = format!("{le}?{le}>");
                writer.write_all(error_response.as_bytes())?;
                return Ok(());
            }
        } else {
            (command.to_string(), raw_command.into())
        };

        // Get timeout from config
        let timeout = Duration::from_millis(ctx.state.config.lock().unwrap().obd2_timeout_ms);

        // Record unique OBD2 PIDs (store uppercase for dedup)
        if let Ok(mut log) = ctx.state.pid_log.lock() {
            log.insert(effective_command.to_uppercase());
        }

        // Forward OBD2 command to dongle task
        // RPM extraction happens in the dongle task - it sends to rpm_led automatically
        match send_command(&ctx.state.dongle_tx, &effective_raw, timeout) {
            Ok(response) => {
                // Store this command for per-client repeat functionality
                client_state.last_obd_command = Some(effective_command.clone());

                // Format and send dongle response (echo already sent character-by-character)
                let formatted = client_state.format_response(&response);
                writer
                    .write_all(&formatted)
                    .context("Failed to write response")?;
            }
            Err(e) => {
                error!("Dongle error: {e}");
                // Send proper ELM327 error response (echo already sent)
                let le = client_state.line_ending();
                let error_msg = e.to_elm327_error();
                let error_response = format!("{le}{error_msg}{le}>");
                writer.write_all(error_response.as_bytes())?;
            }
        }

        Ok(())
    }
}
