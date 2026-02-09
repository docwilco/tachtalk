use anyhow::Result;
use esp_idf_hal::delay::FreeRtos;
use esp_idf_hal::prelude::*;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::mdns::EspMdns;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::netif::{EspNetif, NetifConfiguration, NetifStack};
use esp_idf_svc::wifi::{
    AccessPointConfiguration, AuthMethod, ClientConfiguration, Configuration,
    EspWifi, WifiDriver,
};
use esp_idf_svc::ipv4::{
    self, ClientConfiguration as IpClientConfiguration, ClientSettings as IpClientSettings,
    Configuration as IpConfiguration, Ipv4Addr, Mask, Subnet,
};
use log::{debug, error, info, warn};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

mod config;
mod cpu_metrics;
mod dns;
mod obd2;
mod sse_server;
mod thread_util;
mod watchdog;
mod web_server;

use crate::watchdog::WatchdogHandle;
use config::Config;
use sse_server::{sse_server_task, SseSender};

/// Test metrics shared across tasks
pub struct TestMetrics {
    /// Total requests sent
    pub total_requests: AtomicU32,
    /// Requests in the last second
    pub requests_per_sec: AtomicU32,
    /// Total errors
    pub total_errors: AtomicU32,
    /// Test running flag
    pub test_running: AtomicBool,
    /// Test start time (uptime ms)
    pub test_start_ms: AtomicU32,
    /// Mode 5: bytes captured
    pub bytes_captured: AtomicU32,
    /// Mode 5: records captured
    pub records_captured: AtomicU32,
    /// Mode 5: buffer usage percentage (0-100)
    pub buffer_usage_pct: AtomicU32,
    /// Mode 5: client connected
    pub client_connected: AtomicBool,
    /// Mode 5: capture overflow occurred
    pub capture_overflow: AtomicBool,
}

impl Default for TestMetrics {
    fn default() -> Self {
        Self {
            total_requests: AtomicU32::new(0),
            requests_per_sec: AtomicU32::new(0),
            total_errors: AtomicU32::new(0),
            test_running: AtomicBool::new(false),
            test_start_ms: AtomicU32::new(0),
            bytes_captured: AtomicU32::new(0),
            records_captured: AtomicU32::new(0),
            buffer_usage_pct: AtomicU32::new(0),
            client_connected: AtomicBool::new(false),
            capture_overflow: AtomicBool::new(false),
        }
    }
}

impl TestMetrics {
    pub fn reset(&self) {
        self.total_requests.store(0, Ordering::Relaxed);
        self.requests_per_sec.store(0, Ordering::Relaxed);
        self.total_errors.store(0, Ordering::Relaxed);
        self.test_start_ms.store(0, Ordering::Relaxed);
        self.bytes_captured.store(0, Ordering::Relaxed);
        self.records_captured.store(0, Ordering::Relaxed);
        self.buffer_usage_pct.store(0, Ordering::Relaxed);
        self.client_connected.store(false, Ordering::Relaxed);
        self.capture_overflow.store(false, Ordering::Relaxed);
    }
}

/// Central state shared across tasks
pub struct State {
    pub config: Mutex<Config>,
    pub wifi: Mutex<EspWifi<'static>>,
    /// AP SSID (cached at startup from config)
    pub ap_ssid: String,
    pub sse_tx: SseSender,
    /// Test metrics
    pub metrics: TestMetrics,
    /// Whether we have an active TCP connection to the OBD2 dongle
    pub dongle_connected: AtomicBool,
    /// Control channel for the test task
    pub test_control_tx: Mutex<Option<std::sync::mpsc::Sender<TestControlMessage>>>,
    /// Mode 5 capture buffer (shared so web server can read/clear it)
    pub capture_buffer: Mutex<Vec<u8>>,
}

/// Messages to control the test task
pub enum TestControlMessage {
    Start,
    Stop,
}

/// Create STA network interface with static IP or DHCP based on config
fn create_sta_netif(config: &Config) -> Result<EspNetif> {
    if config.ip.use_dhcp {
        info!("STA netif: DHCP mode");
        Ok(EspNetif::new(NetifStack::Sta)?)
    } else {
        let ip: Ipv4Addr = config.ip.ip.parse().map_err(|_| {
            anyhow::anyhow!("Invalid static IP: {}", config.ip.ip)
        })?;
        let mask = config.ip.prefix_len;

        info!("STA netif: Static IP {ip}/{mask} (no gateway)");

        let mut sta_config = NetifConfiguration::wifi_default_client();
        sta_config.ip_configuration = Some(IpConfiguration::Client(
            IpClientConfiguration::Fixed(IpClientSettings {
                ip,
                subnet: Subnet {
                    gateway: Ipv4Addr::UNSPECIFIED,
                    mask: Mask(mask),
                },
                dns: None,
                secondary_dns: None,
            }),
        ));
        Ok(EspNetif::new_with_conf(&sta_config)?)
    }
}

