use anyhow::Result;
use embedded_svc::http::Method;
use embedded_svc::io::Write;
use esp_idf_svc::http::server::{Configuration, EspHttpServer};
use log::{debug, error, info, warn};
use std::sync::atomic::Ordering;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;

use esp_idf_svc::sys::{esp_get_free_heap_size, esp_get_minimum_free_heap_size};

use crate::rpm_leds::RpmTaskMessage;
use crate::sse_server::SSE_PORT;
use crate::State;

/// IP configuration for WiFi request
#[derive(serde::Deserialize)]
struct IpConfigRequest {
    use_dhcp: bool,
    #[serde(default = "default_static_ip")]
    ip: String,
    #[serde(default = "default_prefix_len")]
    prefix_len: u8,
}

fn default_static_ip() -> String {
    "192.168.0.20".to_string()
}

const fn default_prefix_len() -> u8 {
    24
}

/// WiFi configuration request from web UI
#[derive(serde::Deserialize)]
struct WifiRequest {
    ssid: String,
    password: Option<String>,
    ip: Option<IpConfigRequest>,
}

/// Brightness change request from web UI
#[derive(serde::Deserialize)]
struct BrightnessRequest {
    brightness: u8,
    #[serde(default)]
    save: bool,
}

/// WiFi network scan result
#[derive(serde::Serialize)]
struct Network {
    ssid: String,
    rssi: i8,
}

/// Network status response
#[derive(serde::Serialize)]
struct NetworkStatus {
    ssid: Option<String>,
    ip: Option<String>,
    mac: String,
    rssi: Option<i8>,
}

/// Connection status for the diagram
#[derive(serde::Serialize)]
struct ConnectionStatus {
    /// WiFi connected to OBD2 dongle network
    wifi_connected: bool,
    /// TCP connection to OBD2 dongle established
    dongle_tcp_connected: bool,
    /// Number of downstream OBD2 clients connected
    obd2_client_count: u32,
}

/// TCP connection info for a single connection
#[derive(serde::Serialize)]
struct TcpConnectionInfo {
    local: String,
    remote: String,
}

/// TCP connections status
#[derive(serde::Serialize)]
struct TcpStatus {
    /// Dongle connection (if connected)
    dongle: Option<TcpConnectionInfo>,
    /// Client connections
    clients: Vec<TcpConnectionInfo>,
}

/// Socket type enumeration
#[derive(serde::Serialize)]
enum SocketType {
    Tcp,
    Udp,
    Unknown(i32),
}

/// Information about a single socket
#[derive(serde::Serialize)]
struct SocketInfo {
    fd: i32,
    socket_type: SocketType,
    local: Option<String>,
    remote: Option<String>,
}

/// Enumerate all open LWIP sockets
///
/// LWIP sockets use FDs starting at `LWIP_SOCKET_OFFSET` (48 on ESP32).
/// We probe each potential FD with `getsockopt` to see if it's a valid socket.
#[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
fn enumerate_sockets() -> Vec<SocketInfo> {
    use esp_idf_svc::sys::{lwip_getsockopt, SOL_SOCKET, SO_TYPE, SOCK_STREAM, SOCK_DGRAM, 
                           LWIP_SOCKET_OFFSET, CONFIG_LWIP_MAX_SOCKETS};

    let socket_offset = LWIP_SOCKET_OFFSET as i32;
    let max_sockets = CONFIG_LWIP_MAX_SOCKETS as i32;
    let mut sockets = Vec::new();

    for fd in socket_offset..(socket_offset + max_sockets) {
        // Try to get socket type - if this fails, FD is not a valid socket
        let mut sock_type: i32 = 0;
        let mut optlen: u32 = std::mem::size_of::<i32>() as u32;

        let result = unsafe {
            lwip_getsockopt(
                fd,
                SOL_SOCKET as i32,
                SO_TYPE as i32,
                std::ptr::addr_of_mut!(sock_type).cast(),
                &mut optlen,
            )
        };

        if result != 0 {
            continue; // Not a valid socket
        }

        let socket_type = match sock_type {
            x if x == SOCK_STREAM as i32 => SocketType::Tcp,
            x if x == SOCK_DGRAM as i32 => SocketType::Udp,
            x => SocketType::Unknown(x),
        };

        // Get local address
        let local = get_socket_addr(fd, false);
        // Get remote address (for connected sockets)
        let remote = get_socket_addr(fd, true);

        sockets.push(SocketInfo {
            fd,
            socket_type,
            local,
            remote,
        });
    }

    sockets
}

