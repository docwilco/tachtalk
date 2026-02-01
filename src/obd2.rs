use anyhow::{Result, Context};
use log::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::leds::LedController;

pub const OBD2_PORT: u16 = 35000;
const DONGLE_IP: &str = "192.168.0.10";
const DONGLE_PORT: u16 = 35000;
const IDLE_POLL_INTERVAL_MS: u64 = 100; // 10Hz

pub struct Obd2Proxy {
    config: Arc<Mutex<Config>>,
    led_controller: Arc<Mutex<LedController>>,
    last_request_time: Arc<Mutex<Instant>>,
}

impl Obd2Proxy {
    pub fn new(
        config: Arc<Mutex<Config>>,
        led_controller: Arc<Mutex<LedController>>,
    ) -> Result<Self> {
        Ok(Self {
            config,
            led_controller,
            last_request_time: Arc::new(Mutex::new(Instant::now())),
        })
    }

    pub fn run(self) -> Result<()> {
        // Start background poller thread
        let config_clone = self.config.clone();
        let led_clone = self.led_controller.clone();
        let last_request_clone = self.last_request_time.clone();
        
        std::thread::spawn(move || {
            Self::background_poller(config_clone, led_clone, last_request_clone);
        });

        // Start proxy server
        let listener = TcpListener::bind(format!("0.0.0.0:{}", OBD2_PORT))?;
        info!("OBD2 proxy listening on port {}", OBD2_PORT);

        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let config = self.config.clone();
                    let led_controller = self.led_controller.clone();
                    let last_request = self.last_request_time.clone();
                    
                    std::thread::spawn(move || {
                        if let Err(e) = Self::handle_client(stream, config, led_controller, last_request) {
                            error!("Error handling client: {:?}", e);
                        }
                    });
                }
                Err(e) => {
                    error!("Error accepting connection: {:?}", e);
                }
            }
        }

        Ok(())
    }

    fn background_poller(
        config: Arc<Mutex<Config>>,
        led_controller: Arc<Mutex<LedController>>,
        last_request_time: Arc<Mutex<Instant>>,
    ) {
        loop {
            std::thread::sleep(Duration::from_millis(IDLE_POLL_INTERVAL_MS));

            // Check if we need to poll (no recent requests)
            let should_poll = {
                let last_time = last_request_time.lock().unwrap();
                last_time.elapsed().as_millis() > IDLE_POLL_INTERVAL_MS as u128
            };

            if should_poll {
                // Request RPM from dongle
                if let Ok(rpm) = Self::request_rpm() {
                    if let Ok(mut led) = led_controller.lock() {
                        if let Ok(cfg) = config.lock() {
                            let _ = led.update(rpm, &cfg);
                        }
                    }
                }
            }
        }
    }

    fn handle_client(
        mut client_stream: TcpStream,
        config: Arc<Mutex<Config>>,
        led_controller: Arc<Mutex<LedController>>,
        last_request_time: Arc<Mutex<Instant>>,
    ) -> Result<()> {
        info!("Client connected: {:?}", client_stream.peer_addr()?);

        // Connect to OBD2 dongle
        let mut dongle_stream = TcpStream::connect(format!("{}:{}", DONGLE_IP, DONGLE_PORT))
            .context("Failed to connect to OBD2 dongle")?;
        
        dongle_stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        client_stream.set_read_timeout(Some(Duration::from_secs(5)))?;

        let mut buffer = [0u8; 1024];

        loop {
            // Read from client (Racechrono)
            match client_stream.read(&mut buffer) {
                Ok(0) => {
                    info!("Client disconnected");
                    break;
                }
                Ok(n) => {
                    // Update last request time
                    *last_request_time.lock().unwrap() = Instant::now();

                    let request = &buffer[..n];
                    
                    // Check if this is an RPM request and extract it
                    if let Some(rpm) = Self::extract_rpm_from_request(request) {
                        info!("Extracted RPM from request: {}", rpm);
                        if let Ok(mut led) = led_controller.lock() {
                            if let Ok(cfg) = config.lock() {
                                let _ = led.update(rpm, &cfg);
                            }
                        }
                    }

                    // Forward request to dongle
                    dongle_stream.write_all(request)?;

                    // Read response from dongle
                    match dongle_stream.read(&mut buffer) {
                        Ok(m) => {
                            let response = &buffer[..m];
                            
                            // Extract RPM from response if present
                            if let Some(rpm) = Self::extract_rpm_from_response(response) {
                                info!("Extracted RPM from response: {}", rpm);
                                if let Ok(mut led) = led_controller.lock() {
                                    if let Ok(cfg) = config.lock() {
                                        let _ = led.update(rpm, &cfg);
                                    }
                                }
                            }

                            // Forward response to client
                            client_stream.write_all(response)?;
                        }
                        Err(e) => {
                            error!("Error reading from dongle: {:?}", e);
                            break;
                        }
                    }
                }
                Err(e) => {
                    error!("Error reading from client: {:?}", e);
                    break;
                }
            }
        }

        Ok(())
    }

    fn request_rpm() -> Result<u32> {
        let mut stream = TcpStream::connect(format!("{}:{}", DONGLE_IP, DONGLE_PORT))?;
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;
        
        // OBD2 command for RPM: "010C\r"
        stream.write_all(b"010C\r")?;

        let mut buffer = [0u8; 256];
        let n = stream.read(&mut buffer)?;
        let response = &buffer[..n];

        Self::extract_rpm_from_response(response).ok_or_else(|| anyhow::anyhow!("Failed to extract RPM"))
    }

    fn extract_rpm_from_request(data: &[u8]) -> Option<u32> {
        // Check if this is a response to RPM request being echoed back
        // This is a simplified implementation
        None
    }

    fn extract_rpm_from_response(data: &[u8]) -> Option<u32> {
        // OBD2 response format for RPM (PID 0C): "41 0C XX XX"
        // RPM = ((A * 256) + B) / 4
        let text = std::str::from_utf8(data).ok()?;
        
        // Look for "41 0C" or "410C" pattern
        if let Some(pos) = text.find("410C") {
            let hex_str = &text[pos + 4..].trim();
            let parts: Vec<&str> = hex_str.split_whitespace().collect();
            
            if parts.len() >= 2 {
                let a = u32::from_str_radix(parts[0], 16).ok()?;
                let b = u32::from_str_radix(parts[1], 16).ok()?;
                let rpm = ((a * 256) + b) / 4;
                return Some(rpm);
            }
        }

        None
    }
}
