use crate::error::Result;
use embedded_svc::http::Method;
use embedded_svc::io::Write;
use esp_idf_svc::http::server::{Configuration, EspHttpServer};
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::io::Read as _;
use std::net::Ipv4Addr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use esp_idf_svc::io::EspIOError;
use esp_idf_svc::sys::{
    heap_caps_get_free_size, heap_caps_get_minimum_free_size, lwip_getpeername, lwip_getsockname,
    lwip_getsockopt, sockaddr, sockaddr_in, EspError, AF_INET, ESP_ERR_INVALID_ARG, ESP_FAIL,
    MALLOC_CAP_INTERNAL, MALLOC_CAP_SPIRAM,
};
use std::mem::MaybeUninit;

use crate::config::Config;
use crate::rpm_leds::RpmTaskMessage;
use crate::sse_server::SSE_PORT;
use crate::State;
use smallvec::SmallVec;

/// Result type for HTTP handler closures (uses `EspIOError`, not our crate error)
type HandlerResult = std::result::Result<(), EspIOError>;

// Redefine bindgen u32 constants as i32 to match C socket API function signatures
#[allow(clippy::cast_possible_wrap)]
const SOL_SOCKET: i32 = esp_idf_svc::sys::SOL_SOCKET as i32;
#[allow(clippy::cast_possible_wrap)]
const SO_TYPE: i32 = esp_idf_svc::sys::SO_TYPE as i32;
#[allow(clippy::cast_possible_wrap)]
const SOCK_STREAM: i32 = esp_idf_svc::sys::SOCK_STREAM as i32;
#[allow(clippy::cast_possible_wrap)]
const SOCK_DGRAM: i32 = esp_idf_svc::sys::SOCK_DGRAM as i32;
#[allow(clippy::cast_possible_wrap)]
const LWIP_SOCKET_OFFSET: i32 = esp_idf_svc::sys::LWIP_SOCKET_OFFSET as i32;
#[allow(clippy::cast_possible_wrap)]
const CONFIG_LWIP_MAX_SOCKETS: i32 = esp_idf_svc::sys::CONFIG_LWIP_MAX_SOCKETS as i32;

// Size constants for socket API calls (usize → u32 for C API compatibility)
#[allow(clippy::cast_possible_truncation)]
const SIZE_OF_I32: u32 = std::mem::size_of::<i32>() as u32;
#[allow(clippy::cast_possible_truncation)]
const SIZE_OF_SOCKADDR_IN: u32 = std::mem::size_of::<sockaddr_in>() as u32;

/// Maximum config request body size (16 KB).
const MAX_CONFIG_BODY_SIZE: u64 = 16384;

/// Adapter that wraps an `embedded_io::Read` as a `std::io::Read`.
///
/// Allows passing ESP-IDF HTTP request readers to APIs that expect
/// `std::io::Read`, such as `serde_json::from_reader`.
struct StdRead<R>(R);

impl<R: embedded_svc::io::Read> std::io::Read for StdRead<R>
where
    R::Error: std::error::Error + Send + Sync + 'static,
{
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf).map_err(std::io::Error::other)
    }
}

/// Adapter that wraps an `embedded_io::Write` as a `std::io::Write`.
///
/// Allows passing ESP-IDF HTTP response writers to APIs that expect
/// `std::io::Write`, such as `serde_json::to_writer`.
struct StdWrite<W>(W);

impl<W: embedded_svc::io::Write> std::io::Write for StdWrite<W>
where
    W::Error: std::error::Error + Send + Sync + 'static,
{
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf).map_err(std::io::Error::other)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush().map_err(std::io::Error::other)
    }
}

/// Serialize `value` as JSON directly into an `embedded_io::Write` writer.
///
/// Avoids the intermediate `String` allocation of `serde_json::to_string`.
/// Recovers the original write error from the `serde_json` error chain.
fn write_json<W: embedded_svc::io::Write>(
    writer: &mut W,
    value: &impl serde::Serialize,
) -> HandlerResult
where
    W::Error: std::error::Error + Send + Sync + 'static,
{
    // serde_json wraps IO errors from StdWrite, which in turn boxes our
    // EspIOError via Error::other.  Unwind that chain to recover the
    // original EspIOError.  Pure serialization errors (e.g. a Serialize
    // impl failed) get ErrorKind::InvalidData and are logged + mapped
    // to ESP_FAIL.
    serde_json::to_writer(StdWrite(writer), value).map_err(|e| {
        let io_err = std::io::Error::from(e);
        if io_err.kind() == std::io::ErrorKind::Other {
            // IO error from StdWrite — recover the boxed EspIOError.
            // StdWrite always wraps EspIOError via Error::other.
            *io_err
                .into_inner()
                .expect("Other-kind io::Error always has inner")
                .downcast::<EspIOError>()
                .expect("StdWrite always boxes EspIOError")
        } else {
            // Pure serialization error (e.g. a Serialize impl failed)
            warn!("write_json: serialization failed: {io_err}");
            EspIOError::from(EspError::from_infallible::<{ ESP_FAIL }>())
        }
    })
}

