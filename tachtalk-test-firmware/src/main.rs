use crate::error::Result;
use esp_idf_hal::delay::FreeRtos;
use esp_idf_hal::prelude::*;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use thread_util::StackMemory::{Internal, Spiram};

/// Firmware variant identifier for OTA: "regular" or "test"
pub const FIRMWARE_VARIANT: &str = "test";
use esp_idf_svc::ipv4::Ipv4Addr;
use esp_idf_svc::mdns::EspMdns;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::EspWifi;
use log::{error, info, warn};
use smallvec::SmallVec;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod config;
mod cpu_metrics;
mod dns;
mod error;
mod obd2;
mod ota;
mod sse_server;
pub mod status_leds;
mod thread_util;
mod watchdog;
mod web_server;
mod wifi;

use config::Config;
use obd2::{AtomicDongleTcpState, DongleTcpState};
use ota::AtomicOtaState;
use ota::OtaState;
use sse_server::{sse_server_task, SseSender};
use status_leds::{StatusLedController, StatusLedMessage, StatusLedSender};
use wifi::{init_wifi, wifi_connection_manager, AtomicWifiStaState, WifiStaState};

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

/// Last known value (or error) for a single OBD2 PID.
#[derive(Clone)]
pub enum PidValue {
    /// Raw data bytes (excludes service byte and PID byte).
    Value(SmallVec<[u8; 4]>),
    /// Error string from the last query attempt.
    Error(String),
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
    pub dongle_tcp_state: AtomicDongleTcpState,
    /// WiFi STA connection state (for status LED and web UI)
    pub wifi_sta_state: AtomicWifiStaState,
    /// Channel sender for the status LED task
    pub status_led_tx: StatusLedSender,
    /// Control channel for the test task
    pub test_control_tx: Mutex<Option<std::sync::mpsc::Sender<TestControlMessage>>>,
    /// Mode 5 capture buffer (shared so web server can read/clear it)
    pub capture_buffer: Mutex<Vec<u8>>,
    /// Last PID values (or errors) keyed by Mode 01 PID byte.
    pub pid_values: Mutex<HashMap<u8, PidValue>>,
    /// OTA download status
    pub ota_status: AtomicOtaState,
    /// OTA progress percentage (0-100)
    pub ota_progress: AtomicU8,
    /// OTA error message (set when `ota_status` == `OtaState::Error`)
    pub ota_error: Mutex<String>,
}

impl State {
    /// Create a new `State` with the given injected dependencies; all other fields
    /// are initialised to their default (zero / empty) values.
    fn new(
        config: Config,
        wifi: EspWifi<'static>,
        ap_ssid: String,
        sse_tx: SseSender,
        test_control_tx: std::sync::mpsc::Sender<TestControlMessage>,
        status_led_tx: StatusLedSender,
    ) -> Self {
        Self {
            config: Mutex::new(config),
            wifi: Mutex::new(wifi),
            ap_ssid,
            sse_tx,
            metrics: TestMetrics::default(),
            dongle_tcp_state: AtomicDongleTcpState::new(DongleTcpState::default()),
            wifi_sta_state: AtomicWifiStaState::new(WifiStaState::default()),
            status_led_tx,
            test_control_tx: Mutex::new(Some(test_control_tx)),
            capture_buffer: Mutex::default(),
            pid_values: Mutex::default(),
            ota_status: AtomicOtaState::new(OtaState::default()),
            ota_progress: AtomicU8::new(0),
            ota_error: Mutex::default(),
        }
    }
}

