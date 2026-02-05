use anyhow::Result;
use esp_idf_hal::delay::FreeRtos;
use esp_idf_hal::gpio::AnyIOPin;
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
use log::{debug, info, warn, error};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

mod config;
mod cpu_stats;
mod dns;
mod leds;
mod obd2;
mod sse_server;
mod thread_util;
mod watchdog;
mod web_server;

use crate::watchdog::WatchdogHandle;
use config::Config;
use leds::LedController;
use obd2::{dongle_task, rpm_led_task, DongleSender, Obd2Proxy, RpmTaskSender};
use sse_server::{sse_server_task, SseSender};

use std::sync::atomic::{AtomicBool, AtomicU32};

const AP_SSID_PREFIX: &str = "TachTalk-";

/// Central state shared across tasks
///
/// Note: `ap_ssid` is stored here (not just in Config) because the default value
/// requires the WiFi MAC address, which isn't available until after WiFi driver init.
/// We resolve it once at startup: use `config.ap_ssid` if set, otherwise generate
/// from MAC. This also means WiFi config changes require a reboot to take effect,
/// which is consistent with how `wifi_connection_manager` caches credentials.
pub struct State {
    pub config: Mutex<Config>,
    pub wifi: Mutex<EspWifi<'static>>,
    pub wifi_mode: Mutex<WifiMode>,
    /// Resolved AP SSID (from config or MAC-derived default)
    pub ap_ssid: String,
    pub sse_tx: SseSender,
    pub dongle_tx: DongleSender,
    pub rpm_tx: RpmTaskSender,
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
}

/// Convert a subnet mask string (e.g., "255.255.255.0") to CIDR prefix length (e.g., 24)
fn subnet_mask_to_cidr(mask_str: &str) -> Result<u8> {
    let mask: Ipv4Addr = mask_str.parse().map_err(|e| {
        anyhow::anyhow!("Invalid subnet mask '{mask_str}': {e}")
    })?;
    let bits = u32::from(mask);
    // Validate it's a valid mask (all 1s followed by all 0s)
    let leading_ones = bits.leading_ones();
    let expected = if leading_ones == 32 {
        u32::MAX
    } else {
        u32::MAX << (32 - leading_ones)
    };
    if bits != expected {
        return Err(anyhow::anyhow!(
            "Invalid subnet mask '{mask_str}': not a valid mask (expected contiguous 1s)"
        ));
    }
    Ok(u8::try_from(leading_ones).unwrap())
}

/// `WiFi` mode the device is running in
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum WifiMode {
    /// Running as an access point (no STA configured or STA disconnected)
    AccessPoint,
    /// Connected to a configured network as a client (AP disabled)
    Client,
}

/// Create STA network interface with static IP or DHCP based on config
fn create_sta_netif(config: &Config) -> Result<EspNetif> {
    if config.ip.use_dhcp {
        info!("STA netif: DHCP mode");
        Ok(EspNetif::new(NetifStack::Sta)?)
    } else {
        // Parse static IP configuration
        let ip_str = config.ip.effective_ip().unwrap();
        let gateway_str = config.ip.effective_gateway().unwrap();
        let subnet_str = config.ip.effective_subnet().unwrap();

        let ip: Ipv4Addr = ip_str.parse().map_err(|e| {
            anyhow::anyhow!("Invalid static IP '{ip_str}': {e}")
        })?;
        let gateway: Ipv4Addr = gateway_str.parse().map_err(|e| {
            anyhow::anyhow!("Invalid gateway '{gateway_str}': {e}")
        })?;
        let mask = subnet_mask_to_cidr(subnet_str)?;

        // Parse optional DNS
        let dns = config.ip.dns.as_ref().and_then(|s| s.parse().ok());

        info!("STA netif: Static IP {ip} gateway {gateway}/{mask} dns {dns:?}");

        let mut sta_config = NetifConfiguration::wifi_default_client();
        sta_config.ip_configuration = Some(IpConfiguration::Client(
            IpClientConfiguration::Fixed(IpClientSettings {
                ip,
                subnet: Subnet {
                    gateway,
                    mask: Mask(mask),
                },
                dns,
                secondary_dns: None,
            }),
        ));
        Ok(EspNetif::new_with_conf(&sta_config)?)
    }
}

