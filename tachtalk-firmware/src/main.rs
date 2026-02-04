use anyhow::Result;
use esp_idf_hal::delay::FreeRtos;
use esp_idf_hal::gpio::AnyIOPin;
use esp_idf_hal::prelude::*;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::mdns::EspMdns;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::netif::{EspNetif, NetifConfiguration, NetifStack};
use esp_idf_svc::wifi::{
    AccessPointConfiguration, AuthMethod, BlockingWifi, ClientConfiguration, Configuration,
    EspWifi, WifiDriver,
};
use esp_idf_svc::ipv4::{
    self, ClientConfiguration as IpClientConfiguration, ClientSettings as IpClientSettings,
    Configuration as IpConfiguration, Ipv4Addr, Mask, Subnet,
};
use log::{debug, info, warn, error};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod config;
mod dns;
mod leds;
mod obd2;
mod sse_server;
mod watchdog;
mod web_server;

use crate::watchdog::WatchdogHandle;
use config::Config;
use leds::LedController;
use obd2::{AtCommandLog, PidLog, Obd2Proxy, start_rpm_led_task};

const AP_SSID_PREFIX: &str = "TachTalk-";

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

/// Start WiFi in Mixed mode (AP + STA) and spawn connection manager thread
fn start_wifi(
    config: &Config,
    wifi: BlockingWifi<EspWifi<'static>>,
    wifi_mode: &Arc<Mutex<WifiMode>>,
    ap_ssid: &str,
    ap_password: Option<String>,
    ap_auth_method: AuthMethod,
) -> Result<Arc<Mutex<BlockingWifi<EspWifi<'static>>>>> {
    // Get STA credentials from config
    let sta_ssid = config.wifi.ssid.clone();
    let sta_password = config.wifi.password.clone().unwrap_or_default();
    let sta_auth_method = if sta_password.is_empty() { AuthMethod::None } else { AuthMethod::WPA2Personal };

    // Determine AP password for config
    let ap_pw = ap_password.as_deref().unwrap_or("");

    // Start WiFi in Mixed mode (AP + STA) so web UI is accessible while scanning
    info!("Starting WiFi in Mixed mode: AP '{ap_ssid}' + STA '{sta_ssid}'");
    let mut wifi = wifi;
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

    let wifi = Arc::new(Mutex::new(wifi));

    // Spawn connection manager thread
    let wifi_clone = wifi.clone();
    let wifi_mode_clone = wifi_mode.clone();
    let ap_ssid = ap_ssid.to_string();
    std::thread::spawn(move || {
        wifi_connection_manager(
            &wifi_clone,
            &wifi_mode_clone,
            &sta_ssid,
            &sta_password,
            &ap_ssid,
            ap_password,
            ap_auth_method,
        );
    });

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

    // Initialize configuration
    let config = Arc::new(Mutex::new(Config::load_or_default()));

    // Apply configured log level
    {
        let cfg = config.lock().unwrap();
        let level = cfg.log_level.as_level_filter();
        // Set for all targets (use "*" for global)
        if let Err(e) = esp_idf_svc::log::set_target_level("*", level) {
            warn!("Failed to set log level: {e}");
        } else {
            info!("Log level set to {:?}", cfg.log_level);
        }
    }

    let wifi_mode = Arc::new(Mutex::new(WifiMode::AccessPoint));

    // Initialize LED controller with GPIO from config
    let led_gpio = config.lock().unwrap().led_gpio;
    info!("Initializing LED controller on GPIO {led_gpio}...");
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
    let sta_netif = create_sta_netif(&config.lock().unwrap())?;
    
    // Create AP netif with captive portal DNS
    let ap_netif = create_ap_netif()?;
    
    let wifi = EspWifi::wrap_all(wifi_driver, sta_netif, ap_netif)?;
    let wifi = BlockingWifi::wrap(wifi, sys_loop)?;

    // Generate AP SSID from MAC address
    let mac = wifi.wifi().sta_netif().get_mac()?;
    let ap_ssid = format!("{}{:02X}{:02X}", AP_SSID_PREFIX, mac[4], mac[5]);
    let ap_hostname = ap_ssid.to_lowercase();

    // Get AP password from config
    let ap_password = config.lock().unwrap().ap_password.clone();
    let ap_auth_method = match &ap_password {
        Some(pw) if !pw.is_empty() => AuthMethod::WPA2Personal,
        _ => AuthMethod::None,
    };

    // Start WiFi in Mixed mode and spawn connection manager thread
    let wifi = start_wifi(
        &config.lock().unwrap(),
        wifi,
        &wifi_mode,
        &ap_ssid,
        ap_password,
        ap_auth_method,
    )?;

    let ap_ip_info = wifi.lock().unwrap().wifi().ap_netif().get_ip_info()?;
    info!("AP started - connect to '{ap_ssid}' and navigate to http://{}", ap_ip_info.ip);

    // Start DNS server for captive portal
    dns::start_dns_server();

    // Start SSE server for RPM streaming (on port 8081)
    let sse_tx = sse_server::start_sse_server();

    // Create shared log for tracking AT commands (for debugging via web UI)
    let at_command_log: AtCommandLog = Arc::new(Mutex::new(HashSet::new()));
    
    // Create shared log for tracking OBD2 PIDs (for debugging via web UI)
    let pid_log: PidLog = Arc::new(Mutex::new(HashSet::new()));

    // Start web server
    {
        let config_clone = config.clone();
        let mode_clone = wifi_mode.clone();
        let wifi_clone = wifi.clone();
        let ap_hostname_clone = ap_hostname.clone();
        let at_cmd_log_clone = at_command_log.clone();
        let pid_log_clone = pid_log.clone();

        std::thread::spawn(move || {
            if let Err(e) = web_server::start_server(&config_clone, &mode_clone, &wifi_clone, Some(ap_hostname_clone), at_cmd_log_clone, pid_log_clone) {
                error!("Web server error: {e:?}");
            }
        });
    }

    info!("Web server started - configuration available at http://{}", ap_ip_info.ip);

    // Start mDNS for local discovery (tachtalk.local)
    let _mdns = setup_mdns();

    // Start OBD2 proxy and RPM/LED task
    let dongle_tx = obd2::start_dongle_task(config.clone());
    
    // Start the combined RPM poller and LED update task
    // This takes ownership of led_controller (no Arc<Mutex> needed)
    let rpm_tx = start_rpm_led_task(led_controller, config.clone(), sse_tx.clone(), dongle_tx.clone());
    
    {
        let config_clone = config.clone();
        let rpm_tx_clone = rpm_tx.clone();
        let at_cmd_log_clone = at_command_log.clone();
        let pid_log_clone = pid_log.clone();
        
        std::thread::spawn(move || {
            let proxy = Obd2Proxy::new(config_clone, rpm_tx_clone, dongle_tx, at_cmd_log_clone, pid_log_clone);
            if let Err(e) = proxy.run() {
                error!("OBD2 proxy error: {e:?}");
            }
        });
    }
    info!("OBD2 proxy started");

    info!("All systems running!");

    // Main loop - keep alive
    loop {
        FreeRtos::delay_ms(1000);
    }
}