/// Messages to control the test task
pub enum TestControlMessage {
    Start(crate::config::StartOptions),
    Stop,
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

/// Spawn all background tasks
#[allow(clippy::too_many_arguments)]
fn spawn_background_tasks(
    state: &Arc<State>,
    sse_rx: std::sync::mpsc::Receiver<sse_server::SseMessage>,
    test_control_rx: std::sync::mpsc::Receiver<TestControlMessage>,
    status_led_rx: std::sync::mpsc::Receiver<StatusLedMessage>,
    status_led_red_pin: u8,
    status_led_yellow_pin: u8,
    status_led_green_pin: u8,
    status_led_flicker_ms: u16,
    ap_hostname: String,
    ap_ip: Ipv4Addr,
) {
    // Start DNS server for captive portal
    dns::start_dns_server(ap_ip);

    // Start SSE server for metrics streaming (on port 81)
    {
        let state = state.clone();
        thread_util::spawn_named(c"sse_srv", 8192, Spiram, move || {
            sse_server_task(&sse_rx, &state);
        });
    }

    // Start test task (handles modes 1-5)
    {
        let state = state.clone();
        thread_util::spawn_named(c"test_task", 8192, Internal, move || {
            obd2::test_task(&state, &test_control_rx);
        });
    }

    // Start web server
    {
        let state = state.clone();
        thread_util::spawn_named(c"web_srv", 8192, Spiram, move || {
            if let Err(e) = web_server::start_server(&state, Some(&ap_hostname), ap_ip) {
                error!("Web server error: {e:?}");
            }
        });
    }

    // Start WiFi connection manager
    {
        let state = state.clone();
        thread_util::spawn_named(c"wifi_mgr", 8192, Spiram, move || {
            wifi_connection_manager(&state);
        });
    }

    // Start status LED task
    {
        let mut status_led_controller = StatusLedController::new(
            status_led_red_pin,
            status_led_yellow_pin,
            status_led_green_pin,
        );
        let flicker_duration = Duration::from_millis(u64::from(status_led_flicker_ms));
        thread_util::spawn_named(c"status_led", 3072, Internal, move || {
            status_led_controller.boot_animation(Duration::from_millis(250));
            status_leds::status_led_task(status_led_controller, &status_led_rx, flicker_duration);
        });
    }
}

fn log_smallvec_sizes<T: Default + Copy>(type_name: &str, sizes: impl IntoIterator<Item = usize>) {
    let ptr_size = std::mem::size_of::<usize>();
    let elem_size = std::mem::size_of::<T>();
    let elem_align = std::mem::align_of::<T>();
    // SmallVec (union) layout:
    //   struct { len: usize, data: union { inline: [T; N], heap: (*mut T, usize) } }
    //   union_align = max(align_of::<T>(), size_of::<usize>())
    //   union_size = round_up(max(N * elem_size, 2 * ptr_size), union_align)
    //   total = union_align /* len + padding */ + union_size
    let union_align = elem_align.max(ptr_size);
    let heap_size = 2 * ptr_size;
    let mut parts: SmallVec<[String; 16]> = SmallVec::new();
    for n in sizes {
        let actual = match n {
            1 => std::mem::size_of::<SmallVec<[T; 1]>>(),
            2 => std::mem::size_of::<SmallVec<[T; 2]>>(),
            3 => std::mem::size_of::<SmallVec<[T; 3]>>(),
            4 => std::mem::size_of::<SmallVec<[T; 4]>>(),
            5 => std::mem::size_of::<SmallVec<[T; 5]>>(),
            6 => std::mem::size_of::<SmallVec<[T; 6]>>(),
            7 => std::mem::size_of::<SmallVec<[T; 7]>>(),
            8 => std::mem::size_of::<SmallVec<[T; 8]>>(),
            9 => std::mem::size_of::<SmallVec<[T; 9]>>(),
            10 => std::mem::size_of::<SmallVec<[T; 10]>>(),
            11 => std::mem::size_of::<SmallVec<[T; 11]>>(),
            12 => std::mem::size_of::<SmallVec<[T; 12]>>(),
            13 => std::mem::size_of::<SmallVec<[T; 13]>>(),
            14 => std::mem::size_of::<SmallVec<[T; 14]>>(),
            15 => std::mem::size_of::<SmallVec<[T; 15]>>(),
            16 => std::mem::size_of::<SmallVec<[T; 16]>>(),
            17 => std::mem::size_of::<SmallVec<[T; 17]>>(),
            18 => std::mem::size_of::<SmallVec<[T; 18]>>(),
            19 => std::mem::size_of::<SmallVec<[T; 19]>>(),
            20 => std::mem::size_of::<SmallVec<[T; 20]>>(),
            21 => std::mem::size_of::<SmallVec<[T; 21]>>(),
            22 => std::mem::size_of::<SmallVec<[T; 22]>>(),
            23 => std::mem::size_of::<SmallVec<[T; 23]>>(),
            24 => std::mem::size_of::<SmallVec<[T; 24]>>(),
            25 => std::mem::size_of::<SmallVec<[T; 25]>>(),
            26 => std::mem::size_of::<SmallVec<[T; 26]>>(),
            27 => std::mem::size_of::<SmallVec<[T; 27]>>(),
            28 => std::mem::size_of::<SmallVec<[T; 28]>>(),
            29 => std::mem::size_of::<SmallVec<[T; 29]>>(),
            30 => std::mem::size_of::<SmallVec<[T; 30]>>(),
            31 => std::mem::size_of::<SmallVec<[T; 31]>>(),
            32 => std::mem::size_of::<SmallVec<[T; 32]>>(),
            other => {
                warn!("Unsupported SmallVec size {other}, skipping");
                continue;
            }
        };
        let inline_size = n * elem_size;
        let union_size = inline_size.max(heap_size).next_multiple_of(union_align);
        let calculated = union_align + union_size;
        if actual != calculated {
            warn!("SmallVec<[{type_name}; {n}]>: actual={actual}, formula={calculated} — MISMATCH");
        }
        parts.push(format!("{n}={actual}"));
    }
    info!("SmallVec<[{type_name}; N]> sizes: {}", parts.join(", "));
}

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    // Mark the running OTA slot as valid so the bootloader won't roll back.
    // Must be called early — if the firmware crashes before this, the bootloader
    // reverts to the previous image on the next reset.
    if let Err(e) = ota::mark_running_slot_valid() {
        warn!("Failed to mark OTA slot valid: {e:?}");
    }