/// Get local or remote address of a socket
#[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
fn get_socket_addr(fd: i32, peer: bool) -> Option<String> {
    use esp_idf_svc::sys::{lwip_getpeername, lwip_getsockname, sockaddr_in, sockaddr, AF_INET};
    use std::mem::MaybeUninit;

    let mut addr: MaybeUninit<sockaddr_in> = MaybeUninit::uninit();
    let mut addrlen: u32 = std::mem::size_of::<sockaddr_in>() as u32;

    let result = unsafe {
        if peer {
            lwip_getpeername(fd, addr.as_mut_ptr().cast::<sockaddr>(), &mut addrlen)
        } else {
            lwip_getsockname(fd, addr.as_mut_ptr().cast::<sockaddr>(), &mut addrlen)
        }
    };

    if result != 0 {
        return None;
    }

    let addr = unsafe { addr.assume_init() };

    // Check if it's an IPv4 address
    #[allow(clippy::unnecessary_cast)]
    if i32::from(addr.sin_family) != AF_INET as i32 {
        return None;
    }

    // Convert to string - sin_addr.s_addr is in network byte order
    let ip = std::net::Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
    let port = u16::from_be(addr.sin_port);

    Some(format!("{ip}:{port}"))
}

/// Log all open sockets to console (for debugging FD exhaustion)
pub fn log_sockets() {
    let sockets = enumerate_sockets();
    if sockets.is_empty() {
        warn!("No open sockets found (unexpected)");
        return;
    }
    
    warn!("Open sockets ({}/{}):", sockets.len(), esp_idf_svc::sys::CONFIG_LWIP_MAX_SOCKETS);
    for s in &sockets {
        let type_str = match &s.socket_type {
            SocketType::Tcp => "TCP",
            SocketType::Udp => "UDP",
            SocketType::Unknown(t) => {
                warn!("  fd={}: type={} (unknown)", s.fd, t);
                continue;
            }
        };
        let local = s.local.as_deref().unwrap_or("?");
        let remote = s.remote.as_deref().unwrap_or("-");
        warn!("  fd={}: {} {} -> {}", s.fd, type_str, local, remote);
    }
}

/// Debug info response
#[derive(serde::Serialize)]
struct DebugInfo {
    at_commands: Vec<String>,
    pids: Vec<String>,
    free_heap: u32,
    min_free_heap: u32,
}

/// Polling metrics response
#[derive(serde::Serialize)]
struct PollingMetricsResponse {
    fast_pid_count: u32,
    slow_pid_count: u32,
    promotions: u32,
    demotions: u32,
    removals: u32,
    dongle_requests_total: u32,
    dongle_requests_per_sec: u32,
}

// HTML split into two parts to inject SSE_PORT without runtime allocation
// Generated by build.rs from src/index.html
const HTML_INDEX_START: &str = include_str!(concat!(env!("OUT_DIR"), "/index_start.html"));
const HTML_INDEX_END: &str = include_str!(concat!(env!("OUT_DIR"), "/index_end.html"));