/// Create AP network interface with captive portal DNS configuration
fn create_ap_netif() -> Result<EspNetif> {
    // Custom router config that uses our IP as DNS
    // (default uses 8.8.8.8 which bypasses our captive portal DNS)
    let ap_router_config = ipv4::RouterConfiguration {
        subnet: ipv4::Subnet {
            gateway: Ipv4Addr::new(192, 168, 71, 1),
            mask: ipv4::Mask(24),
        },
        dhcp_enabled: true,
        dns: Some(Ipv4Addr::new(192, 168, 71, 1)),           // Point to our DNS server
        secondary_dns: Some(Ipv4Addr::new(192, 168, 71, 1)), // Also use our DNS
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
    let sta_auth_method = if sta_password.is_empty() { AuthMethod::None } else { AuthMethod::WPA2Personal };

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

fn main() -> Result<()> {
    // It is necessary to call this function once. Otherwise some patches to the runtime
    // implemented by esp-idf-sys might not link properly. See https://github.com/esp-rs/esp-idf-template/issues/71
    esp_idf_svc::sys::link_patches();

    // Bind the log crate to the ESP Logging facilities
    esp_idf_svc::log::EspLogger::initialize_default();

    info!("Starting tachtalk firmware...");
    info!("LWIP_MAX_SOCKETS: {}", esp_idf_svc::sys::CONFIG_LWIP_MAX_SOCKETS);
    info!("Obd2Buffer size: {} bytes", std::mem::size_of::<obd2::Obd2Buffer>());

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // Initialize NVS for config storage
    config::init_nvs(nvs.clone())?;

    // Load configuration
    let config = Config::load_or_default();

    // Apply configured log level
    {
        let level = config.log_level.as_level_filter();
        // Set for all targets (use "*" for global)
        if let Err(e) = esp_idf_svc::log::set_target_level("*", level) {
            warn!("Failed to set log level: {e}");
        } else {
            info!("Log level set to {:?}", config.log_level);
        }
    }

    // Initialize LED controller with GPIO from config
    let led_gpio = config.led_gpio;
    info!("Initializing LED controller on GPIO {led_gpio}...");
    // Reset the GPIO pin to clear any residual RMT configuration from previous boot
    // This ensures clean initialization when GPIO pin is changed via config
    unsafe {
        esp_idf_svc::sys::gpio_reset_pin(i32::from(led_gpio));
    }
    // SAFETY: We trust the user-configured GPIO pin number is valid for this board
    let led_pin = unsafe { AnyIOPin::new(i32::from(led_gpio)) };
    let led_controller = LedController::new(
        led_pin,
        peripherals.rmt.channel0,
    )?;

    // Initialize WiFi with custom AP configuration for captive portal DNS
    info!("Initializing WiFi...");
    
    // Create WiFi driver
    let wifi_driver = WifiDriver::new(peripherals.modem, sys_loop.clone(), Some(nvs.clone()))?;
    
    // Create STA netif - static IP or DHCP based on config
    let sta_netif = create_sta_netif(&config)?;
    
    // Create AP netif with captive portal DNS
    let ap_netif = create_ap_netif()?;
    
    let wifi = EspWifi::wrap_all(wifi_driver, sta_netif, ap_netif)?;

    // Generate AP SSID: use config override or derive from MAC address
    let mac = wifi.sta_netif().get_mac()?;
    let ap_ssid = config.ap_ssid.clone().unwrap_or_else(|| {
        format!("{}{:02X}{:02X}", AP_SSID_PREFIX, mac[4], mac[5])
    });
    let ap_hostname = ap_ssid.to_lowercase();

    // Get AP password from config
    let ap_password = config.ap_password.clone();
    let ap_auth_method = match &ap_password {
        Some(pw) if !pw.is_empty() => AuthMethod::WPA2Personal,
        _ => AuthMethod::None,
    };

    // Start WiFi in Mixed mode
    let wifi = start_wifi(
        &config,
        wifi,
        &ap_ssid,
        ap_password.as_deref(),
        ap_auth_method,
    )?;

    let ap_ip_info = wifi.ap_netif().get_ip_info()?;
    info!("AP started - connect to '{ap_ssid}' and navigate to http://{}", ap_ip_info.ip);

    // Create all channels upfront
    let (sse_tx, sse_rx) = std::sync::mpsc::channel();
    let (dongle_tx, dongle_rx) = std::sync::mpsc::channel();
    let (rpm_tx, rpm_rx) = std::sync::mpsc::channel();

    // Create central State struct
    let state = Arc::new(State {
        config: Mutex::new(config),
        wifi: Mutex::new(wifi),
        wifi_mode: Mutex::new(WifiMode::AccessPoint),
        ap_ssid,
        sse_tx,
        dongle_tx,
        rpm_tx,
        shared_rpm: Mutex::new(None),
        at_command_log: Mutex::new(HashSet::new()),
        pid_log: Mutex::new(HashSet::new()),
        dongle_connected: AtomicBool::new(false),
        dongle_tcp_info: Mutex::new(None),
        obd2_client_count: AtomicU32::new(0),
        client_tcp_info: Mutex::new(Vec::new()),
    });

    // Start DNS server for captive portal
    dns::start_dns_server();

    // Start SSE server for RPM streaming (on port 8081)
    {
        thread_util::spawn_named(c"sse_srv", move || {
            sse_server_task(&sse_rx);
        });
    }

    // Start OBD2 dongle task
    {
        let state = state.clone();
        thread_util::spawn_named(c"dongle", move || {
            dongle_task(&state, &dongle_rx);
        });
    }

    // Start the combined RPM poller and LED update task
    {
        let state = state.clone();
        thread_util::spawn_named(c"rpm_led", move || {
            rpm_led_task(&state, led_controller, rpm_rx);
        });
    }

    // Start web server
    {
        let state = state.clone();
        let ap_hostname_clone = ap_hostname.clone();
        thread_util::spawn_named(c"web_srv", move || {
            if let Err(e) = web_server::start_server(&state, Some(ap_hostname_clone)) {
                error!("Web server error: {e:?}");
            }
        });
    }

    info!("Web server started - configuration available at http://{}", ap_ip_info.ip);

    // Start mDNS for local discovery (tachtalk.local)
    let _mdns = setup_mdns();

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

    info!("All systems running!");

    // CPU usage monitoring
    let mut cpu_snapshots = std::collections::HashMap::new();
    let mut cpu_total = 0u64;

    // Main loop - keep alive and print CPU stats
    loop {
        // Sleep for 5 seconds (5 iterations of 1s)
        for _ in 0..5 {
            FreeRtos::delay_ms(1000);
        }
        cpu_stats::print_cpu_usage_deltas(&mut cpu_snapshots, &mut cpu_total);
    }
}

/// Configuration bundle for WiFi connection manager
struct WifiManagerContext<'a> {
    state: &'a Arc<State>,
    client_config: Configuration,
    mixed_config: Configuration,
    watchdog: WatchdogHandle,
}

impl WifiManagerContext<'_> {
    /// Extract STA SSID from the client configuration
    fn sta_ssid(&self) -> &str {
        match &self.client_config {
            Configuration::Client(c) => c.ssid.as_str(),
            _ => unreachable!("client_config is always Configuration::Client"),
        }
    }

    /// Handle Mixed mode: attempt to connect to configured STA network, switch to Station mode on success
    fn handle_mixed_mode(&self) {
        let sta_ssid = self.sta_ssid();
        debug!("Attempting to connect to '{sta_ssid}'...");
        
        // Initiate connection (non-blocking)
        {
            let mut wifi_guard = self.state.wifi.lock().unwrap();
            if let Err(e) = wifi_guard.connect() {
                debug!("STA connection initiation failed: {e:?}");
                drop(wifi_guard);
                // Wait before next attempt
                for _ in 0..2 {
                    FreeRtos::delay_ms(1000);
                    self.watchdog.feed();
                }
                return;
            }
        }
        
        // Poll for connection with 15s timeout, releasing mutex between polls
        for _ in 0..15 {
            self.watchdog.feed();
            FreeRtos::delay_ms(1000);
            
            let wifi_guard = self.state.wifi.lock().unwrap();
            if wifi_guard.is_connected().unwrap_or(false) {
                if let Ok(ip_info) = wifi_guard.sta_netif().get_ip_info() {
                    if !ip_info.ip.is_unspecified() {
                        info!("WiFi STA connected to '{sta_ssid}' with IP: {}", ip_info.ip);
                        drop(wifi_guard);
                        self.switch_to_station_mode();
                        return;
                    }
                }
            }
        }
        
        debug!("Connection to '{sta_ssid}' timed out after 15s");
        
        // Wait before next attempt
        for _ in 0..2 {
            FreeRtos::delay_ms(1000);
            self.watchdog.feed();
        }
    }
    
    /// Switch from Mixed mode to Station-only mode after successful connection
    fn switch_to_station_mode(&self) {
        info!("Switching to Station-only mode");
        
        {
            let mut wifi_guard = self.state.wifi.lock().unwrap();
            
            if let Err(e) = wifi_guard.stop() {
                warn!("Failed to stop WiFi for mode switch: {e:?}");
                return;
            }
            
            if let Err(e) = wifi_guard.set_configuration(&self.client_config) {
                warn!("Failed to switch to Station-only mode: {e:?}");
                self.restore_mixed_mode(&mut wifi_guard);
                return;
            }
            
            if let Err(e) = wifi_guard.start() {
                warn!("Failed to start WiFi in Station-only mode: {e:?}");
                self.restore_mixed_mode(&mut wifi_guard);
                return;
            }
            
            // Brief delay for WiFi driver to settle after mode switch
            FreeRtos::delay_ms(100);
            
            // Reconnect in Station-only mode (non-blocking)
            if let Err(e) = wifi_guard.connect() {
                warn!("Failed to reconnect after mode switch: {e:?}");
                self.restore_mixed_mode(&mut wifi_guard);
                return;
            }
        }
        
        // Poll for reconnection with 15s timeout, releasing mutex between polls
        for _ in 0..15 {
            self.watchdog.feed();
            FreeRtos::delay_ms(1000);
            
            let wifi_guard = self.state.wifi.lock().unwrap();
            if wifi_guard.is_connected().unwrap_or(false) {
                if let Ok(ip_info) = wifi_guard.sta_netif().get_ip_info() {
                    if !ip_info.ip.is_unspecified() {
                        info!("Reconnected in Station-only mode with IP: {}", ip_info.ip);
                        drop(wifi_guard);
                        *self.state.wifi_mode.lock().unwrap() = WifiMode::Client;
                        return;
                    }
                }
            }
        }
        
        // Reconnection failed - fall back to Mixed mode
        warn!("Failed to reconnect in Station-only mode after 15s, falling back to Mixed mode");
        let mut wifi_guard = self.state.wifi.lock().unwrap();
        self.restore_mixed_mode(&mut wifi_guard);
    }
    
    /// Restore Mixed mode configuration after a failed mode switch
    fn restore_mixed_mode(&self, wifi_guard: &mut EspWifi<'static>) {
        if let Err(e) = wifi_guard.stop() {
            warn!("Failed to stop WiFi for fallback: {e:?}");
        }
        if let Err(e) = wifi_guard.set_configuration(&self.mixed_config) {
            warn!("Failed to restore Mixed mode config: {e:?}");
        }
        if let Err(e) = wifi_guard.start() {
            warn!("Failed to restart WiFi in Mixed mode: {e:?}");
        }
    }

    /// Handle Client mode: monitor connection, switch back to Mixed if disconnected
    fn handle_client_mode(&self) {
        FreeRtos::delay_ms(1000);
        self.watchdog.feed();
        
        let connected = {
            let wifi_guard = self.state.wifi.lock().unwrap();
            wifi_guard.is_connected().unwrap_or(false)
        };
        
        if !connected {
            warn!("WiFi STA disconnected from '{}' - switching back to Mixed mode", self.sta_ssid());
            
            let mut wifi_guard = self.state.wifi.lock().unwrap();
            if let Err(e) = wifi_guard.stop() {
                warn!("Failed to stop WiFi for mode switch: {e:?}");
            }
            if let Err(e) = wifi_guard.set_configuration(&self.mixed_config) {
                warn!("Failed to switch to Mixed mode: {e:?}");
            } else {
                info!("Switched to Mixed mode (AP re-enabled)");
                *self.state.wifi_mode.lock().unwrap() = WifiMode::AccessPoint;
            }
            if let Err(e) = wifi_guard.start() {
                warn!("Failed to restart WiFi in Mixed mode: {e:?}");
            }
        }
    }
}