    info!("Starting tachtalk-test firmware...");

    // Log SmallVec<[u8; N]> sizes for various N to verify inline capacity on this platform
    {
        const SIZES: &[usize] = &[4, 8, 12, 16, 20, 24, 28, 32];
        let mut parts: SmallVec<[String; 8]> = SmallVec::new();
        for &n in SIZES {
            let size = match n {
                4 => std::mem::size_of::<SmallVec<[u8; 4]>>(),
                8 => std::mem::size_of::<SmallVec<[u8; 8]>>(),
                12 => std::mem::size_of::<SmallVec<[u8; 12]>>(),
                16 => std::mem::size_of::<SmallVec<[u8; 16]>>(),
                20 => std::mem::size_of::<SmallVec<[u8; 20]>>(),
                24 => std::mem::size_of::<SmallVec<[u8; 24]>>(),
                28 => std::mem::size_of::<SmallVec<[u8; 28]>>(),
                32 => std::mem::size_of::<SmallVec<[u8; 32]>>(),
                _ => unreachable!(),
            };
            parts.push(format!("{n}={size}"));
        }
        info!("SmallVec<[u8; N]> sizes: {}", parts.join(", "));
    }

    log_smallvec_sizes::<u8>("u8", 1..33);
    log_smallvec_sizes::<u16>("u16", 1..17);
    log_smallvec_sizes::<u32>("u32", 1..9);
    log_smallvec_sizes::<u64>("u64", 1..5);

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
    let (status_led_tx, status_led_rx) = std::sync::mpsc::channel();

    // Read status LED config before moving config into State
    let status_led_red_pin = config.status_led_red_pin;
    let status_led_yellow_pin = config.status_led_yellow_pin;
    let status_led_green_pin = config.status_led_green_pin;
    let status_led_flicker_ms = config.status_led_flicker_ms;

    // Create central State struct
    let state = Arc::new(State::new(
        config,
        wifi,
        ap_ssid,
        sse_tx,
        test_control_tx,
        status_led_tx,
    ));

    spawn_background_tasks(
        &state,
        sse_rx,
        test_control_rx,
        status_led_rx,
        status_led_red_pin,
        status_led_yellow_pin,
        status_led_green_pin,
        status_led_flicker_ms,
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
