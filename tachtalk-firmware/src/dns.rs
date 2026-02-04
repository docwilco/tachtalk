//! Captive portal DNS server.
//!
//! Responds to all DNS queries with the AP's IP address to enable captive portal detection.

use crate::watchdog::WatchdogHandle;
use log::{debug, error, info, warn};
use std::net::{Ipv4Addr, UdpSocket};
use std::time::Duration;

const DNS_PORT: u16 = 53;
const AP_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 71, 1);

/// Start the captive portal DNS server in a background thread.
///
/// The server responds to all A record queries with the AP IP address,
/// causing all DNS lookups to resolve to the captive portal.
pub fn start_dns_server() {
    crate::thread_util::spawn_named(c"dns_srv", || {
        if let Err(e) = run_dns_server() {
            error!("DNS server error: {e}");
        }
    });
}

fn run_dns_server() -> std::io::Result<()> {
    info!("DNS server starting on port {DNS_PORT}...");
    
    let socket = UdpSocket::bind(("0.0.0.0", DNS_PORT))?;
    // Set timeout to ~3s so we feed watchdog well within 5s default timeout
    socket.set_read_timeout(Some(Duration::from_secs(3)))?;
    
    let watchdog = WatchdogHandle::register("dns_server");
    
    info!("DNS server started");

    let mut buf = [0u8; 512];

    loop {
        watchdog.feed();
        
        let (len, src) = match socket.recv_from(&mut buf) {
            Ok(result) => result,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                // Timeout - just continue to feed watchdog
                continue;
            }
            Err(e) => {
                warn!("DNS recv error: {e}");
                continue;
            }
        };

        if len < 12 {
            debug!("DNS query too short ({len} bytes)");
            continue;
        }

        let query = &buf[..len];
        
        // Log DNS query with parsed name
        let name = parse_dns_name(&query[12..]).unwrap_or_else(|| "<invalid>".to_string());
        info!("DNS: {name} from {}", src.ip());

        // Build response
        if let Some(response) = build_dns_response(query) {
            if let Err(e) = socket.send_to(&response, src) {
                warn!("DNS send error: {e}");
            }
        }
    }
}

/// Build a DNS response that answers all A queries with the AP IP.
fn build_dns_response(query: &[u8]) -> Option<Vec<u8>> {
    if query.len() < 12 {
        return None;
    }

    // Parse header
    let id = &query[0..2];
    let flags = u16::from_be_bytes([query[2], query[3]]);
    let qdcount = u16::from_be_bytes([query[4], query[5]]);

    // Check if it's a standard query (QR=0, Opcode=0)
    if (flags & 0x7800) != 0 {
        debug!("DNS: Not a standard query, ignoring");
        return None;
    }

    if qdcount == 0 {
        debug!("DNS: No questions in query");
        return None;
    }

    // Find the end of the question section (skip the QNAME)
    let mut pos = 12;
    while pos < query.len() && query[pos] != 0 {
        let label_len = query[pos] as usize;
        if label_len > 63 {
            // Compression pointer or invalid
            debug!("DNS: Invalid label length");
            return None;
        }
        pos += 1 + label_len;
    }

    if pos >= query.len() {
        return None;
    }

    // Skip null terminator
    pos += 1;

    // Check QTYPE and QCLASS (need at least 4 more bytes)
    if pos + 4 > query.len() {
        return None;
    }

    let _qtype = u16::from_be_bytes([query[pos], query[pos + 1]]);
    let qclass = u16::from_be_bytes([query[pos + 2], query[pos + 3]]);
    pos += 4;

    // Only respond to A record (type 1) queries for IN class (class 1)
    // But for captive portal, we'll respond to all queries
    if qclass != 1 {
        debug!("DNS: Non-IN class query, ignoring");
        return None;
    }

    // Build response
    let mut response = Vec::with_capacity(query.len() + 16);

    // Header
    response.extend_from_slice(id); // Transaction ID
    response.extend_from_slice(&0x8180u16.to_be_bytes()); // Flags: QR=1, AA=1, RD=1, RA=1
    response.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT = 1
    response.extend_from_slice(&1u16.to_be_bytes()); // ANCOUNT = 1
    response.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT = 0
    response.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT = 0

    // Question section (copy from query)
    response.extend_from_slice(&query[12..pos]);

    // Answer section
    response.extend_from_slice(&0xC00Cu16.to_be_bytes()); // Name pointer to offset 12
    response.extend_from_slice(&1u16.to_be_bytes()); // TYPE = A (1)
    response.extend_from_slice(&1u16.to_be_bytes()); // CLASS = IN (1)
    response.extend_from_slice(&60u32.to_be_bytes()); // TTL = 60 seconds
    response.extend_from_slice(&4u16.to_be_bytes()); // RDLENGTH = 4 (IPv4)
    response.extend_from_slice(&AP_IP.octets()); // RDATA = AP IP

    Some(response)
}

/// Parse a DNS name from the query for logging purposes.
fn parse_dns_name(data: &[u8]) -> Option<String> {
    let mut name = String::new();
    let mut pos = 0;

    while pos < data.len() && data[pos] != 0 {
        let label_len = data[pos] as usize;
        if label_len > 63 || pos + 1 + label_len > data.len() {
            return None;
        }

        if !name.is_empty() {
            name.push('.');
        }

        let label = &data[pos + 1..pos + 1 + label_len];
        name.push_str(&String::from_utf8_lossy(label));
        pos += 1 + label_len;
    }

    Some(name)
}
