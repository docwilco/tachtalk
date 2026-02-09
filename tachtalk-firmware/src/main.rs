use anyhow::Result;
use esp_idf_hal::delay::FreeRtos;
use esp_idf_hal::gpio::AnyIOPin;
use esp_idf_hal::prelude::*;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::ipv4::{
    self, ClientConfiguration as IpClientConfiguration, ClientSettings as IpClientSettings,
    Configuration as IpConfiguration, Ipv4Addr, Mask, Subnet,
};
use esp_idf_svc::mdns::EspMdns;
use esp_idf_svc::netif::{EspNetif, NetifConfiguration, NetifStack};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::{gpio_pull_mode_t_GPIO_PULLUP_ONLY, gpio_pullup_en, gpio_set_pull_mode};
use esp_idf_svc::wifi::{
    AccessPointConfiguration, AuthMethod, ClientConfiguration, Configuration, EspWifi, WifiDriver,
};
use log::{debug, error, info, warn};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

mod config;
mod controls;
mod cpu_metrics;
mod dns;
mod obd2;
mod rpm_leds;
mod sse_server;
mod thread_util;
mod watchdog;
mod web_server;

use crate::watchdog::WatchdogHandle;
use config::Config;
use obd2::{cache_manager_task, dongle_task, CacheManagerSender, DongleSender, Obd2Proxy};
use rpm_leds::{rpm_led_task, LedController, RpmTaskSender};
use sse_server::{sse_server_task, SseSender};

use std::sync::atomic::{AtomicBool, AtomicU32};

/// Metrics for PID polling
pub struct PollingMetrics {
    /// Number of PIDs in the fast polling queue
    pub fast_pid_count: AtomicU32,
    /// Number of PIDs in the slow polling queue
    pub slow_pid_count: AtomicU32,
    /// Total promotions from slow to fast
    pub promotions: AtomicU32,
    /// Total demotions from fast to slow
    pub demotions: AtomicU32,
    /// Total PIDs removed from polling
    pub removals: AtomicU32,
    /// Total dongle requests sent (wraps at `u32::MAX`)
    pub dongle_requests_total: AtomicU32,
    /// Dongle requests in the last second
    pub dongle_requests_per_sec: AtomicU32,
    /// List of PIDs in the fast queue (for display)
    pub fast_pids: Mutex<Vec<String>>,
    /// List of PIDs in the slow queue (for display)
    pub slow_pids: Mutex<Vec<String>>,
}

impl Default for PollingMetrics {
    fn default() -> Self {
        Self {
            fast_pid_count: AtomicU32::new(0),
            slow_pid_count: AtomicU32::new(0),
            promotions: AtomicU32::new(0),
            demotions: AtomicU32::new(0),
            removals: AtomicU32::new(0),
            dongle_requests_total: AtomicU32::new(0),
            dongle_requests_per_sec: AtomicU32::new(0),
            fast_pids: Mutex::new(Vec::new()),
            slow_pids: Mutex::new(Vec::new()),
        }
    }
}

/// Central state shared across tasks
///
/// Note: WiFi config changes require a reboot to take effect, which is consistent
/// with how `wifi_connection_manager` caches credentials.
pub struct State {
    pub config: Mutex<Config>,
    pub wifi: Mutex<EspWifi<'static>>,
    /// AP SSID (cached at startup from config)
    pub ap_ssid: String,
    pub sse_tx: SseSender,
    pub rpm_tx: RpmTaskSender,
    pub dongle_control_tx: DongleSender,
    pub cache_manager_tx: CacheManagerSender,
    pub shared_rpm: Mutex<Option<u32>>,
    pub at_command_log: Mutex<HashSet<String>>,
    pub pid_log: Mutex<HashSet<String>>,
    /// Whether we have an active TCP connection to the OBD2 dongle
    pub dongle_connected: AtomicBool,
    /// TCP connection info for dongle: (`local_addr`, `remote_addr`)
    pub dongle_tcp_info: Mutex<Option<(SocketAddr, SocketAddr)>>,
    /// Number of currently connected OBD2 clients (downstream)
    pub obd2_client_count: AtomicU32,
    /// TCP connection info for each client: (`local_addr`, `remote_addr`)
    pub client_tcp_info: Mutex<Vec<(SocketAddr, SocketAddr)>>,
    /// PID polling metrics
    pub polling_metrics: PollingMetrics,
    /// Cached responses for supported PIDs queries (0100, 0120, ..., 01E0)
    /// plus a `ready` flag indicating whether capability queries have completed.
    pub supported_pids: Mutex<obd2::SupportedPidsCache>,
    /// Whether the ECU supports multi-PID queries (e.g., `010C0D` for RPM + vehicle speed)
    pub supports_multi_pid: AtomicBool,
}