/// Create AP network interface with captive portal DNS configuration
fn create_ap_netif(ap_ip: Ipv4Addr, ap_prefix_len: u8) -> Result<EspNetif> {
    let ap_router_config = ipv4::RouterConfiguration {
        subnet: ipv4::Subnet {
            gateway: ap_ip,
            mask: ipv4::Mask(ap_prefix_len),
        },
        dhcp_enabled: true,
        dns: Some(ap_ip),
        secondary_dns: Some(ap_ip),
    };

    let mut ap_netif_config = NetifConfiguration::wifi_default_router();
    ap_netif_config.ip_configuration = Some(ipv4::Configuration::Router(ap_router_config));
    Ok(EspNetif::new_with_conf(&ap_netif_config)?)
}

/// Initialize mDNS for local discovery (tachtalk-test.local)
fn setup_mdns() -> Option<EspMdns> {
    match EspMdns::take() {
        Ok(mut m) => {
            let _ = m.set_hostname("tachtalk-test");
            let _ = m.set_instance_name("TachTalk Test Firmware");
            let _ = m.add_service(None, "_http", "_tcp", 80, &[]);
            info!("mDNS started: tachtalk-test.local");
            Some(m)
        }
        Err(e) => {
            warn!("Failed to start mDNS: {e:?}");
            None
        }
    }
}

/// Start WiFi in Mixed mode (AP + STA)
fn start_wifi(
    config: &Config,
    mut wifi: EspWifi<'static>,
    ap_ssid: &str,
    ap_password: Option<&str>,
    ap_auth_method: AuthMethod,
) -> Result<EspWifi<'static>> {
    let sta_ssid = config.wifi.ssid.clone();
    let sta_password = config.wifi.password.clone().unwrap_or_default();
    let sta_auth_method = if sta_password.is_empty() { AuthMethod::None } else { AuthMethod::WPA2Personal };

    let ap_pw = ap_password.unwrap_or("");

    info!("Starting WiFi in Mixed mode: AP '{ap_ssid}' + STA '{sta_ssid}'");
    wifi.set_configuration(&Configuration::Mixed(
        ClientConfiguration {
            ssid: sta_ssid.as_str().try_into().unwrap_or_default(),
            password: sta_password.as_str().try_into().unwrap_or_default(),
            auth_method: sta_auth_method,
            ..Default::default()
        },
        AccessPointConfiguration {
            ssid: ap_ssid.try_into().unwrap(),
            password: ap_pw.try_into().unwrap_or_default(),
            auth_method: ap_auth_method,
            channel: 0,
            ..Default::default()
        },
    ))?;
    wifi.start()?;

    Ok(wifi)
}

/// Initialize logging and load configuration from NVS
fn init_logging_and_config(nvs: EspDefaultNvsPartition) -> Result<Config> {
    config::init_nvs(nvs)?;
    let config = Config::load_or_default();

    let level = config.log_level.as_level_filter();
    if let Err(e) = esp_idf_svc::log::set_target_level("*", level) {
        warn!("Failed to set log level: {e}");
    } else {
        info!("Log level set to {:?}", config.log_level);
    }

    Ok(config)
}

/// Initialize WiFi driver and network interfaces
fn init_wifi(
    config: &Config,
    modem: esp_idf_hal::modem::Modem,
    sys_loop: EspSystemEventLoop,
    nvs: EspDefaultNvsPartition,
) -> Result<(EspWifi<'static>, String)> {
    info!("Initializing WiFi...");

    let wifi_driver = WifiDriver::new(modem, sys_loop, Some(nvs))?;
    let sta_netif = create_sta_netif(config)?;
    let ap_netif = create_ap_netif(config.ap_ip, config.ap_prefix_len)?;
    let wifi = EspWifi::wrap_all(wifi_driver, sta_netif, ap_netif)?;

    let ap_ssid = config.ap_ssid.clone();

    let ap_password = config.ap_password.clone();
    let ap_auth_method = match &ap_password {
        Some(pw) if !pw.is_empty() => AuthMethod::WPA2Personal,
        _ => AuthMethod::None,
    };

    let wifi = start_wifi(config, wifi, &ap_ssid, ap_password.as_deref(), ap_auth_method)?;

    let ap_ip_info = wifi.ap_netif().get_ip_info()?;
    info!("AP started - connect to '{ap_ssid}' and navigate to http://{}", ap_ip_info.ip);

    Ok((wifi, ap_ssid))
}

