use anyhow::Result;
use esp_idf_hal::delay::FreeRtos;
use esp_idf_hal::gpio::PinDriver;
use esp_idf_hal::prelude::*;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{BlockingWifi, ClientConfiguration, Configuration, EspWifi};
use log::*;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod config;
mod leds;
mod obd2;
mod web_server;

use config::Config;
use leds::LedController;
use obd2::{Obd2Proxy, OBD2_PORT};

const WIFI_SSID: &str = env!("WIFI_SSID");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");

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

    // Initialize configuration
    let config = Arc::new(Mutex::new(Config::load_or_default()));

    // Initialize LED controller
    info!("Initializing LED controller...");
    let led_controller = Arc::new(Mutex::new(LedController::new(
        peripherals.pins.gpio48,
        peripherals.rmt.channel0,
    )?));

    // Initialize WiFi
    info!("Initializing WiFi...");
    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs.clone()))?,
        sys_loop,
    )?;

    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: WIFI_SSID.try_into().unwrap(),
        password: WIFI_PASSWORD.try_into().unwrap(),
        ..Default::default()
    }))?;

    wifi.start()?;
    info!("WiFi started");

    wifi.connect()?;
    info!("WiFi connected");

    wifi.wait_netif_up()?;
    info!("WiFi netif up");

    let ip_info = wifi.wifi().sta_netif().get_ip_info()?;
    info!("WiFi IP Info: {:?}", ip_info);

    // Start web server
    info!("Starting web server...");
    let config_clone = config.clone();
    let led_clone = led_controller.clone();
    std::thread::spawn(move || {
        if let Err(e) = web_server::start_server(config_clone, led_clone) {
            error!("Web server error: {:?}", e);
        }
    });

    // Start OBD2 proxy
    info!("Starting OBD2 proxy...");
    let config_clone = config.clone();
    let led_clone = led_controller.clone();
    let proxy = Obd2Proxy::new(config_clone, led_clone)?;
    
    std::thread::spawn(move || {
        if let Err(e) = proxy.run() {
            error!("OBD2 proxy error: {:?}", e);
        }
    });

    info!("All systems running!");

    // Main loop - keep alive
    loop {
        FreeRtos::delay_ms(1000);
    }
}
