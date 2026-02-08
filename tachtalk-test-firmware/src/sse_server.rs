//! Standalone SSE server for test metrics streaming
//!
//! This runs on a separate port (81) to avoid blocking the main HTTP server.
//! Browsers can connect via EventSource with CORS.

use log::{debug, error, info, warn};
use smallvec::SmallVec;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::watchdog::WatchdogHandle;
use crate::State;

/// Port for the SSE server (separate from main HTTP server)
pub const SSE_PORT: u16 = 81;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
const METRICS_INTERVAL: Duration = Duration::from_millis(500);
const MAX_SSE_CLIENTS: usize = 3;

/// Message types for SSE server (currently unused, but kept for future expansion)
pub enum SseMessage {
    /// Generic update notification
    Update,
}

/// Sender for SSE messages
pub type SseSender = Sender<SseMessage>;
pub type SseReceiver = Receiver<SseMessage>;

/// Run the SSE server task
pub fn sse_server_task(rx: &SseReceiver, state: &Arc<State>) {
    if let Err(e) = run_sse_server(rx, state) {
        error!("SSE server error: {e}");
    }
}

/// SSE server main loop
fn run_sse_server(rx: &Receiver<SseMessage>, state: &Arc<State>) -> std::io::Result<()> {
    info!("SSE server starting on port {SSE_PORT}...");

    let listener = TcpListener::bind(("0.0.0.0", SSE_PORT))?;
    listener.set_nonblocking(true)?;

    let watchdog = WatchdogHandle::register("sse_server");

    info!("SSE server started on port {SSE_PORT}");

    let mut clients: SmallVec<[TcpStream; MAX_SSE_CLIENTS]> = SmallVec::new();
    let mut last_heartbeat = Instant::now();
    let mut last_metrics = Instant::now();

    loop {
        watchdog.feed();

        // Accept new connections (non-blocking)
        match listener.accept() {
            Ok((stream, addr)) => {
                info!("SSE: New connection from {addr}");
                if let Some(client_stream) = handle_new_connection(stream) {
                    // Enforce max client limit - close oldest if at capacity
                    if clients.len() >= MAX_SSE_CLIENTS {
                        if let Some(oldest) = clients.first() {
                            info!("SSE: Max clients reached ({MAX_SSE_CLIENTS}), closing oldest connection");
                            let _ = oldest.shutdown(std::net::Shutdown::Both);
                        }
                        clients.remove(0);
                    }

                    // Send current metrics to new client
                    let msg = build_metrics_message(state);
                    let _ = send_to_client(&client_stream, msg.as_bytes());
                    clients.push(client_stream);
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

        // Send metrics periodically
        if last_metrics.elapsed() >= METRICS_INTERVAL && !clients.is_empty() {
            let msg = build_metrics_message(state);
            let before = clients.len();
            clients.retain(|client| send_to_client(client, msg.as_bytes()).is_ok());
            let removed = before - clients.len();
            if removed > 0 {
                debug!("SSE: Removed {removed} dead clients during metrics broadcast, {} remaining", clients.len());
            }
            last_metrics = Instant::now();
        }

        // Process incoming messages (drain channel but don't do anything special)
        match rx.try_recv() {
            Ok(SseMessage::Update) => {
                // Metrics will be sent on next interval
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

/// Build metrics JSON message for SSE
fn build_metrics_message(state: &State) -> String {
    let metrics = &state.metrics;
    
    let test_running = metrics.test_running.load(Ordering::Relaxed);
    let requests_per_sec = metrics.requests_per_sec.load(Ordering::Relaxed);
    let total_requests = metrics.total_requests.load(Ordering::Relaxed);
    let total_errors = metrics.total_errors.load(Ordering::Relaxed);
    let dongle_connected = state.dongle_connected.load(Ordering::Relaxed);
    
    // Mode 5 specific
    let bytes_captured = metrics.bytes_captured.load(Ordering::Relaxed);
    let records_captured = metrics.records_captured.load(Ordering::Relaxed);
    let buffer_usage_pct = metrics.buffer_usage_pct.load(Ordering::Relaxed);
    let client_connected = metrics.client_connected.load(Ordering::Relaxed);
    let capture_overflow = metrics.capture_overflow.load(Ordering::Relaxed);
    
    format!(
        "event: metrics\ndata: {{\"test_running\":{test_running},\"requests_per_sec\":{requests_per_sec},\"total_requests\":{total_requests},\"total_errors\":{total_errors},\"dongle_connected\":{dongle_connected},\"bytes_captured\":{bytes_captured},\"records_captured\":{records_captured},\"buffer_usage_pct\":{buffer_usage_pct},\"client_connected\":{client_connected},\"capture_overflow\":{capture_overflow}}}\n\n"
    )
}

/// Handle a new connection - read HTTP request and send SSE headers
fn handle_new_connection(mut stream: TcpStream) -> Option<TcpStream> {
    // Set timeout for reading the HTTP request
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok()?;

    let mut buf = [0u8; 512];
    let mut total_read = 0;

    // Read until we see the end of HTTP headers (\r\n\r\n)
    loop {
        match stream.read(&mut buf[total_read..]) {
            Ok(0) => {
                debug!("SSE: Client disconnected during handshake");
                return None;
            }
            Ok(n) => {
                total_read += n;
                // Check if we have complete headers
                if let Some(_pos) = find_header_end(&buf[..total_read]) {
                    break;
                }
                if total_read >= buf.len() {
                    warn!("SSE: Request too large");
                    return None;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                debug!("SSE: Read error during handshake: {e}");
                return None;
            }
        }
    }

    // Parse request to check for /events path
    let request = String::from_utf8_lossy(&buf[..total_read]);
    if !request.starts_with("GET /events") && !request.starts_with("GET / ") {
        debug!("SSE: Invalid path in request");
        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\n\r\n");
        return None;
    }

    // Send SSE response headers
    let headers = concat!(
        "HTTP/1.1 200 OK\r\n",
        "Content-Type: text/event-stream\r\n",
        "Cache-Control: no-cache\r\n",
        "Connection: keep-alive\r\n",
        "Access-Control-Allow-Origin: *\r\n",
        "\r\n"
    );

    if stream.write_all(headers.as_bytes()).is_err() {
        debug!("SSE: Failed to send headers");
        return None;
    }

    // Configure for non-blocking async operation
    stream.set_nonblocking(true).ok()?;

    Some(stream)
}

/// Find end of HTTP headers (\r\n\r\n)
fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|pos| pos + 4)
}

/// Send data to a client, returning Ok(()) on success
fn send_to_client(client: &TcpStream, data: &[u8]) -> Result<(), ()> {
    // Use a reference to avoid moving ownership
    let mut writer = client;
    writer.write_all(data).map_err(|_| ())?;
    writer.flush().map_err(|_| ())
}
