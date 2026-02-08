//! Web server for test firmware configuration and control

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

use crate::sse_server::SSE_PORT;
use crate::config::Config;
use crate::{State, TestControlMessage};

/// Check if a config change would require a device restart
fn check_restart_needed(current: &Config, new: &Config) -> bool {
    let wifi_changed = current.wifi.ssid != new.wifi.ssid
        || current.wifi.password != new.wifi.password;
    let ip_changed = current.ip.use_dhcp != new.ip.use_dhcp
        || current.ip.ip != new.ip.ip
        || current.ip.prefix_len != new.ip.prefix_len;
    let ap_changed = current.ap_ssid != new.ap_ssid
        || current.ap_password != new.ap_password
        || current.ap_ip != new.ap_ip
        || current.ap_prefix_len != new.ap_prefix_len;
    
    wifi_changed || ip_changed || ap_changed
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

/// Test status response
#[derive(serde::Serialize)]
#[allow(clippy::struct_excessive_bools)]
struct TestStatus {
    test_running: bool,
    query_mode: u8,
    requests_per_sec: u32,
    total_requests: u32,
    total_errors: u32,
    dongle_connected: bool,
    // Mode 5 specific
    bytes_captured: u32,
    records_captured: u32,
    buffer_usage_pct: u32,
    client_connected: bool,
    capture_overflow: bool,
}

/// Debug info response
#[derive(serde::Serialize)]
struct DebugInfo {
    free_heap: u32,
    min_free_heap: u32,
}

/// Capture status response
#[derive(serde::Serialize)]
struct CaptureStatus {
    buffer_bytes: u32,
    buffer_capacity: u32,
    records: u32,
    overflow: bool,
    test_running: bool,
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
#[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
fn enumerate_sockets() -> Vec<SocketInfo> {
    use esp_idf_svc::sys::{lwip_getsockopt, SOL_SOCKET, SO_TYPE, SOCK_STREAM, SOCK_DGRAM, 
                           LWIP_SOCKET_OFFSET, CONFIG_LWIP_MAX_SOCKETS};

    let socket_offset = LWIP_SOCKET_OFFSET as i32;
    let max_sockets = CONFIG_LWIP_MAX_SOCKETS as i32;
    let mut sockets = Vec::new();

    for fd in socket_offset..(socket_offset + max_sockets) {
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
            continue;
        }

        let socket_type = match sock_type {
            x if x == SOCK_STREAM as i32 => SocketType::Tcp,
            x if x == SOCK_DGRAM as i32 => SocketType::Udp,
            x => SocketType::Unknown(x),
        };

        let local = get_socket_addr(fd, false);
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

    #[allow(clippy::unnecessary_cast)]
    if i32::from(addr.sin_family) != AF_INET as i32 {
        return None;
    }

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

// HTML split into two parts to inject SSE_PORT without runtime allocation
// Generated by build.rs from src/index.html
const HTML_INDEX_START: &str = include_str!(concat!(env!("OUT_DIR"), "/index_start.html"));
const HTML_INDEX_END: &str = include_str!(concat!(env!("OUT_DIR"), "/index_end.html"));

#[allow(clippy::too_many_lines)]
pub fn start_server(state: &Arc<State>, ap_hostname: Option<String>, ap_ip: Ipv4Addr) -> Result<()> {
    info!("Web server starting...");
    
    let server_config = Configuration {
        uri_match_wildcard: true,
        max_open_sockets: 6,
        session_timeout: core::time::Duration::from_secs(2),
        lru_purge_enable: true,
        ..Default::default()
    };
    let mut server = EspHttpServer::new(&server_config)?;

    // Serve the main HTML page
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
        let cfg_guard = state_clone.config.lock().unwrap();
        let json = serde_json::to_string(&*cfg_guard).unwrap();
        
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // GET default config endpoint
    server.fn_handler("/api/config/default", Method::Get, |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        info!("HTTP: GET /api/config/default");
        let default_config = Config::default();
        let json = serde_json::to_string(&default_config).unwrap();
        
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // POST config check endpoint
    let state_clone = state.clone();
    server.fn_handler("/api/config/check", Method::Post, move |mut req| -> Result<(), esp_idf_svc::io::EspIOError> {
        debug!("HTTP: POST /api/config/check");
        let mut buf = vec![0u8; 4096];
        let bytes_read = req.read(&mut buf)?;
        
        if let Ok(mut new_config) = serde_json::from_slice::<Config>(&buf[..bytes_read]) {
            new_config.validate();
            let needs_restart = {
                let cfg_guard = state_clone.config.lock().unwrap();
                check_restart_needed(&cfg_guard, &new_config)
            };
            let mut response = req.into_ok_response()?;
            if needs_restart {
                response.write_all(b"{\"restart\":true}")?;
            } else {
                response.write_all(b"{\"restart\":false}")?;
            }
        } else {
            warn!("Invalid config JSON received");
            req.into_status_response(400)?;
        }
        
        Ok(())
    })?;

    // POST config endpoint
    let state_clone = state.clone();
    server.fn_handler("/api/config", Method::Post, move |mut req| -> Result<(), esp_idf_svc::io::EspIOError> {
        info!("HTTP: POST /api/config");
        let mut buf = vec![0u8; 4096];
        let bytes_read = req.read(&mut buf)?;
        
        if let Ok(mut new_config) = serde_json::from_slice::<Config>(&buf[..bytes_read]) {
            new_config.validate();
            
            debug!("Config update: query_mode={:?}, log_level={:?}", 
                   new_config.test.query_mode, new_config.log_level);
            
            let needs_restart = {
                let cfg_guard = state_clone.config.lock().unwrap();
                check_restart_needed(&cfg_guard, &new_config)
            };
            
            {
                let mut cfg_guard = state_clone.config.lock().unwrap();
                *cfg_guard = new_config;
                if let Err(e) = cfg_guard.save() {
                    warn!("Failed to save config: {e}");
                }
            }
            
            if needs_restart {
                info!("Config changed (requires restart), restarting in 2 seconds...");
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

    // POST test/start endpoint
    let state_clone = state.clone();
    server.fn_handler("/api/test/start", Method::Post, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        info!("HTTP: POST /api/test/start");
        
        if let Some(tx) = state_clone.test_control_tx.lock().unwrap().as_ref() {
            if tx.send(TestControlMessage::Start).is_err() {
                warn!("Failed to send start command");
            }
        }
        
        req.into_ok_response()?;
        Ok(())
    })?;

    // POST test/stop endpoint
    let state_clone = state.clone();
    server.fn_handler("/api/test/stop", Method::Post, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        info!("HTTP: POST /api/test/stop");
        
        if let Some(tx) = state_clone.test_control_tx.lock().unwrap().as_ref() {
            if tx.send(TestControlMessage::Stop).is_err() {
                warn!("Failed to send stop command");
            }
        }
        
        req.into_ok_response()?;
        Ok(())
    })?;

    // GET test/status endpoint
    let state_clone = state.clone();
    server.fn_handler("/api/test/status", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        debug!("HTTP: GET /api/test/status");
        
        let query_mode = {
            let cfg_guard = state_clone.config.lock().unwrap();
            cfg_guard.test.query_mode as u8
        };
        
        let metrics = &state_clone.metrics;
        let status = TestStatus {
            test_running: metrics.test_running.load(Ordering::Relaxed),
            query_mode,
            requests_per_sec: metrics.requests_per_sec.load(Ordering::Relaxed),
            total_requests: metrics.total_requests.load(Ordering::Relaxed),
            total_errors: metrics.total_errors.load(Ordering::Relaxed),
            dongle_connected: state_clone.dongle_connected.load(Ordering::Relaxed),
            bytes_captured: metrics.bytes_captured.load(Ordering::Relaxed),
            records_captured: metrics.records_captured.load(Ordering::Relaxed),
            buffer_usage_pct: metrics.buffer_usage_pct.load(Ordering::Relaxed),
            client_connected: metrics.client_connected.load(Ordering::Relaxed),
            capture_overflow: metrics.capture_overflow.load(Ordering::Relaxed),
        };
        
        let json = serde_json::to_string(&status).unwrap_or_else(|_| "{}".to_string());
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // GET capture download endpoint — returns header + raw binary capture data
    let state_clone = state.clone();
    server.fn_handler("/api/capture", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        info!("HTTP: GET /api/capture");

        // Only allow download when test is stopped
        if state_clone.metrics.test_running.load(Ordering::Relaxed) {
            warn!("Capture download rejected: test still running");
            req.into_status_response(409)?;
            return Ok(());
        }

        let header = crate::obd2::build_capture_header(&state_clone);
        let buf_guard = state_clone.capture_buffer.lock().unwrap();

        if buf_guard.is_empty() {
            req.into_status_response(204)?;
            return Ok(());
        }

        let total_len = header.len() + buf_guard.len();
        let content_len = total_len.to_string();
        let mut response = req.into_response(200, Some("OK"), &[
            ("Content-Type", "application/octet-stream"),
            ("Content-Disposition", "attachment; filename=\"capture.ttcap\""),
            ("Content-Length", &content_len),
        ])?;
        response.write_all(&header)?;
        // Write capture data in chunks to avoid holding the lock too long
        // (but we already hold it, so just write it all)
        response.write_all(&buf_guard)?;

        Ok(())
    })?;

    // POST capture clear endpoint — clears the capture buffer
    let state_clone = state.clone();
    server.fn_handler("/api/capture/clear", Method::Post, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        info!("HTTP: POST /api/capture/clear");

        // Only allow clear when test is stopped
        if state_clone.metrics.test_running.load(Ordering::Relaxed) {
            warn!("Capture clear rejected: test still running");
            req.into_status_response(409)?;
            return Ok(());
        }

        {
            let mut buf_guard = state_clone.capture_buffer.lock().unwrap();
            buf_guard.clear();
        }
        state_clone.metrics.bytes_captured.store(0, Ordering::Relaxed);
        state_clone.metrics.records_captured.store(0, Ordering::Relaxed);
        state_clone.metrics.buffer_usage_pct.store(0, Ordering::Relaxed);
        state_clone.metrics.capture_overflow.store(false, Ordering::Relaxed);

        info!("Capture buffer cleared");
        req.into_ok_response()?;
        Ok(())
    })?;

    // GET capture status endpoint
    let state_clone = state.clone();
    server.fn_handler("/api/capture/status", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        debug!("HTTP: GET /api/capture/status");

        #[allow(clippy::cast_possible_truncation)]
        let buf_len = state_clone.capture_buffer.lock().unwrap().len() as u32;
        let capture_buffer_size = {
            let cfg_guard = state_clone.config.lock().unwrap();
            cfg_guard.test.capture_buffer_size
        };

        let status = CaptureStatus {
            buffer_bytes: buf_len,
            buffer_capacity: capture_buffer_size,
            records: state_clone.metrics.records_captured.load(Ordering::Relaxed),
            overflow: state_clone.metrics.capture_overflow.load(Ordering::Relaxed),
            test_running: state_clone.metrics.test_running.load(Ordering::Relaxed),
        };

        let json = serde_json::to_string(&status).unwrap_or_else(|_| "{}".to_string());
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // GET wifi scan endpoint
    let state_clone = state.clone();
    server.fn_handler("/api/wifi/scan", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        info!("HTTP: GET /api/wifi/scan");
        let mut wifi_guard = state_clone.wifi.lock().unwrap();
        
        let networks: Vec<Network> = match wifi_guard.scan() {
            Ok(aps) => {
                debug!("WiFi scan found {} networks", aps.len());
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
        let wifi_guard = state_clone.wifi.lock().unwrap();
        
        let sta_netif = wifi_guard.sta_netif();
        let ip_info = sta_netif.get_ip_info().ok();
        
        let mac_bytes = wifi_guard.driver().get_mac(esp_idf_svc::wifi::WifiDeviceId::Sta).unwrap_or([0u8; 6]);
        let mac = format!("{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            mac_bytes[0], mac_bytes[1], mac_bytes[2],
            mac_bytes[3], mac_bytes[4], mac_bytes[5]);
        
        let (ssid, rssi) = if wifi_guard.is_connected().unwrap_or(false) {
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
            ip: ip_info.as_ref().map(|i| format!("{}/{}", i.ip, i.subnet.mask.0)),
            mac,
            rssi,
        };
        
        let json = serde_json::to_string(&status).unwrap_or_else(|_| "{}".to_string());
        
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // GET all open sockets endpoint (for debugging)
    server.fn_handler("/api/sockets", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        debug!("HTTP: GET /api/sockets");
        
        let sockets = enumerate_sockets();
        let json = serde_json::to_string(&sockets).unwrap_or_else(|_| "[]".to_string());
        
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // GET debug info endpoint
    server.fn_handler("/api/debug_info", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        debug!("HTTP: GET /api/debug_info");
        
        let free_heap = unsafe { esp_get_free_heap_size() };
        let min_free_heap = unsafe { esp_get_minimum_free_heap_size() };
        
        let info = DebugInfo {
            free_heap,
            min_free_heap,
        };
        
        let json = serde_json::to_string(&info).unwrap_or_else(|_| "{}".to_string());
        
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // POST reboot endpoint
    let state_clone = state.clone();
    server.fn_handler("/api/reboot", Method::Post, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        info!("HTTP: POST /api/reboot - Device reboot requested");
        
        req.into_ok_response()?;
        
        let state = state_clone.clone();
        crate::thread_util::spawn_named(c"restart", move || {
            std::thread::sleep(std::time::Duration::from_secs(1));
            
            info!("Stopping WiFi before reboot...");
            if let Ok(mut wifi_guard) = state.wifi.lock() {
                if let Err(e) = wifi_guard.stop() {
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

    // Captive portal fallback handler
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
    <title>TachTalk Test Setup</title>
    <meta http-equiv="refresh" content="0;url={redirect_url}">
</head>
<body>
    <p>Redirecting to <a href="{redirect_url}">TachTalk Test Setup</a>...</p>
</body>
</html>
"#
        );
        
        info!("Captive portal enabled for hostname: {hostname}");
        
        server.fn_handler("/*", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
            let host = req.header("Host").unwrap_or("");
            let host_lower = host.to_lowercase();
            let host_without_port = host_lower.split(':').next().unwrap_or("");
            
            let is_valid_host = valid_hosts.iter().any(|h| h == host_without_port);
            
            if is_valid_host {
                info!("HTTP: GET {} -> 404 (host: {})", req.uri(), host);
                req.into_status_response(404)?;
            } else {
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