/// Spawn all background tasks
fn spawn_background_tasks(
    state: &Arc<State>,
    sse_rx: std::sync::mpsc::Receiver<sse_server::SseMessage>,
    test_control_rx: std::sync::mpsc::Receiver<TestControlMessage>,
    ap_hostname: String,
    ap_ip: Ipv4Addr,
) {
    // Start DNS server for captive portal
    dns::start_dns_server(ap_ip);

    // Start SSE server for metrics streaming (on port 81)
    {
        let state = state.clone();
        thread_util::spawn_named(c"sse_srv", move || {
            sse_server_task(&sse_rx, &state);
        });
    }

    // Start test task (handles modes 1-5)
    {
        let state = state.clone();
        thread_util::spawn_named(c"test_task", move || {
            obd2::test_task(&state, &test_control_rx);
        });
    }

    // Start web server
    {
        let state = state.clone();
        thread_util::spawn_named(c"web_srv", move || {
            if let Err(e) = web_server::start_server(&state, Some(&ap_hostname), ap_ip) {
                error!("Web server error: {e:?}");
            }
        });
    }

    // Start WiFi connection manager
    {
        let state = state.clone();
        thread_util::spawn_named(c"wifi_mgr", move || {
            wifi_connection_manager(&state);
        });
    }
}

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    info!("Starting tachtalk-test firmware...");

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let config = init_logging_and_config(nvs.clone())?;
    let (wifi, ap_ssid) = init_wifi(&config, peripherals.modem, sys_loop, nvs)?;

    let ap_hostname = ap_ssid.to_lowercase();
    let ap_ip = config.ap_ip;

    // Create channels
    let (sse_tx, sse_rx) = std::sync::mpsc::channel();
    let (test_control_tx, test_control_rx) = std::sync::mpsc::channel();

    // Create central State struct
    let state = Arc::new(State {
        config: Mutex::new(config),
        wifi: Mutex::new(wifi),
        ap_ssid,
        sse_tx,
        metrics: TestMetrics::default(),
        dongle_connected: AtomicBool::new(false),
        test_control_tx: Mutex::new(Some(test_control_tx)),
        capture_buffer: Mutex::new(Vec::new()),
    });

    spawn_background_tasks(
        &state,
        sse_rx,
        test_control_rx,
        ap_hostname,
        ap_ip,
    );

    // Start mDNS for local discovery
    let _mdns = setup_mdns();

    info!("All systems running!");

    // Main loop - metrics monitoring
    let mut cpu_snapshots = std::collections::HashMap::new();
    let mut cpu_total = 0u64;

    loop {
        for _ in 0..5 {
            FreeRtos::delay_ms(1000);
        }

        let (dump_cpu, dump_sockets) = {
            let cfg = state.config.lock().unwrap();
            (cfg.dump_cpu_metrics, cfg.dump_socket_info)
        };

        if dump_cpu {
            cpu_metrics::print_cpu_usage_deltas(&mut cpu_snapshots, &mut cpu_total);
        }
        if dump_sockets {
            web_server::log_sockets();
        }

        // Print test metrics
        let metrics = &state.metrics;
        if metrics.test_running.load(Ordering::Relaxed) {
            info!(
                "Test: {}/sec | total: {} | errors: {}",
                metrics.requests_per_sec.load(Ordering::Relaxed),
                metrics.total_requests.load(Ordering::Relaxed),
                metrics.total_errors.load(Ordering::Relaxed),
            );
        }
    }
}

/// Connection state for WiFi station interface
enum StaConnectionState {
    Disconnected,
    AwaitingIp,
    Connected(Ipv4Addr),
}

/// Background task to manage WiFi STA connection
fn wifi_connection_manager(state: &Arc<State>) {
    let watchdog = WatchdogHandle::register(c"wifi_manager");

    let sta_ssid = {
        let cfg_guard = state.config.lock().unwrap();
        cfg_guard.wifi.ssid.clone()
    };

    let mut was_connected = false;

    loop {
        watchdog.feed();

        let connection_state = {
            let wifi_guard = state.wifi.lock().unwrap();
            let l2_connected = match wifi_guard.is_connected() {
                Ok(connected) => connected,
                Err(e) => {
                    error!("Failed to check WiFi connection status: {e}");
                    false
                }
            };
            if l2_connected {
                match wifi_guard.sta_netif().get_ip_info() {
                    Ok(info) if !info.ip.is_unspecified() => {
                        StaConnectionState::Connected(info.ip)
                    }
                    Ok(_) => StaConnectionState::AwaitingIp,
                    Err(e) => {
                        error!("Failed to get STA IP info: {e}");
                        StaConnectionState::AwaitingIp
                    }
                }
            } else {
                StaConnectionState::Disconnected
            }
        };

        match connection_state {
            StaConnectionState::Connected(ip) => {
                if !was_connected {
                    info!("WiFi STA connected to '{sta_ssid}' with IP: {ip}");
                    was_connected = true;
                }
                FreeRtos::delay_ms(1000);
            }
            StaConnectionState::AwaitingIp => {
                FreeRtos::delay_ms(1000);
            }
            StaConnectionState::Disconnected => {
                if was_connected {
                    warn!("WiFi STA disconnected from '{sta_ssid}'");
                    was_connected = false;
                }

                debug!("Attempting to connect to '{sta_ssid}'...");

                {
                    let mut wifi_guard = state.wifi.lock().unwrap();
                    if let Err(e) = wifi_guard.connect() {
                        debug!("STA connection initiation failed: {e:?}");
                    }
                }

                for _ in 0..15 {
                    watchdog.feed();
                    FreeRtos::delay_ms(1000);

                    let wifi_guard = state.wifi.lock().unwrap();
                    match wifi_guard.is_connected() {
                        Ok(true) => break,
                        Ok(false) => {}
                        Err(e) => {
                            error!("Failed to check WiFi connection status: {e}");
                        }
                    }
                }
            }
        }
    }
}
