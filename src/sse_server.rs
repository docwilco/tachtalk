//! Standalone SSE server for RPM streaming
//!
//! This runs on a separate port (8081) to avoid blocking the main HTTP server.
//! Browsers can connect via EventSource with CORS.

use log::{debug, error, info, warn};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use crate::watchdog::WatchdogHandle;

/// Port for the SSE server (separate from main HTTP server)
pub const SSE_PORT: u16 = 8081;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// Message types for SSE server
pub enum SseMessage {
    /// RPM update to broadcast
    RpmUpdate(u32),
}

/// Sender for SSE messages
pub type SseSender = Sender<SseMessage>;

/// Start the SSE server and return a sender for RPM updates
pub fn start_sse_server() -> SseSender {
    let (tx, rx) = mpsc::channel::<SseMessage>();

    std::thread::spawn(move || {
        if let Err(e) = run_sse_server(&rx) {
            error!("SSE server error: {e}");
        }
    });

    tx
}

/// SSE server main loop
fn run_sse_server(rx: &Receiver<SseMessage>) -> std::io::Result<()> {
    info!("SSE server starting on port {SSE_PORT}...");

    let listener = TcpListener::bind(("0.0.0.0", SSE_PORT))?;
    listener.set_nonblocking(true)?;

    let watchdog = WatchdogHandle::register("sse_server");

    info!("SSE server started on port {SSE_PORT}");

    let mut clients: Vec<TcpStream> = Vec::new();
    let mut current_rpm: Option<u32> = None;
    let mut last_heartbeat = Instant::now();

    loop {
        watchdog.feed();

        // Accept new connections (non-blocking)
        match listener.accept() {
            Ok((stream, addr)) => {
                info!("SSE: New connection from {addr}");
                if let Some(client) = handle_new_connection(stream) {
                    // Send current RPM to new client
                    if let Some(rpm) = current_rpm {
                        let msg = format!("data: {{\"rpm\":{rpm}}}\n\n");
                        let _ = send_to_client(&client, msg.as_bytes());
                    }
                    clients.push(client);
                    info!("SSE: Client connected (total={})", clients.len());
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No pending connections, continue
            }
            Err(e) => {
                warn!("SSE: Accept error: {e}");
            }
        }

        // Send heartbeat to detect dead connections
        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL && !clients.is_empty() {
            let before = clients.len();
            clients.retain(|client| send_to_client(client, b": heartbeat\n\n").is_ok());
            let removed = before - clients.len();
            if removed > 0 {
                info!("SSE: Heartbeat removed {removed} dead clients, {} remaining", clients.len());
            }
            last_heartbeat = Instant::now();
        }

        // Process incoming messages
        match rx.try_recv() {
            Ok(SseMessage::RpmUpdate(rpm)) => {
                current_rpm = Some(rpm);
                let msg = format!("data: {{\"rpm\":{rpm}}}\n\n");

                let before = clients.len();
                clients.retain(|client| send_to_client(client, msg.as_bytes()).is_ok());
                let removed = before - clients.len();
                if removed > 0 {
                    debug!("SSE: Removed {removed} dead clients during broadcast, {} remaining", clients.len());
                }
            }
            Err(TryRecvError::Empty) => {
                // No messages, brief sleep to prevent busy loop
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(TryRecvError::Disconnected) => {
                warn!("SSE: Channel disconnected, shutting down");
                break;
            }
        }
    }

    Ok(())
}

/// Handle a new connection - read HTTP request and send SSE headers
fn handle_new_connection(mut stream: TcpStream) -> Option<TcpStream> {
    // Set timeout for reading the HTTP request
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok()?;

    // Read the HTTP request (we don't really parse it, just consume it)
    let mut buf = [0u8; 1024];
    let n = match stream.read(&mut buf) {
        Ok(n) if n > 0 => n,
        _ => {
            debug!("SSE: Failed to read request");
            return None;
        }
    };

    // Check if it looks like an HTTP GET request
    let request = String::from_utf8_lossy(&buf[..n]);
    if !request.starts_with("GET ") {
        debug!("SSE: Not a GET request");
        return None;
    }

    // Send SSE response headers with CORS
    let response = concat!(
        "HTTP/1.1 200 OK\r\n",
        "Content-Type: text/event-stream\r\n",
        "Cache-Control: no-cache\r\n",
        "Connection: keep-alive\r\n",
        "Access-Control-Allow-Origin: *\r\n",
        "\r\n",
        "data: {\"rpm\":null}\n\n"
    );

    if stream.write_all(response.as_bytes()).is_err() {
        debug!("SSE: Failed to send headers");
        return None;
    }

    // Switch to non-blocking for the event loop
    stream.set_nonblocking(true).ok()?;
    // Clear timeouts for long-lived connection
    stream.set_read_timeout(None).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

    Some(stream)
}

/// Send data to a client, returns Ok if successful
fn send_to_client(client: &TcpStream, data: &[u8]) -> std::io::Result<()> {
    // Use a reference that implements Write
    (&*client).write_all(data)?;
    (&*client).flush()
}