/// Create STA network interface with static IP or DHCP based on config
fn create_sta_netif(config: &Config) -> Result<EspNetif> {
    if config.ip.use_dhcp {
        info!("STA netif: DHCP mode");
        Ok(EspNetif::new(NetifStack::Sta)?)
    } else {
        // Parse static IP configuration
        let ip: Ipv4Addr = config
            .ip
            .ip
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid static IP: {}", config.ip.ip))?;
        let mask = config.ip.prefix_len;

        info!("STA netif: Static IP {ip}/{mask} (no gateway)");

        let mut sta_config = NetifConfiguration::wifi_default_client();
        sta_config.ip_configuration = Some(IpConfiguration::Client(IpClientConfiguration::Fixed(
            IpClientSettings {
                ip,
                subnet: Subnet {
                    gateway: Ipv4Addr::UNSPECIFIED,
                    mask: Mask(mask),
                },
                dns: None,
                secondary_dns: None,
            },
        )));
        Ok(EspNetif::new_with_conf(&sta_config)?)
    }
}

/// Create AP network interface with captive portal DNS configuration
fn create_ap_netif(ap_ip: Ipv4Addr, ap_prefix_len: u8) -> Result<EspNetif> {
    // Custom router config that uses our IP as DNS
    // (default uses 8.8.8.8 which bypasses our captive portal DNS)
    let ap_router_config = ipv4::RouterConfiguration {
        subnet: ipv4::Subnet {
            gateway: ap_ip,
            mask: ipv4::Mask(ap_prefix_len),
        },
        dhcp_enabled: true,
        dns: Some(ap_ip),           // Point to our DNS server
        secondary_dns: Some(ap_ip), // Also use our DNS
    };

    let mut ap_netif_config = NetifConfiguration::wifi_default_router();
    ap_netif_config.ip_configuration = Some(ipv4::Configuration::Router(ap_router_config));
    Ok(EspNetif::new_with_conf(&ap_netif_config)?)
}

