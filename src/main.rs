use anyhow::Result;
use esp_idf_hal::delay::FreeRtos;
use esp_idf_hal::prelude::*;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{
    AccessPointConfiguration, AuthMethod, BlockingWifi, ClientConfiguration, Configuration,
    EspWifi,
};
use log::{info, warn, error};
use std::sync::{Arc, Mutex};

mod config;
mod leds;
mod obd2;
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
}

fn main() -> Result<()> {
    // It is necessary to call this function once. Otherwise some patches to the runtime
    // implemented by esp-idf-sys might not link properly. See https://github.com/esp-rs/esp-idf-template/issues/71
    esp_idf_svc::sys::link_patches();

    // Bind the log crate to the ESP Logging facilities
    esp_idf_svc::log::EspLogger::initialize_default();

    info!("Starting tachtalk firmware...");

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // Initialize NVS for config storage
    config::init_nvs(nvs.clone())?;

    // Initialize configuration
    let config = Arc::new(Mutex::new(Config::load_or_default()));
    let wifi_mode = Arc::new(Mutex::new(WifiMode::AccessPoint));

    // Initialize LED controller
    info!("Initializing LED controller...");
    let led_controller = Arc::new(Mutex::new(LedController::new(
        peripherals.pins.gpio48,
        peripherals.rmt.channel0,
    )?));

    // Initialize WiFi
    info!("Initializing WiFi...");
    let wifi = EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs.clone()))?;
    let mut wifi = BlockingWifi::wrap(wifi, sys_loop)?;

    // Check if WiFi is configured
    let wifi_configured = {
        let cfg = config.lock().unwrap();
        cfg.wifi.is_configured()
    };

    let current_mode = if wifi_configured {
        // Try to connect to configured network
        let (ssid, password) = {
            let cfg = config.lock().unwrap();
            (cfg.wifi.ssid.clone(), cfg.wifi.password.clone().unwrap_or_default())
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
                    warn!("Failed to get IP: {e:?}, falling back to AP mode");
                    let ap_password = config.lock().unwrap().ap_password.clone();
                    start_ap_mode(&mut wifi, ap_password)?
                } else {
                    let ip_info = wifi.wifi().sta_netif().get_ip_info()?;
                    info!("WiFi IP Info: {ip_info:?}");
                    WifiMode::Client
                }
            }
            Err(e) => {
                warn!("WiFi connection failed: {e:?}, starting AP mode");
                let ap_password = config.lock().unwrap().ap_password.clone();
                start_ap_mode(&mut wifi, ap_password)?
            }
        }
    } else {
        info!("No WiFi configured, starting AP mode");
        let ap_password = config.lock().unwrap().ap_password.clone();
        start_ap_mode(&mut wifi, ap_password)?
    };

    *wifi_mode.lock().unwrap() = current_mode;

    // Start SSE manager for RPM streaming
    let sse_tx = web_server::start_sse_manager();

    // Start web server
    info!("Starting web server...");
    let config_clone = config.clone();
    let led_clone = led_controller.clone();
    let mode_clone = wifi_mode.clone();
    let wifi = Arc::new(Mutex::new(wifi));
    let wifi_clone = wifi.clone();
    let sse_tx_clone = sse_tx.clone();

    std::thread::spawn(move || {
        if let Err(e) = web_server::start_server(config_clone, led_clone, mode_clone, wifi_clone, sse_tx_clone) {
            error!("Web server error: {e:?}");
        }
    });

    // Only start OBD2 proxy in client mode
    if current_mode == WifiMode::Client {
        info!("Starting OBD2 proxy...");
        let config_clone = config.clone();
        let led_clone = led_controller.clone();
        let proxy = Obd2Proxy::new(config_clone, led_clone, sse_tx)?;

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
