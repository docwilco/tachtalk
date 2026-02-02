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
use esp_idf_svc::ipv4::{self, Ipv4Addr};
use log::{info, warn, error};
use std::sync::{Arc, Mutex};

mod config;
mod dns;
mod leds;
mod obd2;
mod watchdog;
mod web_server;

use config::Config;
use leds::LedController;
use obd2::Obd2Proxy;

const AP_SSID_PREFIX: &str = "TachTalk-";

/// `WiFi` mode the device is running in
#[derive(Clone, Copy, PartialEq)]
pub enum WifiMode {
    /// Connected to a configured network as a client
    Client,
    /// Running as an access point for configuration
    AccessPoint,
    /// Mixed mode: AP running while attempting to connect to configured network
    Mixed,
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
    let led_pin = unsafe { AnyIOPin::new(led_gpio as i32) };
    let led_controller = Arc::new(Mutex::new(LedController::new(
        led_pin,
        peripherals.rmt.channel0,
    )?));

    // Initialize WiFi with custom AP configuration for captive portal DNS
    info!("Initializing WiFi...");
    
    // Create WiFi driver
    let wifi_driver = WifiDriver::new(peripherals.modem, sys_loop.clone(), Some(nvs.clone()))?;
    
    // Create STA netif with default config
    let sta_netif = EspNetif::new(NetifStack::Sta)?;
    
    // Create AP netif with custom router config that uses our IP as DNS
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
    let ap_netif = EspNetif::new_with_conf(&ap_netif_config)?;
    
    let wifi = EspWifi::wrap_all(wifi_driver, sta_netif, ap_netif)?;
    let mut wifi = BlockingWifi::wrap(wifi, sys_loop)?;

    // Check if WiFi is configured
    let wifi_configured = {
        let cfg = config.lock().unwrap();
        cfg.wifi.is_configured()
    };

    let current_mode = if wifi_configured {
        // Try to connect to configured network
        let (ssid, password, ap_password) = {
            let cfg = config.lock().unwrap();
            (cfg.wifi.ssid.clone(), cfg.wifi.password.clone().unwrap_or_default(), cfg.ap_password.clone())
        };

        info!("Attempting to connect to WiFi: {ssid}");

        wifi.set_configuration(&Configuration::Client(ClientConfiguration {
            ssid: ssid.as_str().try_into().unwrap_or_default(),
            password: password.as_str().try_into().unwrap_or_default(),
            ..Default::default()
        }))?;

        wifi.start()?;

        match wifi.connect() {
            Ok(()) => {
                info!("WiFi connected");
                if let Err(e) = wifi.wait_netif_up() {
                    warn!("Failed to get IP: {e:?}, starting mixed mode");
                    start_mixed_mode(&mut wifi, ap_password, &ssid, &password)?
                } else {
                    let ip_info = wifi.wifi().sta_netif().get_ip_info()?;
                    info!("WiFi IP Info: {ip_info:?}");
                    WifiMode::Client
                }
            }
            Err(e) => {
                warn!("WiFi connection failed: {e:?}, starting mixed mode");
                start_mixed_mode(&mut wifi, ap_password, &ssid, &password)?
            }
        }
    } else {
        info!("No WiFi configured, starting AP mode");
        let ap_password = config.lock().unwrap().ap_password.clone();
        start_ap_mode(&mut wifi, ap_password)?
    };

    *wifi_mode.lock().unwrap() = current_mode;

    // Start mDNS service in client mode for tachtalk.local access
    let _mdns = if current_mode == WifiMode::Client {
        match EspMdns::take() {
            Ok(mut mdns) => {
                if let Err(e) = mdns.set_hostname("tachtalk") {
                    warn!("Failed to set mDNS hostname: {e:?}");
                } else if let Err(e) = mdns.set_instance_name("TachTalk Tachometer") {
                    warn!("Failed to set mDNS instance name: {e:?}");
                } else if let Err(e) = mdns.add_service(None, "_http", "_tcp", 80, &[]) {
                    warn!("Failed to add mDNS HTTP service: {e:?}");
                } else {
                    info!("mDNS started: tachtalk.local");
                }
                Some(mdns)
            }
            Err(e) => {
                warn!("Failed to initialize mDNS: {e:?}");
                None
            }
        }
    } else {
        None
    };

    // Get the AP hostname for captive portal (used in AP and Mixed modes)
    let ap_hostname = if current_mode == WifiMode::AccessPoint || current_mode == WifiMode::Mixed {
        let mac = wifi.wifi().sta_netif().get_mac().unwrap_or([0u8; 6]);
        let hostname = format!("{}{:02X}{:02X}", AP_SSID_PREFIX, mac[4], mac[5]).to_lowercase();
        
        // Start DNS server for captive portal
        dns::start_dns_server();
        
        Some(hostname)
    } else {
        None
    };