#[allow(clippy::too_many_lines)] // Route registration function - length is proportional to endpoints
pub fn start_server(state: &Arc<State>, ap_hostname: Option<String>, ap_ip: Ipv4Addr) -> Result<()> {
    info!("Web server starting...");
    
    // Enable wildcard URI matching for captive portal fallback handler
    // Enable LRU purge to handle abrupt disconnections from captive portal browsers
    // LWIP max is 16 sockets; leave room for DNS, SSE, mDNS, OBD2 proxy, dongle, httpd control
    let server_config = Configuration {
        uri_match_wildcard: true,
        max_open_sockets: 6,
        session_timeout: core::time::Duration::from_secs(2),
        lru_purge_enable: true,
        ..Default::default()
    };
    let mut server = EspHttpServer::new(&server_config)?;

    // Serve the main HTML page (inject SSE port between two static parts)
    server.fn_handler("/", Method::Get, |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        let mut response = req.into_ok_response()?;
        response.write_all(HTML_INDEX_START.as_bytes())?;
        response.write_all(SSE_PORT.to_string().as_bytes())?;
        response.write_all(HTML_INDEX_END.as_bytes())?;
        Ok(())
    })?;

    // GET config endpoint
    let state_clone = state.clone();
    server.fn_handler("/api/config", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        info!("HTTP: GET /api/config");
        let cfg = state_clone.config.lock().unwrap();
        let json = serde_json::to_string(&*cfg).unwrap();
        
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // POST config endpoint
    let state_clone = state.clone();
    server.fn_handler("/api/config", Method::Post, move |mut req| -> Result<(), esp_idf_svc::io::EspIOError> {
        info!("HTTP: POST /api/config");
        let mut buf = vec![0u8; 8192];
        let bytes_read = req.read(&mut buf)?;
        
        if let Ok(mut new_config) = serde_json::from_slice::<crate::config::Config>(&buf[..bytes_read]) {
            // Validate/clamp values to safe ranges
            new_config.validate();
            
            debug!("Config update: {} thresholds, log_level={:?}", 
                   new_config.thresholds.len(), new_config.log_level);
            
            let old_gpio = {
                let cfg = state_clone.config.lock().unwrap();
                if cfg.led_gpio == new_config.led_gpio {
                    None
                } else {
                    Some(cfg.led_gpio)
                }
            };
            
            {
                let mut cfg = state_clone.config.lock().unwrap();
                *cfg = new_config;
                if let Err(e) = cfg.save() {
                    warn!("Failed to save config: {e}");
                }
            }
            
            // Notify RPM task of config change (to recalculate render interval)
            let _ = state_clone.rpm_tx.send(RpmTaskMessage::ConfigChanged);
            
            if let Some(old_gpio) = old_gpio {
                info!("LED GPIO changed from {old_gpio} to new value, resetting old pin and restarting in 2 seconds...");
                // Reset the OLD GPIO to clear RMT routing before restart
                unsafe { esp_idf_svc::sys::gpio_reset_pin(i32::from(old_gpio)); }
                let mut response = req.into_ok_response()?;
                response.write_all(b"{\"restart\":true}")?;
                crate::thread_util::spawn_named(c"restart", || {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    unsafe { esp_idf_svc::sys::esp_restart(); }
                });
            } else {
                req.into_ok_response()?;
            }
        } else {
            warn!("Invalid config JSON received");
            req.into_status_response(400)?;
        }
        
        Ok(())
    })?;

    // POST brightness endpoint - immediate brightness change without saving
    let state_clone = state.clone();
    server.fn_handler("/api/brightness", Method::Post, move |mut req| -> Result<(), esp_idf_svc::io::EspIOError> {
        debug!("HTTP: POST /api/brightness");
        let mut buf = [0u8; 32];
        let bytes_read = req.read(&mut buf)?;
        
        if let Ok(brightness_req) = serde_json::from_slice::<BrightnessRequest>(&buf[..bytes_read]) {
            debug!("Brightness update: {} (save={})", brightness_req.brightness, brightness_req.save);
            
            // Send brightness to LED task immediately
            let _ = state_clone.rpm_tx.send(RpmTaskMessage::Brightness(brightness_req.brightness));
            
            // Optionally save to config
            if brightness_req.save {
                let mut cfg = state_clone.config.lock().unwrap();
                cfg.brightness = brightness_req.brightness;
                if let Err(e) = cfg.save() {
                    warn!("Failed to save brightness config: {e}");
                }
            }
            
            req.into_ok_response()?;
        } else {
            warn!("Invalid brightness JSON received");
            req.into_status_response(400)?;
        }
        
        Ok(())
    })?;

    // POST wifi endpoint - save wifi and restart
    let state_clone = state.clone();
    server.fn_handler("/api/wifi", Method::Post, move |mut req| -> Result<(), esp_idf_svc::io::EspIOError> {
        info!("HTTP: POST /api/wifi");
        let mut buf = vec![0u8; 1024];
        let bytes_read = req.read(&mut buf)?;
        
        if let Ok(wifi_req) = serde_json::from_slice::<WifiRequest>(&buf[..bytes_read]) {
            info!("WiFi config update: ssid={:?}, dhcp={:?}", 
                  wifi_req.ssid, wifi_req.ip.as_ref().map(|i| i.use_dhcp));
            let mut cfg = state_clone.config.lock().unwrap();
            cfg.wifi.ssid = wifi_req.ssid;
            cfg.wifi.password = wifi_req.password.filter(|p| !p.is_empty());
            
            // Update IP config if provided
            if let Some(ip_cfg) = wifi_req.ip {
                cfg.ip.use_dhcp = ip_cfg.use_dhcp;
                cfg.ip.ip = if ip_cfg.ip.is_empty() {
                    "192.168.0.20".to_string()
                } else {
                    ip_cfg.ip
                };
                cfg.ip.prefix_len = ip_cfg.prefix_len;
            }
            
            if let Err(e) = cfg.save() {
                error!("Failed to save config: {e:?}");
                req.into_status_response(500)?;
                return Ok(());
            }
            
            req.into_ok_response()?;
            
            // Schedule restart after response is sent
            info!("WiFi configured, restarting in 2 seconds...");
            crate::thread_util::spawn_named(c"restart", || {
                std::thread::sleep(std::time::Duration::from_secs(2));
                unsafe {
                    esp_idf_svc::sys::esp_restart();
                }
            });
        } else {
            req.into_status_response(400)?;
        }
        
        Ok(())
    })?;

    // GET wifi scan endpoint
    let state_clone = state.clone();
    server.fn_handler("/api/wifi/scan", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        info!("HTTP: GET /api/wifi/scan");
        let mut wifi = state_clone.wifi.lock().unwrap();
        
        let networks: Vec<Network> = match wifi.scan() {
            Ok(aps) => {
                debug!("WiFi scan found {} networks", aps.len());
                // Deduplicate by SSID, keeping strongest signal
                let mut best_by_ssid: HashMap<String, i8> = HashMap::new();
                for ap in &aps {
                    let ssid = ap.ssid.to_string();
                    if ssid.is_empty() {
                        continue;
                    }
                    best_by_ssid
                        .entry(ssid)
                        .and_modify(|rssi| *rssi = (*rssi).max(ap.signal_strength))
                        .or_insert(ap.signal_strength);
                }
                // Convert to vec and sort by signal strength (strongest first)
                let mut networks: Vec<Network> = best_by_ssid
                    .into_iter()
                    .map(|(ssid, rssi)| Network { ssid, rssi })
                    .collect();
                networks.sort_by(|a, b| b.rssi.cmp(&a.rssi));
                networks
            }
            Err(e) => {
                error!("WiFi scan failed: {e:?}");
                Vec::new()
            }
        };
        
        let json = serde_json::to_string(&networks).unwrap_or_else(|_| "[]".to_string());
        
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // GET network status endpoint
    let state_clone = state.clone();
    server.fn_handler("/api/network", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        info!("HTTP: GET /api/network");
        let wifi = state_clone.wifi.lock().unwrap();
        
        let sta_netif = wifi.sta_netif();
        let ip_info = sta_netif.get_ip_info().ok();
        
        let mac_bytes = wifi.driver().get_mac(esp_idf_svc::wifi::WifiDeviceId::Sta).unwrap_or([0u8; 6]);
        let mac = format!("{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            mac_bytes[0], mac_bytes[1], mac_bytes[2],
            mac_bytes[3], mac_bytes[4], mac_bytes[5]);
        
        // Get SSID and RSSI from current STA connection info
        let (ssid, rssi) = if wifi.is_connected().unwrap_or(false) {
            let mut ap_info: esp_idf_svc::sys::wifi_ap_record_t = unsafe { std::mem::zeroed() };
            let result = unsafe { esp_idf_svc::sys::esp_wifi_sta_get_ap_info(&mut ap_info) };
            if result == esp_idf_svc::sys::ESP_OK {
                let ssid_bytes = &ap_info.ssid;
                let ssid_len = ssid_bytes.iter().position(|&b| b == 0).unwrap_or(ssid_bytes.len());
                let ssid = String::from_utf8_lossy(&ssid_bytes[..ssid_len]).to_string();
                (Some(ssid), Some(ap_info.rssi))
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };
        
        let status = NetworkStatus {
            ssid,
            ip: ip_info.as_ref().map(|i| {
                format!("{}/{}", i.ip, i.subnet.mask.0)
            }),
            mac,
            rssi,
        };
        
        let json = serde_json::to_string(&status).unwrap_or_else(|_| "{}".to_string());
        
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // GET connection status endpoint for diagram
    let state_clone = state.clone();
    server.fn_handler("/api/status", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        use std::sync::atomic::Ordering;
        
        debug!("HTTP: GET /api/status");
        
        let wifi_connected = state_clone.wifi.lock().unwrap().is_connected().unwrap_or(false);
        let dongle_tcp_connected = state_clone.dongle_connected.load(Ordering::Relaxed);
        let obd2_client_count = state_clone.obd2_client_count.load(Ordering::Relaxed);
        
        let status = ConnectionStatus {
            wifi_connected,
            dongle_tcp_connected,
            obd2_client_count,
        };
        
        let json = serde_json::to_string(&status).unwrap_or_else(|_| "{}".to_string());
        
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // GET TCP connection details endpoint
    let state_clone = state.clone();
    server.fn_handler("/api/tcp", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        debug!("HTTP: GET /api/tcp");
        
        let dongle = state_clone.dongle_tcp_info.lock().unwrap()
            .map(|(local, remote)| TcpConnectionInfo {
                local: local.to_string(),
                remote: remote.to_string(),
            });
        
        let clients: Vec<TcpConnectionInfo> = state_clone.client_tcp_info.lock().unwrap()
            .iter()
            .map(|(local, remote)| TcpConnectionInfo {
                local: local.to_string(),
                remote: remote.to_string(),
            })
            .collect();
        
        let status = TcpStatus { dongle, clients };
        let json = serde_json::to_string(&status).unwrap_or_else(|_| "{}".to_string());
        
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // GET all open sockets endpoint (for debugging FD exhaustion)
    server.fn_handler("/api/sockets", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        debug!("HTTP: GET /api/sockets");
        
        let sockets = enumerate_sockets();
        let json = serde_json::to_string(&sockets).unwrap_or_else(|_| "[]".to_string());
        
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // GET RPM endpoint (fallback for non-SSE clients)
    let state_clone = state.clone();
    server.fn_handler("/api/rpm", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        debug!("HTTP: GET /api/rpm");
        let rpm = state_clone.shared_rpm.lock().unwrap();
        let json = match *rpm {
            Some(r) => format!(r#"{{"rpm":{r}}}"#),
            None => r#"{"rpm":null}"#.to_string(),
        };
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // GET debug info endpoint (AT commands, PIDs, memory stats, etc.)
    let state_clone = state.clone();
    server.fn_handler("/api/debug_info", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        debug!("HTTP: GET /api/debug_info");
        
        let at_commands: Vec<String> = state_clone.at_command_log
            .lock()
            .map(|log| {
                let mut cmds: Vec<String> = log.iter().cloned().collect();
                cmds.sort();
                cmds
            })
            .unwrap_or_default();
        
        let pids: Vec<String> = state_clone.pid_log
            .lock()
            .map(|log| {
                let mut pids: Vec<String> = log.iter().cloned().collect();
                pids.sort();
                pids
            })
            .unwrap_or_default();
        
        // SAFETY: These are simple C functions that return u32 values
        let free_heap = unsafe { esp_get_free_heap_size() };
        let min_free_heap = unsafe { esp_get_minimum_free_heap_size() };
        
        let info = DebugInfo {
            at_commands,
            pids,
            free_heap,
            min_free_heap,
        };
        
        let json = serde_json::to_string(&info).unwrap_or_else(|_| r#"{"at_commands":[],"free_heap":0,"min_free_heap":0}"#.to_string());
        
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // GET polling metrics endpoint
    let state_clone = state.clone();
    server.fn_handler("/api/metrics", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        debug!("HTTP: GET /api/metrics");
        
        let metrics = &state_clone.polling_metrics;
        let response_data = PollingMetricsResponse {
            fast_pid_count: metrics.fast_pid_count.load(Ordering::Relaxed),
            slow_pid_count: metrics.slow_pid_count.load(Ordering::Relaxed),
            promotions: metrics.promotions.load(Ordering::Relaxed),
            demotions: metrics.demotions.load(Ordering::Relaxed),
            removals: metrics.removals.load(Ordering::Relaxed),
            dongle_requests_total: metrics.dongle_requests_total.load(Ordering::Relaxed),
            dongle_requests_per_sec: metrics.dongle_requests_per_sec.load(Ordering::Relaxed),
        };
        
        let json = serde_json::to_string(&response_data).unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string());
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // POST reboot endpoint
    let state_clone = state.clone();
    server.fn_handler("/api/reboot", Method::Post, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        info!("HTTP: POST /api/reboot - Device reboot requested");
        
        req.into_ok_response()?;
        
        // Schedule restart after response is sent
        let state = state_clone.clone();
        crate::thread_util::spawn_named(c"restart", move || {
            std::thread::sleep(std::time::Duration::from_secs(1));
            
            // Stop WiFi before restarting to ensure clean shutdown
            info!("Stopping WiFi before reboot...");
            if let Ok(mut wifi) = state.wifi.lock() {
                if let Err(e) = wifi.stop() {
                    warn!("Failed to stop WiFi: {e:?}");
                }
            }
            
            info!("Rebooting device now...");
            unsafe {
                esp_idf_svc::sys::esp_restart();
            }
        });
        
        Ok(())
    })?;

    // Captive portal fallback handler - redirect requests with wrong Host header
    // Must be registered last as it's a wildcard that matches everything
    if let Some(hostname) = ap_hostname {
        let ap_ip_str = ap_ip.to_string();
        let valid_hosts: Vec<String> = vec![
            hostname.clone(),
            format!("{hostname}.local"),
            ap_ip_str.clone(),
        ];
        let redirect_url = format!("http://{ap_ip_str}/");
        let captive_portal_html = format!(
            r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="UTF-8">
    <title>TachTalk Setup</title>
    <meta http-equiv="refresh" content="0;url={redirect_url}">
</head>
<body>
    <p>Redirecting to <a href="{redirect_url}">TachTalk Setup</a>...</p>
</body>
</html>
"#
        );
        
        info!("Captive portal enabled for hostname: {hostname}");
        
        server.fn_handler("/*", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
            // Check Host header
            let host = req.header("Host").unwrap_or("");
            let host_lower = host.to_lowercase();
            // Strip port if present
            let host_without_port = host_lower.split(':').next().unwrap_or("");
            
            let is_valid_host = valid_hosts.iter().any(|h| h == host_without_port);
            
            if is_valid_host {
                // Valid host but unknown path - return 404
                info!("HTTP: GET {} -> 404 (host: {})", req.uri(), host);
                req.into_status_response(404)?;
            } else {
                // Wrong host - redirect to captive portal
                info!("HTTP: GET {} -> 302 captive (host: {})", req.uri(), host);
                let mut response = req.into_response(302, Some("Found"), &[
                    ("Location", &redirect_url),
                    ("Cache-Control", "no-cache"),
                    ("Connection", "close"),
                ])?;
                response.write_all(captive_portal_html.as_bytes())?;
            }
            Ok(())
        })?;
    }

    info!("Web server started on http://0.0.0.0:80");
    
    // Keep server alive
    std::mem::forget(server);
    
    Ok(())
}