/// Initialize mDNS for local discovery (tachtalk.local)
fn setup_mdns() -> Option<EspMdns> {
    match EspMdns::take() {
        Ok(mut m) => {
            let _ = m.set_hostname("tachtalk");
            let _ = m.set_instance_name("TachTalk Tachometer");
            let _ = m.add_service(None, "_http", "_tcp", 80, &[]);
            info!("mDNS started: tachtalk.local");
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
    // Get STA credentials from config
    let sta_ssid = config.wifi.ssid.clone();
    let sta_password = config.wifi.password.clone().unwrap_or_default();
    let sta_auth_method = if sta_password.is_empty() {
        AuthMethod::None
    } else {
        AuthMethod::WPA2Personal
    };

    // Determine AP password for config
    let ap_pw = ap_password.unwrap_or("");

    // Start WiFi in Mixed mode (AP + STA) so web UI is accessible while scanning
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

    // Apply configured log level
    let level = config.log_level.as_level_filter();
    if let Err(e) = esp_idf_svc::log::set_target_level("*", level) {
        warn!("Failed to set log level: {e}");
    } else {
        info!("Log level set to {:?}", config.log_level);
    }

    Ok(config)
}

/// Initialize LED controller with GPIO from config
fn init_led_controller<C: esp_idf_hal::rmt::RmtChannel>(
    config: &Config,
    rmt_channel: impl esp_idf_hal::peripheral::Peripheral<P = C> + 'static,
) -> Result<LedController> {
    let led_gpio = config.led_gpio;
    let brightness = config.brightness;
    info!("Initializing LED controller on GPIO {led_gpio} with brightness {brightness}...");

    // Reset the GPIO pin to clear any residual RMT configuration from previous boot
    // This ensures clean initialization when GPIO pin is changed via config
    unsafe {
        esp_idf_svc::sys::gpio_reset_pin(i32::from(led_gpio));
    }

    // SAFETY: We trust the user-configured GPIO pin number is valid for this board
    let led_pin = unsafe { AnyIOPin::new(i32::from(led_gpio)) };
    LedController::new(led_pin, rmt_channel, brightness)
}

/// Initialize rotary encoder if configured (both pins must be non-zero)
fn init_encoder<PCNT: esp_idf_hal::pcnt::Pcnt>(
    config: &Config,
    pcnt: impl esp_idf_hal::peripheral::Peripheral<P = PCNT> + 'static,
) -> Option<esp_idf_hal::pcnt::PcntDriver<'static>> {
    if config.encoder_pin_a == 0 || config.encoder_pin_b == 0 {
        debug!("Rotary encoder disabled (pins not configured)");
        return None;
    }

    info!(
        "Initializing rotary encoder on GPIO {} (A) and {} (B)...",
        config.encoder_pin_a, config.encoder_pin_b
    );

    // Configure pins with internal pull-ups (~45kΩ) for the encoder
    // SAFETY: We trust the user-configured GPIO pin numbers are valid
    unsafe {
        gpio_set_pull_mode(
            i32::from(config.encoder_pin_a),
            gpio_pull_mode_t_GPIO_PULLUP_ONLY,
        );
        gpio_set_pull_mode(
            i32::from(config.encoder_pin_b),
            gpio_pull_mode_t_GPIO_PULLUP_ONLY,
        );
        gpio_pullup_en(i32::from(config.encoder_pin_a));
        gpio_pullup_en(i32::from(config.encoder_pin_b));
    }

    let pin_a = unsafe { esp_idf_hal::gpio::AnyInputPin::new(i32::from(config.encoder_pin_a)) };
    let pin_b = unsafe { esp_idf_hal::gpio::AnyInputPin::new(i32::from(config.encoder_pin_b)) };

    match controls::init_encoder(pcnt, pin_a, pin_b) {
        Ok(driver) => {
            info!("Rotary encoder initialized");
            Some(driver)
        }
        Err(e) => {
            error!("Failed to initialize rotary encoder: {e:?}");
            None
        }
    }
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

    // AP SSID from config (default is MAC-derived, computed in Config::default())
    let ap_ssid = config.ap_ssid.clone();

    // Get AP password from config
    let ap_password = config.ap_password.clone();
    let ap_auth_method = match &ap_password {
        Some(pw) if !pw.is_empty() => AuthMethod::WPA2Personal,
        _ => AuthMethod::None,
    };

    // Start WiFi in Mixed mode
    let wifi = start_wifi(
        config,
        wifi,
        &ap_ssid,
        ap_password.as_deref(),
        ap_auth_method,
    )?;

    let ap_ip_info = wifi.ap_netif().get_ip_info()?;
    info!(
        "AP started - connect to '{ap_ssid}' and navigate to http://{}",
        ap_ip_info.ip
    );

    Ok((wifi, ap_ssid))
}

/// Create shared state, channels, and spawn all background tasks.
fn spawn_background_tasks(
    config: Config,
    wifi: EspWifi<'static>,
    ap_ssid: String,
    led_controller: LedController,
    encoder_driver: Option<esp_idf_hal::pcnt::PcntDriver<'static>>,
) -> Arc<State> {
    let ap_hostname = ap_ssid.to_lowercase();
    let ap_ip = config.ap_ip;

    // Create all channels
    let (sse_tx, sse_rx) = std::sync::mpsc::channel();
    let (dongle_control_tx, dongle_control_rx) = std::sync::mpsc::channel();
    let (cache_manager_tx, cache_manager_rx) = std::sync::mpsc::channel();
    let (rpm_tx, rpm_rx) = std::sync::mpsc::channel();

    let state = Arc::new(State {
        config: Mutex::new(config),
        wifi: Mutex::new(wifi),
        ap_ssid,
        sse_tx,
        rpm_tx,
        dongle_control_tx,
        cache_manager_tx,
        shared_rpm: Mutex::new(None),
        at_command_log: Mutex::new(HashSet::new()),
        pid_log: Mutex::new(HashSet::new()),
        dongle_connected: AtomicBool::new(false),
        dongle_tcp_info: Mutex::new(None),
        obd2_client_count: AtomicU32::new(0),
        client_tcp_info: Mutex::new(Vec::new()),
        polling_metrics: PollingMetrics::default(),
        supported_pids: Mutex::new(obd2::SupportedPidsCache::default()),
        supports_multi_pid: AtomicBool::new(false),
    });

    // Start DNS server for captive portal
    dns::start_dns_server(ap_ip);

    // Start SSE server for RPM streaming (on port 81)
    {
        let state = state.clone();
        thread_util::spawn_named(c"sse_srv", move || {
            sse_server_task(&sse_rx, &state);
        });
    }

    // Start OBD2 dongle task
    {
        let state = state.clone();
        thread_util::spawn_named(c"dongle", move || {
            dongle_task(&state, &dongle_control_rx);
        });
    }

    // Start cache manager task
    {
        let state = state.clone();
        thread_util::spawn_named(c"cache_mgr", move || {
            cache_manager_task(&state, &cache_manager_rx);
        });
    }

    // Start the combined RPM poller and LED update task
    // Pin to Core 1 to avoid interference from WiFi (which runs on Core 0)
    {
        let state = state.clone();
        thread_util::spawn_named_on_core(c"rpm_led", esp_idf_hal::cpu::Core::Core1, move || {
            rpm_led_task(&state, led_controller, rpm_rx);
        });
    }

    // Start web server
    {
        let state = state.clone();
        thread_util::spawn_named(c"web_srv", move || {
            if let Err(e) = web_server::start_server(&state, Some(ap_hostname), ap_ip) {
                error!("Web server error: {e:?}");
            }
        });
    }

    // Start OBD2 proxy
    {
        let state = state.clone();
        thread_util::spawn_named(c"obd2_proxy", move || {
            let proxy = Obd2Proxy::new(state);
            if let Err(e) = proxy.run() {
                error!("OBD2 proxy error: {e:?}");
            }
        });
    }
    info!("OBD2 proxy started");

    // Start WiFi connection manager
    {
        let state = state.clone();
        thread_util::spawn_named(c"wifi_mgr", move || {
            wifi_connection_manager(&state);
        });
    }

    // Start controls task (rotary encoder and/or button)
    if let Some(driver) = encoder_driver {
        let state = state.clone();
        thread_util::spawn_named(c"controls", move || {
            controls::controls_task(&state, driver);
        });
    }

    state
}

fn main() -> Result<()> {
    // It is necessary to call this function once. Otherwise some patches to the runtime
    // implemented by esp-idf-sys might not link properly. See https://github.com/esp-rs/esp-idf-template/issues/71
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    info!("Starting tachtalk firmware...");
    info!(
        "LWIP_MAX_SOCKETS: {}",
        esp_idf_svc::sys::CONFIG_LWIP_MAX_SOCKETS
    );
    info!(
        "Obd2Buffer size: {} bytes",
        std::mem::size_of::<obd2::Obd2Buffer>()
    );

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let config = init_logging_and_config(nvs.clone())?;
    let led_controller = init_led_controller(&config, peripherals.rmt.channel0)?;
    let encoder_driver = init_encoder(&config, peripherals.pcnt0);

    let (wifi, ap_ssid) = init_wifi(&config, peripherals.modem, sys_loop, nvs)?;

    let state = spawn_background_tasks(config, wifi, ap_ssid, led_controller, encoder_driver);

    // Start mDNS for local discovery (tachtalk.local)
    let _mdns = setup_mdns();

    info!("All systems running!");

    // Main loop - CPU usage and polling metrics monitoring
    let mut cpu_snapshots = std::collections::HashMap::new();
    let mut cpu_total = 0u64;

    loop {
        use std::sync::atomic::Ordering;
        for _ in 0..5 {
            FreeRtos::delay_ms(1000);
        }

        // Check config for debug dump settings
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

        // Print polling metrics
        let metrics = &state.polling_metrics;
        info!(
            "Polling: {} fast, {} slow PIDs | {}/sec | +{} -{} x{}",
            metrics.fast_pid_count.load(Ordering::Relaxed),
            metrics.slow_pid_count.load(Ordering::Relaxed),
            metrics.dongle_requests_per_sec.load(Ordering::Relaxed),
            metrics.promotions.load(Ordering::Relaxed),
            metrics.demotions.load(Ordering::Relaxed),
            metrics.removals.load(Ordering::Relaxed),
        );
        if let Ok(fast) = metrics.fast_pids.lock() {
            if !fast.is_empty() {
                info!("  Fast: {}", fast.join(", "));
            }
        }
        if let Ok(slow) = metrics.slow_pids.lock() {
            if !slow.is_empty() {
                info!("  Slow: {}", slow.join(", "));
            }
        }
    }
}

/// Connection state for WiFi station interface
enum StaConnectionState {
    /// Not connected at L2 (WiFi association)
    Disconnected,
    /// L2 connected, waiting for IP (DHCP or static IP being applied)
    AwaitingIp,
    /// Fully connected with a valid IP address
    Connected(Ipv4Addr),
}

/// Background task to manage WiFi STA connection
/// Always runs in Mixed mode (AP + STA) - AP is never disabled
fn wifi_connection_manager(state: &Arc<State>) {
    let watchdog = WatchdogHandle::register(c"wifi_manager");

    // Read STA SSID from config (cached at task start - changes require reboot)
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
                    Ok(info) if !info.ip.is_unspecified() => StaConnectionState::Connected(info.ip),
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
                // Fully connected with IP - just monitor
                if !was_connected {
                    info!("WiFi STA connected to '{sta_ssid}' with IP: {ip}");
                    was_connected = true;
                }
                FreeRtos::delay_ms(1000);
            }
            StaConnectionState::AwaitingIp => {
                // L2 connected but waiting for IP - don't call connect(), just wait
                FreeRtos::delay_ms(1000);
            }
            StaConnectionState::Disconnected => {
                // Not connected at L2 - try to connect
                if was_connected {
                    warn!("WiFi STA disconnected from '{sta_ssid}'");
                    was_connected = false;
                }

                debug!("Attempting to connect to '{sta_ssid}'...");

                // Initiate connection (non-blocking)
                {
                    let mut wifi_guard = state.wifi.lock().unwrap();
                    if let Err(e) = wifi_guard.connect() {
                        debug!("STA connection initiation failed: {e:?}");
                    }
                }

                // Wait for L2 connection or timeout (15s)
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