    // Start SSE manager for RPM streaming
    let sse_tx = web_server::start_sse_manager();

    // Start web server
    let config_clone = config.clone();
    let led_clone = led_controller.clone();
    let mode_clone = wifi_mode.clone();
    let wifi = Arc::new(Mutex::new(wifi));
    let wifi_clone = wifi.clone();
    let sse_tx_clone = sse_tx.clone();

    std::thread::spawn(move || {
        if let Err(e) = web_server::start_server(config_clone, led_clone, mode_clone, wifi_clone, sse_tx_clone, ap_hostname) {
            error!("Web server error: {e:?}");
        }
    });

    // Only start OBD2 proxy in client mode
    if current_mode == WifiMode::Client {
        let dongle_tx = obd2::start_dongle_task();

        let config_clone = config.clone();
        let led_clone = led_controller.clone();
        let proxy = Obd2Proxy::new(config_clone, led_clone, sse_tx, dongle_tx);

        std::thread::spawn(move || {
            if let Err(e) = proxy.run() {
                error!("OBD2 proxy error: {e:?}");
            }
        });
    } else {
        info!("OBD2 proxy disabled in AP mode - configure WiFi first");
    }

    info!("All systems running!");

    // Main loop - keep alive
    loop {
        FreeRtos::delay_ms(1000);
    }
}

fn start_ap_mode(wifi: &mut BlockingWifi<EspWifi<'static>>, ap_password: Option<String>) -> Result<WifiMode> {
    // Generate SSID from MAC address
    let mac = wifi.wifi().sta_netif().get_mac()?;
    let ssid = format!("{}{:02X}{:02X}", AP_SSID_PREFIX, mac[4], mac[5]);
    
    let (auth_method, password_display, password) = match ap_password {
        Some(ref pw) if !pw.is_empty() => (
            AuthMethod::WPA2Personal,
            format!("password: {pw}"),
            pw.as_str(),
        ),
        _ => (AuthMethod::None, "open network".to_string(), ""),
    };
    
    info!("Starting Access Point: {ssid} ({password_display})");

    wifi.set_configuration(&Configuration::AccessPoint(AccessPointConfiguration {
        ssid: ssid.as_str().try_into().unwrap(),
        password: password.try_into().unwrap_or_default(),
        auth_method,
        channel: 1,
        ..Default::default()
    }))?;

    wifi.start()?;

    let ip_info = wifi.wifi().ap_netif().get_ip_info()?;
    info!("AP started - connect to '{}' and navigate to http://{}", ssid, ip_info.ip);

    Ok(WifiMode::AccessPoint)
}

/// Start mixed mode: AP + STA attempting to connect to configured network
fn start_mixed_mode(
    wifi: &mut BlockingWifi<EspWifi<'static>>,
    ap_password: Option<String>,
    sta_ssid: &str,
    sta_password: &str,
) -> Result<WifiMode> {
    // Generate AP SSID from MAC address
    let mac = wifi.wifi().sta_netif().get_mac()?;
    let ap_ssid = format!("{}{:02X}{:02X}", AP_SSID_PREFIX, mac[4], mac[5]);
    
    let (auth_method, password_display, ap_pw) = match ap_password {
        Some(ref pw) if !pw.is_empty() => (
            AuthMethod::WPA2Personal,
            format!("password: {pw}"),
            pw.as_str(),
        ),
        _ => (AuthMethod::None, "open network".to_string(), ""),
    };
    
    info!("Starting Mixed Mode: AP '{ap_ssid}' ({password_display}) + STA connecting to '{sta_ssid}'");

    wifi.set_configuration(&Configuration::Mixed(
        ClientConfiguration {
            ssid: sta_ssid.try_into().unwrap_or_default(),
            password: sta_password.try_into().unwrap_or_default(),
            ..Default::default()
        },
        AccessPointConfiguration {
            ssid: ap_ssid.as_str().try_into().unwrap(),
            password: ap_pw.try_into().unwrap_or_default(),
            auth_method,
            channel: 1,
            ..Default::default()
        },
    ))?;

    wifi.start()?;

    let ap_ip_info = wifi.wifi().ap_netif().get_ip_info()?;
    info!("AP started - connect to '{}' and navigate to http://{}", ap_ssid, ap_ip_info.ip);

    // Try to connect to STA in background (non-blocking for initial startup)
    match wifi.connect() {
        Ok(()) => {
            info!("WiFi STA connected in mixed mode");
            if let Ok(()) = wifi.wait_netif_up() {
                let sta_ip_info = wifi.wifi().sta_netif().get_ip_info()?;
                info!("WiFi STA IP Info: {sta_ip_info:?}");
            }
        }
        Err(e) => {
            warn!("WiFi STA connection failed in mixed mode: {e:?} - AP still running");
        }
    }

    Ok(WifiMode::Mixed)
}