/// Check if a config change would require a device restart
/// Returns (GPIOs to reset before restart, `needs_restart`)
#[allow(clippy::similar_names)]
fn check_restart_needed(current: &Config, new: &Config) -> (SmallVec<[u8; 4]>, bool) {
    let led_changed = current.led_gpio != new.led_gpio;
    let encoder_a_changed = current.encoder_pin_a != new.encoder_pin_a;
    let encoder_b_changed = current.encoder_pin_b != new.encoder_pin_b;
    let button_changed = current.button_pin != new.button_pin;
    let status_red_changed = current.status_led_red_pin != new.status_led_red_pin;
    let status_yellow_changed = current.status_led_yellow_pin != new.status_led_yellow_pin;
    let status_green_changed = current.status_led_green_pin != new.status_led_green_pin;
    let wifi_changed =
        current.wifi.ssid != new.wifi.ssid || current.wifi.password != new.wifi.password;
    let ip_changed = current.ip.use_dhcp != new.ip.use_dhcp
        || current.ip.ip != new.ip.ip
        || current.ip.prefix_len != new.ip.prefix_len;
    let ap_changed = current.ap_ssid != new.ap_ssid
        || current.ap_password != new.ap_password
        || current.ap_ip != new.ap_ip
        || current.ap_prefix_len != new.ap_prefix_len;

    // Collect old GPIO pins that need reset (to disconnect from RMT/PCNT peripherals)
    let mut gpios_to_reset = SmallVec::new();
    if led_changed {
        gpios_to_reset.push(current.led_gpio);
    }
    if encoder_a_changed && current.encoder_pin_a != 0 {
        gpios_to_reset.push(current.encoder_pin_a);
    }
    if encoder_b_changed && current.encoder_pin_b != 0 {
        gpios_to_reset.push(current.encoder_pin_b);
    }
    if button_changed && current.button_pin != 0 {
        gpios_to_reset.push(current.button_pin);
    }
    if status_red_changed && current.status_led_red_pin != 0 {
        gpios_to_reset.push(current.status_led_red_pin);
    }
    if status_yellow_changed && current.status_led_yellow_pin != 0 {
        gpios_to_reset.push(current.status_led_yellow_pin);
    }
    if status_green_changed && current.status_led_green_pin != 0 {
        gpios_to_reset.push(current.status_led_green_pin);
    }

    let needs_restart = led_changed
        || encoder_a_changed
        || encoder_b_changed
        || button_changed
        || status_red_changed
        || status_yellow_changed
        || status_green_changed
        || wifi_changed
        || ip_changed
        || ap_changed;

    (gpios_to_reset, needs_restart)
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

/// Capture status response
#[derive(serde::Serialize)]
struct CaptureStatus {
    buffer_bytes: u32,
    buffer_capacity: u32,
    records: u32,
    overflow: bool,
    capture_active: bool,
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
fn enumerate_sockets() -> Vec<SocketInfo> {
    let mut sockets = Vec::new();

    for fd in LWIP_SOCKET_OFFSET..(LWIP_SOCKET_OFFSET + CONFIG_LWIP_MAX_SOCKETS) {
        // Try to get socket type - if this fails, FD is not a valid socket
        let mut sock_type: i32 = 0;
        let mut optlen = SIZE_OF_I32;

        let result = unsafe {
            lwip_getsockopt(
                fd,
                SOL_SOCKET,
                SO_TYPE,
                std::ptr::addr_of_mut!(sock_type).cast(),
                &mut optlen,
            )
        };

        if result != 0 {
            continue; // Not a valid socket
        }

        let socket_type = match sock_type {
            x if x == SOCK_STREAM => SocketType::Tcp,
            x if x == SOCK_DGRAM => SocketType::Udp,
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
fn get_socket_addr(fd: i32, peer: bool) -> Option<String> {
    let mut addr: MaybeUninit<sockaddr_in> = MaybeUninit::uninit();
    let mut addrlen = SIZE_OF_SOCKADDR_IN;

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
    if u32::from(addr.sin_family) != AF_INET {
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

    warn!(
        "Open sockets ({}/{}):",
        sockets.len(),
        CONFIG_LWIP_MAX_SOCKETS
    );
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
    internal_free: usize,
    internal_min_free: usize,
    spiram_free: usize,
    spiram_min_free: usize,
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

/// Register configuration-related routes (GET/POST config, brightness)
fn register_config_routes(server: &mut EspHttpServer<'static>, state: &Arc<State>) -> Result<()> {
    register_index_page(server)?;
    register_get_config(server, state)?;
    register_get_default_config(server)?;
    register_post_config_check(server, state)?;
    register_post_config(server, state)?;
    register_post_brightness(server, state)?;
    Ok(())
}

fn register_index_page(server: &mut EspHttpServer<'static>) -> Result<()> {
    server.fn_handler("/", Method::Get, |req| -> HandlerResult {
        let mut response = req.into_ok_response()?;
        response.write_all(HTML_INDEX_START.as_bytes())?;
        response.write_all(SSE_PORT.to_string().as_bytes())?;
        response.write_all(HTML_INDEX_END.as_bytes())?;
        Ok(())
    })?;
    Ok(())
}

fn register_get_config(server: &mut EspHttpServer<'static>, state: &Arc<State>) -> Result<()> {
    let state_clone = state.clone();
    server.fn_handler("/api/config", Method::Get, move |req| -> HandlerResult {
        info!("HTTP: GET /api/config");
        let cfg = state_clone.config.lock().unwrap();
        let mut response = req.into_ok_response()?;
        write_json(&mut response, &*cfg)?;
        Ok(())
    })?;
    Ok(())
}

fn register_get_default_config(server: &mut EspHttpServer<'static>) -> Result<()> {
    server.fn_handler("/api/config/default", Method::Get, |req| -> HandlerResult {
        info!("HTTP: GET /api/config/default");
        let default_config = crate::config::Config::default();
        let mut response = req.into_ok_response()?;
        write_json(&mut response, &default_config)?;
        Ok(())
    })?;
    Ok(())
}

fn register_post_config_check(
    server: &mut EspHttpServer<'static>,
    state: &Arc<State>,
) -> Result<()> {
    let state_clone = state.clone();
    server.fn_handler(
        "/api/config/check",
        Method::Post,
        move |mut req| -> HandlerResult {
            debug!("HTTP: POST /api/config/check");
            let reader = StdRead(&mut req).take(MAX_CONFIG_BODY_SIZE);

            if let Ok(mut new_config) = serde_json::from_reader::<_, crate::config::Config>(reader)
            {
                new_config.validate();
                let (_, needs_restart) = {
                    let cfg = state_clone.config.lock().unwrap();
                    check_restart_needed(&cfg, &new_config)
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
        },
    )?;
    Ok(())
}

fn register_post_config(server: &mut EspHttpServer<'static>, state: &Arc<State>) -> Result<()> {
    let state_clone = state.clone();
    server.fn_handler(
        "/api/config",
        Method::Post,
        move |mut req| -> HandlerResult {
            info!("HTTP: POST /api/config");
            let reader = StdRead(&mut req).take(MAX_CONFIG_BODY_SIZE);

            if let Ok(mut new_config) = serde_json::from_reader::<_, crate::config::Config>(reader)
            {
                // Validate/clamp values to safe ranges
                new_config.validate();

                debug!(
                    "Config update: {} profiles, active={}, log_level={:?}",
                    new_config.profiles.len(),
                    new_config.active_profile,
                    new_config.log_level
                );

                // Check if any settings changed that require a restart
                let (gpios_to_reset, needs_restart) = {
                    let cfg = state_clone.config.lock().unwrap();
                    check_restart_needed(&cfg, &new_config)
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

                if needs_restart {
                    info!("Config changed (requires restart), restarting in 2 seconds...");
                    // Reset old GPIOs to disconnect from RMT/PCNT peripherals before restart
                    for gpio in gpios_to_reset {
                        unsafe {
                            esp_idf_svc::sys::gpio_reset_pin(i32::from(gpio));
                        }
                    }
                    let mut response = req.into_ok_response()?;
                    response.write_all(b"{\"restart\":true}")?;
                    crate::thread_util::spawn_named(
                        c"restart",
                        4096,
                        crate::thread_util::StackMemory::SpiRam,
                        || {
                            std::thread::sleep(std::time::Duration::from_secs(2));
                            unsafe {
                                esp_idf_svc::sys::esp_restart();
                            }
                        },
                    );
                } else {
                    req.into_ok_response()?;
                }
            } else {
                warn!("Invalid config JSON received");
                req.into_status_response(400)?;
            }

            Ok(())
        },
    )?;
    Ok(())
}

fn register_post_brightness(server: &mut EspHttpServer<'static>, state: &Arc<State>) -> Result<()> {
    let state_clone = state.clone();
    server.fn_handler(
        "/api/brightness",
        Method::Post,
        move |mut req| -> HandlerResult {
            debug!("HTTP: POST /api/brightness");
            let reader = StdRead(&mut req);

            if let Ok(brightness_req) = serde_json::from_reader::<_, BrightnessRequest>(reader) {
                debug!(
                    "Brightness update: {} (save={})",
                    brightness_req.brightness, brightness_req.save
                );

                // Send brightness to LED task immediately
                let _ = state_clone
                    .rpm_tx
                    .send(RpmTaskMessage::Brightness(brightness_req.brightness));

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
        },
    )?;
    Ok(())
}

/// Register network-related routes (WiFi scan, network status)
fn register_network_routes(server: &mut EspHttpServer<'static>, state: &Arc<State>) -> Result<()> {
    // GET wifi scan endpoint
    let state_clone = state.clone();
    server.fn_handler("/api/wifi/scan", Method::Get, move |req| -> HandlerResult {
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

        let mut response = req.into_ok_response()?;
        write_json(&mut response, &networks)?;
        Ok(())
    })?;

    // GET network status endpoint
    let state_clone = state.clone();
    server.fn_handler("/api/network", Method::Get, move |req| -> HandlerResult {
        info!("HTTP: GET /api/network");
        let wifi = state_clone.wifi.lock().unwrap();

        let sta_netif = wifi.sta_netif();
        let ip_info = sta_netif.get_ip_info().ok();

        let mac_bytes = wifi
            .driver()
            .get_mac(esp_idf_svc::wifi::WifiDeviceId::Sta)
            .unwrap_or([0u8; 6]);
        let mac = format!(
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            mac_bytes[0], mac_bytes[1], mac_bytes[2], mac_bytes[3], mac_bytes[4], mac_bytes[5]
        );

        // Get SSID and RSSI from current STA connection info
        let (ssid, rssi) = if wifi.is_connected().unwrap_or(false) {
            let mut ap_info: esp_idf_svc::sys::wifi_ap_record_t = unsafe { std::mem::zeroed() };
            let result = unsafe { esp_idf_svc::sys::esp_wifi_sta_get_ap_info(&mut ap_info) };
            if result == esp_idf_svc::sys::ESP_OK {
                let ssid_bytes = &ap_info.ssid;
                let ssid_len = ssid_bytes
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(ssid_bytes.len());
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
            ip: ip_info
                .as_ref()
                .map(|i| format!("{}/{}", i.ip, i.subnet.mask.0)),
            mac,
            rssi,
        };

        let mut response = req.into_ok_response()?;
        write_json(&mut response, &status)?;
        Ok(())
    })?;

    Ok(())
}

/// Register status and metrics routes (connection status, TCP info, RPM, polling metrics)
fn register_status_routes(server: &mut EspHttpServer<'static>, state: &Arc<State>) -> Result<()> {
    // GET connection status endpoint for diagram
    let state_clone = state.clone();
    server.fn_handler("/api/status", Method::Get, move |req| -> HandlerResult {
        debug!("HTTP: GET /api/status");

        let wifi_connected = state_clone
            .wifi
            .lock()
            .unwrap()
            .is_connected()
            .unwrap_or(false);
        let dongle_tcp_connected = state_clone.dongle_tcp_state.load(Ordering::Relaxed)
            != crate::obd2::DongleTcpState::Disconnected;
        let obd2_client_count = state_clone.obd2_client_count.load(Ordering::Relaxed);

        let status = ConnectionStatus {
            wifi_connected,
            dongle_tcp_connected,
            obd2_client_count,
        };

        let mut response = req.into_ok_response()?;
        write_json(&mut response, &status)?;
        Ok(())
    })?;

    // GET TCP connection details endpoint
    let state_clone = state.clone();
    server.fn_handler("/api/tcp", Method::Get, move |req| -> HandlerResult {
        debug!("HTTP: GET /api/tcp");

        let dongle = state_clone
            .dongle_tcp_info
            .lock()
            .unwrap()
            .map(|(local, remote)| TcpConnectionInfo {
                local: local.to_string(),
                remote: remote.to_string(),
            });

        let clients: Vec<TcpConnectionInfo> = state_clone
            .client_tcp_info
            .lock()
            .unwrap()
            .iter()
            .map(|(local, remote)| TcpConnectionInfo {
                local: local.to_string(),
                remote: remote.to_string(),
            })
            .collect();

        let status = TcpStatus { dongle, clients };
        let mut response = req.into_ok_response()?;
        write_json(&mut response, &status)?;
        Ok(())
    })?;

    // GET RPM endpoint (fallback for non-SSE clients)
    let state_clone = state.clone();
    server.fn_handler("/api/rpm", Method::Get, move |req| -> HandlerResult {
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

    // GET polling metrics endpoint
    let state_clone = state.clone();
    server.fn_handler("/api/metrics", Method::Get, move |req| -> HandlerResult {
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

        let mut response = req.into_ok_response()?;
        write_json(&mut response, &response_data)?;
        Ok(())
    })?;

    Ok(())
}

/// Register debug and system routes (sockets, debug info, reboot)
fn register_debug_routes(server: &mut EspHttpServer<'static>, state: &Arc<State>) -> Result<()> {
    // GET all open sockets endpoint (for debugging FD exhaustion)
    server.fn_handler("/api/sockets", Method::Get, move |req| -> HandlerResult {
        debug!("HTTP: GET /api/sockets");

        let sockets = enumerate_sockets();
        let mut response = req.into_ok_response()?;
        write_json(&mut response, &sockets)?;
        Ok(())
    })?;

    // GET debug info endpoint (AT commands, PIDs, memory stats, etc.)
    let state_clone = state.clone();
    server.fn_handler(
        "/api/debug_info",
        Method::Get,
        move |req| -> HandlerResult {
            debug!("HTTP: GET /api/debug_info");

            let at_commands: Vec<String> = state_clone
                .at_command_log
                .lock()
                .map(|log| {
                    let mut cmds: Vec<String> = log.iter().cloned().collect();
                    cmds.sort();
                    cmds
                })
                .unwrap_or_default();

            let pids: Vec<String> = state_clone
                .pid_log
                .lock()
                .map(|log| {
                    let mut pids: Vec<String> = log.iter().cloned().collect();
                    pids.sort();
                    pids
                })
                .unwrap_or_default();

            // SAFETY: These are simple C functions that return a usize
            let internal_free = unsafe { heap_caps_get_free_size(MALLOC_CAP_INTERNAL) };
            let internal_min_free = unsafe { heap_caps_get_minimum_free_size(MALLOC_CAP_INTERNAL) };
            let spiram_free = unsafe { heap_caps_get_free_size(MALLOC_CAP_SPIRAM) };
            let spiram_min_free = unsafe { heap_caps_get_minimum_free_size(MALLOC_CAP_SPIRAM) };

            let info = DebugInfo {
                at_commands,
                pids,
                internal_free,
                internal_min_free,
                spiram_free,
                spiram_min_free,
            };

            let mut response = req.into_ok_response()?;
            write_json(&mut response, &info)?;
            Ok(())
        },
    )?;

    // POST reboot endpoint
    let state_clone = state.clone();
    server.fn_handler("/api/reboot", Method::Post, move |req| -> HandlerResult {
        info!("HTTP: POST /api/reboot - Device reboot requested");

        req.into_ok_response()?;

        // Schedule restart after response is sent
        let state = state_clone.clone();
        crate::thread_util::spawn_named(
            c"restart",
            4096,
            crate::thread_util::StackMemory::SpiRam,
            move || {
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
            },
        );

        Ok(())
    })?;

    Ok(())
}

/// Register captive portal wildcard handler (must be registered last)
fn register_captive_portal(
    server: &mut EspHttpServer<'static>,
    hostname: &str,
    ap_ip: Ipv4Addr,
) -> Result<()> {
    let hostname = hostname.to_owned();
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

    server.fn_handler("/*", Method::Get, move |req| -> HandlerResult {
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
            let mut response = req.into_response(
                302,
                Some("Found"),
                &[
                    ("Location", &redirect_url),
                    ("Cache-Control", "no-cache"),
                    ("Connection", "close"),
                ],
            )?;
            response.write_all(captive_portal_html.as_bytes())?;
        }
        Ok(())
    })?;

    Ok(())
}

/// OTA download request body
#[derive(serde::Deserialize)]
struct OtaDownloadRequest {
    url: String,
}

/// Stop WiFi and reboot into new firmware. Does not return.
fn perform_ota_reboot(state: &Arc<State>) -> ! {
    std::thread::sleep(std::time::Duration::from_secs(2));
    info!("Stopping WiFi before OTA reboot...");
    if let Ok(mut wifi_guard) = state.wifi.lock() {
        if let Err(e) = wifi_guard.stop() {
            warn!("Failed to stop WiFi: {e:?}");
        }
    }
    info!("Rebooting into new firmware...");
    unsafe {
        esp_idf_svc::sys::esp_restart();
    }
}

/// Schedule a reboot on a new thread after a successful OTA upload.
///
/// Needed for the upload path where we must return the HTTP response first.
fn schedule_ota_reboot(state: Arc<State>) {
    crate::thread_util::spawn_named(
        c"ota-reboot",
        4096,
        crate::thread_util::StackMemory::SpiRam,
        move || {
            perform_ota_reboot(&state);
        },
    );
}

/// Register capture routes: /api/capture (GET), /api/capture/clear, /api/capture/status,
/// /api/capture/start, /api/capture/stop
fn register_capture_routes(server: &mut EspHttpServer<'static>, state: &Arc<State>) -> Result<()> {
    register_capture_data_routes(server, state)?;
    register_capture_control_routes(server, state)?;
    Ok(())
}

/// Register capture data routes: download (GET) and clear (POST).
fn register_capture_data_routes(
    server: &mut EspHttpServer<'static>,
    state: &Arc<State>,
) -> Result<()> {
    // GET capture download endpoint — returns header + raw binary capture data
    let state_clone = state.clone();
    server.fn_handler("/api/capture", Method::Get, move |req| -> HandlerResult {
        info!("HTTP: GET /api/capture");

        let buf_guard = state_clone.capture_buffer.lock().unwrap();

        if buf_guard.is_empty() {
            req.into_status_response(204)?;
            return Ok(());
        }

        // Hot path: capture buffer capped at 6 MB by config validation, fits u32
        #[allow(clippy::cast_possible_truncation)]
        let buffer_len = buf_guard.len() as u32;
        let header = crate::obd2::build_capture_header(&state_clone, buffer_len);

        let total_len = header.len() + buf_guard.len();
        let content_len = total_len.to_string();
        let mut response = req.into_response(
            200,
            Some("OK"),
            &[
                ("Content-Type", "application/octet-stream"),
                (
                    "Content-Disposition",
                    "attachment; filename=\"capture.ttcap\"",
                ),
                ("Content-Length", &content_len),
            ],
        )?;
        response.write_all(&header)?;
        response.write_all(&buf_guard)?;

        Ok(())
    })?;

    // POST capture clear endpoint — clears the capture buffer
    let state_clone = state.clone();
    server.fn_handler(
        "/api/capture/clear",
        Method::Post,
        move |req| -> HandlerResult {
            info!("HTTP: POST /api/capture/clear");

            {
                let mut buf_guard = state_clone.capture_buffer.lock().unwrap();
                buf_guard.clear();
            }
            state_clone
                .polling_metrics
                .records_captured
                .store(0, Ordering::Relaxed);
            state_clone
                .polling_metrics
                .capture_overflow
                .store(false, Ordering::Relaxed);

            info!("Capture buffer cleared");
            req.into_ok_response()?;
            Ok(())
        },
    )?;

    Ok(())
}

/// Register capture control routes: status (GET), start (POST), stop (POST).
fn register_capture_control_routes(
    server: &mut EspHttpServer<'static>,
    state: &Arc<State>,
) -> Result<()> {
    // GET capture status endpoint
    let state_clone = state.clone();
    server.fn_handler(
        "/api/capture/status",
        Method::Get,
        move |req| -> HandlerResult {
            debug!("HTTP: GET /api/capture/status");

            let buffer_bytes = u32::try_from(state_clone.capture_buffer.lock().unwrap().len())
                .expect("buffer length fits in u32");
            let buffer_capacity = state_clone.config.lock().unwrap().obd2.capture_buffer_size;
            let capture_active = state_clone.capture_active.load(Ordering::Relaxed);
            let records = state_clone
                .polling_metrics
                .records_captured
                .load(Ordering::Relaxed);
            let overflow = state_clone
                .polling_metrics
                .capture_overflow
                .load(Ordering::Relaxed);

            let status = CaptureStatus {
                buffer_bytes,
                buffer_capacity,
                records,
                overflow,
                capture_active,
            };

            let mut response = req.into_ok_response()?;
            write_json(&mut response, &status)?;
            Ok(())
        },
    )?;

    // POST capture start — enables capture recording at runtime
    let state_clone = state.clone();
    server.fn_handler(
        "/api/capture/start",
        Method::Post,
        move |req| -> HandlerResult {
            info!("HTTP: POST /api/capture/start");
            state_clone.capture_active.store(true, Ordering::Relaxed);
            req.into_ok_response()?;
            Ok(())
        },
    )?;

    // POST capture stop — disables capture recording at runtime
    let state_clone = state.clone();
    server.fn_handler(
        "/api/capture/stop",
        Method::Post,
        move |req| -> HandlerResult {
            info!("HTTP: POST /api/capture/stop");
            state_clone.capture_active.store(false, Ordering::Relaxed);
            req.into_ok_response()?;
            Ok(())
        },
    )?;

    Ok(())
}

/// Register OTA firmware info and direct-upload routes: `/api/ota/info`, `/api/ota/upload`
fn register_ota_routes(server: &mut EspHttpServer<'static>, state: &Arc<State>) -> Result<()> {
    // GET firmware info (version + variant)
    server.fn_handler("/api/ota/info", Method::Get, move |req| -> HandlerResult {
        debug!("HTTP: GET /api/ota/info");
        let info = crate::ota::firmware_info();
        let mut response = req.into_ok_response()?;
        write_json(&mut response, &info)?;
        Ok(())
    })?;

    // POST firmware binary upload for OTA
    let state_clone = state.clone();
    server.fn_handler(
        "/api/ota/upload",
        Method::Post,
        move |mut req| -> HandlerResult {
            info!("HTTP: POST /api/ota/upload");

            let content_length: usize = req
                .header("Content-Length")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);

            if content_length == 0 {
                warn!("OTA upload: missing or zero Content-Length");
                req.into_status_response(400)?;
                return Ok(());
            }

            info!("OTA upload: Content-Length={content_length}");

            let result = crate::ota::perform_ota(
                |buf| {
                    let n = req.read(buf)?;
                    Ok(n)
                },
                content_length,
            );

            match result {
                Ok(()) => {
                    info!("OTA upload: success, scheduling reboot");
                    let mut response = req.into_ok_response()?;
                    response.write_all(b"{\"success\":true}")?;
                    schedule_ota_reboot(state_clone.clone());
                }
                Err(e) => {
                    error!("OTA upload failed: {e:?}");
                    let mut response = req.into_response(
                        500,
                        Some("OTA Failed"),
                        &[("Content-Type", "application/json")],
                    )?;
                    let body = format!("{{\"success\":false,\"error\":\"{e}\"}}");
                    response.write_all(body.as_bytes())?;
                }
            }

            Ok(())
        },
    )?;

    Ok(())
}

/// Register server-side OTA download route: `POST /api/ota/download`
fn register_ota_download_routes(
    server: &mut EspHttpServer<'static>,
    state: &Arc<State>,
) -> Result<()> {
    let state_clone = state.clone();
    server.fn_handler(
        "/api/ota/download",
        Method::Post,
        move |mut req| -> HandlerResult {
            info!("HTTP: POST /api/ota/download");

            // Reject if OTA already in progress
            let current = state_clone.ota_status.load(Ordering::Relaxed);
            if current != crate::ota::OtaState::Idle && current != crate::ota::OtaState::Error {
                let mut response = req.into_response(
                    409,
                    Some("Conflict"),
                    &[("Content-Type", "application/json")],
                )?;
                response.write_all(b"{\"success\":false,\"error\":\"OTA already in progress\"}")?;
                return Ok(());
            }

            let reader = StdRead(&mut req);
            let parsed: OtaDownloadRequest = serde_json::from_reader(reader).map_err(|e| {
                warn!("OTA download: invalid JSON: {e}");
                EspIOError::from(EspError::from_infallible::<{ ESP_ERR_INVALID_ARG }>())
            })?;

            let url = parsed.url;
            info!("OTA download: url={url}");

            // Reset status
            state_clone
                .ota_status
                .store(crate::ota::OtaState::Updating, Ordering::Relaxed);
            state_clone
                .ota_progress
                .store(0, std::sync::atomic::Ordering::Relaxed);
            *state_clone.ota_error.lock().unwrap() = String::new();

            // Spawn download thread – stack MUST be in internal SRAM because
            // esp_ota_begin disables the SPI flash cache (and thus PSRAM access)
            // while it erases flash sectors.
            let state = state_clone.clone();
            crate::thread_util::spawn_named(
                c"ota-download",
                16384,
                crate::thread_util::StackMemory::Internal,
                move || match crate::ota::download_and_update(
                    &url,
                    &state.ota_status,
                    &state.ota_progress,
                    &state.status_led_tx,
                ) {
                    Ok(()) => {
                        info!("OTA download: success, rebooting");
                        perform_ota_reboot(&state);
                    }
                    Err(e) => {
                        error!("OTA download failed: {e:?}");
                        *state.ota_error.lock().unwrap() = format!("{e}");
                        state
                            .ota_status
                            .store(crate::ota::OtaState::Error, Ordering::Relaxed);
                        let _ = state.status_led_tx.send(
                            crate::status_leds::StatusLedMessage::OtaStatus(
                                crate::ota::OtaState::Error,
                            ),
                        );
                    }
                },
            );

            let mut response = req.into_ok_response()?;
            response.write_all(b"{\"success\":true}")?;
            Ok(())
        },
    )?;

    Ok(())
}

/// Register OTA status polling route: `GET /api/ota/status`
fn register_ota_status_route(
    server: &mut EspHttpServer<'static>,
    state: &Arc<State>,
) -> Result<()> {
    let state_clone = state.clone();
    server.fn_handler(
        "/api/ota/status",
        Method::Get,
        move |req| -> HandlerResult {
            let status = state_clone.ota_status.load(Ordering::Relaxed);
            let progress = state_clone
                .ota_progress
                .load(std::sync::atomic::Ordering::Relaxed);
            let status_u8 = status as u8;
            let json = if status == crate::ota::OtaState::Error {
                let error = state_clone.ota_error.lock().unwrap();
                let escaped = error.replace('\\', "\\\\").replace('"', "\\\"");
                format!(
                    "{{\"status\":{status_u8},\"progress\":{progress},\"error\":\"{escaped}\"}}"
                )
            } else {
                format!("{{\"status\":{status_u8},\"progress\":{progress}}}")
            };
            let mut response = req.into_ok_response()?;
            response.write_all(json.as_bytes())?;
            Ok(())
        },
    )?;

    Ok(())
}

pub fn start_server(
    state: &Arc<State>,
    ap_hostname: Option<String>,
    ap_ip: Ipv4Addr,
) -> Result<()> {
    info!("Web server starting...");

    // Enable wildcard URI matching for captive portal fallback handler
    // Enable LRU purge to handle abrupt disconnections from captive portal browsers
    // LWIP max is 16 sockets; leave room for DNS, SSE, mDNS, OBD2 proxy, dongle, httpd control
    let server_config = Configuration {
        uri_match_wildcard: true,
        max_open_sockets: 6,
        session_timeout: core::time::Duration::from_secs(2),
        lru_purge_enable: true,
        stack_size: 10240,
        ..Default::default()
    };
    let mut server = EspHttpServer::new(&server_config)?;

    register_config_routes(&mut server, state)?;
    register_network_routes(&mut server, state)?;
    register_status_routes(&mut server, state)?;
    register_debug_routes(&mut server, state)?;
    register_capture_routes(&mut server, state)?;
    register_ota_routes(&mut server, state)?;
    register_ota_download_routes(&mut server, state)?;
    register_ota_status_route(&mut server, state)?;

    // Captive portal wildcard must be registered last
    if let Some(hostname) = ap_hostname {
        register_captive_portal(&mut server, &hostname, ap_ip)?;
    }

    info!("Web server started on http://0.0.0.0:80");

    // Keep server alive
    std::mem::forget(server);

    Ok(())
}