/// Background task to manage WiFi STA connection
/// - In Mixed mode: AP running, attempting to connect to configured STA network
/// - In STA mode: Connected to STA, AP disabled
fn wifi_connection_manager(state: &Arc<State>) {
    // Read credentials from config (cached at task start - changes require reboot)
    let (sta_ssid, sta_password, ap_password) = {
        let cfg_guard = state.config.lock().unwrap();
        (
            cfg_guard.wifi.ssid.clone(),
            cfg_guard.wifi.password.clone().unwrap_or_default(),
            cfg_guard.ap_password.clone().unwrap_or_default(),
        )
    };
    let ap_ssid = &state.ap_ssid;
    
    let sta_auth_method = if sta_password.is_empty() { AuthMethod::None } else { AuthMethod::WPA2Personal };
    let ap_auth_method = if ap_password.is_empty() { AuthMethod::None } else { AuthMethod::WPA2Personal };

    let ctx = WifiManagerContext {
        state,
        client_config: Configuration::Client(ClientConfiguration {
            ssid: sta_ssid.as_str().try_into().unwrap_or_default(),
            password: sta_password.as_str().try_into().unwrap_or_default(),
            auth_method: sta_auth_method,
            ..Default::default()
        }),
        mixed_config: Configuration::Mixed(
            ClientConfiguration {
                ssid: sta_ssid.as_str().try_into().unwrap_or_default(),
                password: sta_password.as_str().try_into().unwrap_or_default(),
                auth_method: sta_auth_method,
                ..Default::default()
            },
            AccessPointConfiguration {
                ssid: ap_ssid.as_str().try_into().unwrap(),
                password: ap_password.as_str().try_into().unwrap_or_default(),
                auth_method: ap_auth_method,
                channel: 0,
                ..Default::default()
            },
        ),
        watchdog: WatchdogHandle::register("wifi_manager"),
    };

    loop {
        ctx.watchdog.feed();
        let current_mode = *ctx.state.wifi_mode.lock().unwrap();

        match current_mode {
            WifiMode::AccessPoint => ctx.handle_mixed_mode(),
            WifiMode::Client => ctx.handle_client_mode(),
        }
    }
}