/// Configuration bundle for WiFi connection manager
struct WifiManagerContext<'a> {
    wifi: &'a Arc<Mutex<BlockingWifi<EspWifi<'static>>>>,
    wifi_mode: &'a Arc<Mutex<WifiMode>>,
    sta_ssid: &'a str,
    client_config: Configuration,
    mixed_config: Configuration,
    watchdog: WatchdogHandle,
}

/// Handle Mixed mode: scan for target network, attempt connection if found
fn handle_mixed_mode(ctx: &WifiManagerContext<'_>) {
    let sta_ssid = ctx.sta_ssid;
    debug!("Scanning for '{sta_ssid}'...");
    
    let target_found = {
        let mut wifi = ctx.wifi.lock().unwrap();
        match wifi.scan() {
            Ok(networks) => {
                let found = networks.iter().any(|ap| ap.ssid.as_str() == sta_ssid);
                if found {
                    info!("Found target network '{sta_ssid}' in scan results");
                } else {
                    debug!("Target network '{sta_ssid}' not found ({} networks seen)", networks.len());
                }
                found
            }
            Err(e) => {
                warn!("WiFi scan failed: {e:?}");
                false
            }
        }
    };
    ctx.watchdog.feed();

    if target_found {
        info!("Switching to STA-only mode to connect to '{sta_ssid}'");
        let mut wifi = ctx.wifi.lock().unwrap();
        
        // Stop WiFi before changing configuration
        if let Err(e) = wifi.stop() {
            warn!("Failed to stop WiFi for mode switch: {e:?}");
            return;
        }
        
        if let Err(e) = wifi.set_configuration(&ctx.client_config) {
            warn!("Failed to switch to STA-only mode: {e:?}");
            // Try to restart in Mixed mode
            if let Err(e) = wifi.set_configuration(&ctx.mixed_config) {
                warn!("Failed to restore Mixed mode config: {e:?}");
            }
            if let Err(e) = wifi.start() {
                warn!("Failed to restart WiFi after config failure: {e:?}");
            }
            return;
        }
        
        // Restart WiFi with new configuration
        if let Err(e) = wifi.start() {
            warn!("Failed to start WiFi in STA-only mode: {e:?}");
            if let Err(e) = wifi.set_configuration(&ctx.mixed_config) {
                warn!("Failed to restore Mixed mode config: {e:?}");
            }
            if let Err(e) = wifi.start() {
                warn!("Failed to restart WiFi after start failure: {e:?}");
            }
            return;
        }
        
        // Brief delay for WiFi driver to settle after mode switch
        FreeRtos::delay_ms(100);
        
        match wifi.connect() {
            Ok(()) => {
                // Wait for IP with watchdog-friendly polling (3s chunks, up to 15s total)
                for _ in 0..5 {
                    ctx.watchdog.feed();
                    let result = wifi.ip_wait_while(
                        || wifi.is_up().map(|up| !up),
                        Some(Duration::from_secs(3)),
                    );
                    if result.is_ok() {
                        if let Ok(ip_info) = wifi.wifi().sta_netif().get_ip_info() {
                            info!("WiFi STA connected to '{sta_ssid}' with IP: {}", ip_info.ip);
                            *ctx.wifi_mode.lock().unwrap() = WifiMode::Client;
                            return;
                        }
                    }
                    // Check if already up before next iteration
                    if wifi.is_up().unwrap_or(false) {
                        break;
                    }
                }
            }
            Err(e) => {
                warn!("STA connection failed: {e:?}");
            }
        }
        ctx.watchdog.feed();
        
        // Connection failed - switch back to Mixed mode
        warn!("Connection to '{sta_ssid}' failed, returning to Mixed mode");
        if let Err(e) = wifi.stop() {
            warn!("Failed to stop WiFi for fallback: {e:?}");
        }
        if let Err(e) = wifi.set_configuration(&ctx.mixed_config) {
            warn!("Failed to switch back to Mixed mode: {e:?}");
        }
        if let Err(e) = wifi.start() {
            warn!("Failed to restart WiFi in Mixed mode: {e:?}");
        }
    }
    
    // Wait before next scan
    for _ in 0..10 {
        FreeRtos::delay_ms(1000);
        ctx.watchdog.feed();
    }
}

/// Handle Client mode: monitor connection, switch back to Mixed if disconnected
fn handle_client_mode(ctx: &WifiManagerContext<'_>) {
    FreeRtos::delay_ms(1000);
    
    let connected = {
        let wifi = ctx.wifi.lock().unwrap();
        wifi.is_connected().unwrap_or(false)
    };
    
    if !connected {
        warn!("WiFi STA disconnected from '{}' - switching back to Mixed mode", ctx.sta_ssid);
        
        let mut wifi = ctx.wifi.lock().unwrap();
        if let Err(e) = wifi.stop() {
            warn!("Failed to stop WiFi for mode switch: {e:?}");
        }
        if let Err(e) = wifi.set_configuration(&ctx.mixed_config) {
            warn!("Failed to switch to Mixed mode: {e:?}");
        } else {
            info!("Switched to Mixed mode (AP re-enabled)");
            *ctx.wifi_mode.lock().unwrap() = WifiMode::AccessPoint;
        }
        if let Err(e) = wifi.start() {
            warn!("Failed to restart WiFi in Mixed mode: {e:?}");
        }
    }
}

/// Background task to manage WiFi STA connection
/// - In Mixed mode: AP running, scanning for target STA network
/// - In STA mode: Connected to STA, AP disabled
fn wifi_connection_manager(
    wifi: &Arc<Mutex<BlockingWifi<EspWifi<'static>>>>,
    wifi_mode: &Arc<Mutex<WifiMode>>,
    sta_ssid: &str,
    sta_password: &str,
    ap_ssid: &str,
    ap_password: Option<String>,
    ap_auth_method: AuthMethod,
) {
    let ap_pw = ap_password.unwrap_or_default();
    let sta_auth_method = if sta_password.is_empty() { AuthMethod::None } else { AuthMethod::WPA2Personal };

    let ctx = WifiManagerContext {
        wifi,
        wifi_mode,
        sta_ssid,
        client_config: Configuration::Client(ClientConfiguration {
            ssid: sta_ssid.try_into().unwrap_or_default(),
            password: sta_password.try_into().unwrap_or_default(),
            auth_method: sta_auth_method,
            ..Default::default()
        }),
        mixed_config: Configuration::Mixed(
            ClientConfiguration {
                ssid: sta_ssid.try_into().unwrap_or_default(),
                password: sta_password.try_into().unwrap_or_default(),
                auth_method: sta_auth_method,
                ..Default::default()
            },
            AccessPointConfiguration {
                ssid: ap_ssid.try_into().unwrap(),
                password: ap_pw.as_str().try_into().unwrap_or_default(),
                auth_method: ap_auth_method,
                channel: 0,
                ..Default::default()
            },
        ),
        watchdog: WatchdogHandle::register("wifi_manager"),
    };

    loop {
        ctx.watchdog.feed();
        let current_mode = *ctx.wifi_mode.lock().unwrap();

        match current_mode {
            WifiMode::AccessPoint => handle_mixed_mode(&ctx),
            WifiMode::Client => handle_client_mode(&ctx),
        }
    }
}
