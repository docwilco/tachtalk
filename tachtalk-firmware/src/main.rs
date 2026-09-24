use crate::error::Result;
use esp_idf_hal::delay::FreeRtos;
use esp_idf_hal::gpio::AnyIOPin;
use esp_idf_hal::prelude::*;
use esp_idf_svc::eventloop::EspSystemEventLoop;

/// Firmware variant identifier reported via OTA info. Only "regular"
/// exists since the test firmware's removal; kept so the web UI can spot
/// an old test build still running on a device.
pub const FIRMWARE_VARIANT: &str = "regular";
use esp_idf_svc::mdns::EspMdns;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::{gpio_pull_mode_t_GPIO_PULLUP_ONLY, gpio_pullup_en, gpio_set_pull_mode};
use esp_idf_svc::wifi::EspWifi;
use log::{debug, error, info, warn};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

mod auth;
mod config;
mod controls;
mod cpu_metrics;
mod error;
mod heap_diag;

mod led_display;
mod obd2;
mod ota;
mod sse_server;
mod status_leds;
mod thread_util;
mod watchdog;
mod web_server;
mod wifi;

use config::Config;
use led_display::{led_display_task, LedController, LedTaskSender};
use obd2::{
    cache_manager_task, dongle_task, AtomicDongleTcpState, CacheManagerSender, DongleSender,
    Obd2Proxy,
};
use ota::AtomicOtaState;
use sse_server::{sse_server_task, SseSender};
use status_leds::StatusLedSender;
use thread_util::StackMemory::{Internal, SpiRam};
use wifi::{init_wifi, wifi_connection_manager, AtomicWifiStaState};

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8};

/// Metrics for PID polling
#[derive(Default)]
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
    // --- Capture metrics ---
    /// Number of records captured
    pub records_captured: AtomicU32,
    /// Whether capture buffer has overflowed
    pub capture_overflow: AtomicBool,
}

/// Channel senders bundled for `State` construction.
pub struct TaskChannels {
    pub sse_tx: SseSender,
    pub led_tx: LedTaskSender,
    pub dongle_control_tx: DongleSender,
    pub cache_manager_tx: CacheManagerSender,
    pub status_led_tx: StatusLedSender,
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
    pub led_tx: LedTaskSender,
    pub dongle_control_tx: DongleSender,
    pub cache_manager_tx: CacheManagerSender,
    pub shared_rpm: Mutex<Option<u32>>,
    pub at_command_log: Mutex<HashSet<String>>,
    pub pid_log: Mutex<HashSet<String>>,
    /// Whether we have an active TCP connection to the OBD2 dongle
    pub dongle_tcp_state: AtomicDongleTcpState,
    /// WiFi STA connection state (for status LED and web UI)
    pub wifi_sta_state: AtomicWifiStaState,
    /// Channel sender for the status LED task
    pub status_led_tx: StatusLedSender,
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
    /// OTA download status
    pub ota_status: AtomicOtaState,
    /// OTA progress percentage (0-100)
    pub ota_progress: AtomicU8,
    /// OTA error message (set when `ota_status` == `OtaState::Error`)
    pub ota_error: Mutex<String>,
    /// Traffic capture buffer (in PSRAM)
    pub capture_buffer: Mutex<Vec<u8>>,
    /// Runtime capture toggle (independent of config, toggled via API)
    pub capture_active: AtomicBool,
    /// Session store for web authentication
    pub sessions: auth::SessionStore,
    /// Runtime enabled state for Triggered profiles, indexed parallel to `config.profiles`.
    /// Only entries for Triggered profiles are meaningful; all start `false`.
    pub triggered_enabled: Mutex<Vec<bool>>,
    /// HTTP request counter (incremented by `web_server` on each request)
    pub http_request_count: AtomicU32,
}

impl State {
    /// Notify LED + OBD2 tasks that profile configuration has changed.
    pub fn notify_profile_change(&self) {
        let _ = self.led_tx.send(led_display::LedTaskMessage::ConfigChanged);
        let _ = self
            .cache_manager_tx
            .send(obd2::CacheManagerMessage::ProfilePidsChanged);
    }

    /// Create a new `State` with the given injected dependencies; all other fields
    /// are initialised to their default (zero / empty) values.
    fn new(
        config: Config,
        wifi: EspWifi<'static>,
        ap_ssid: String,
        channels: TaskChannels,
    ) -> Self {
        let capture_enabled = config.obd2.capture_enabled;
        let num_profiles = config.profiles.len();
        Self {
            config: Mutex::new(config),
            wifi: Mutex::new(wifi),
            ap_ssid,
            sse_tx: channels.sse_tx,
            led_tx: channels.led_tx,
            dongle_control_tx: channels.dongle_control_tx,
            cache_manager_tx: channels.cache_manager_tx,
            shared_rpm: Mutex::default(),
            at_command_log: Mutex::default(),
            pid_log: Mutex::default(),
            dongle_tcp_state: AtomicDongleTcpState::default(),
            wifi_sta_state: AtomicWifiStaState::default(),
            status_led_tx: channels.status_led_tx,
            dongle_tcp_info: Mutex::default(),
            obd2_client_count: AtomicU32::new(0),
            client_tcp_info: Mutex::default(),
            polling_metrics: PollingMetrics::default(),
            supported_pids: Mutex::default(),
            ota_status: AtomicOtaState::default(),
            ota_progress: AtomicU8::new(0),
            ota_error: Mutex::default(),
            capture_buffer: Mutex::default(),
            capture_active: AtomicBool::new(capture_enabled),
            sessions: auth::SessionStore::default(),
            triggered_enabled: Mutex::new(vec![false; num_profiles]),
            http_request_count: AtomicU32::new(0),
        }
    }
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

/// Initialize logging and load configuration from NVS
fn init_logging_and_config(nvs: EspDefaultNvsPartition) -> Result<Config> {
    config::init_nvs(nvs)?;
    thread_util::load_stack_overrides();
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

/// Initialize the rev-strip LED controller.
///
/// Must be called from a task pinned to Core 1, not from `main`: the RMT
/// driver's refill interrupt is allocated on the core that installs the
/// driver, and Core 0 services WiFi, whose interrupt load can starve the
/// refill ISR mid-frame and glitch the strip.
fn init_led_controller<C: esp_idf_hal::rmt::RmtChannel>(
    led_gpio: u8,
    brightness: u8,
    rmt_channel: impl esp_idf_hal::peripheral::Peripheral<P = C> + 'static,
) -> Result<LedController> {
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

/// Initialize the rotary encoder if configured (both pins must be non-zero).
///
/// Returns the PCNT driver. The ISR and notification are set up later
/// inside `controls_task` (which runs on its own thread).
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

/// Create shared state, channels, and spawn all background tasks.
#[allow(clippy::too_many_lines)] // linear list of task spawns; splitting adds no clarity
fn spawn_background_tasks(
    config: Config,
    wifi: EspWifi<'static>,
    ap_ssid: String,
    rev_rmt: esp_idf_hal::rmt::CHANNEL0,
    status_rmt: esp_idf_hal::rmt::CHANNEL2,
    encoder_driver: Option<esp_idf_hal::pcnt::PcntDriver<'static>>,
) -> Arc<State> {
    let ap_ip = config.ap_ip;

    // Create all channels
    let (sse_tx, sse_rx) = std::sync::mpsc::channel();
    let (dongle_control_tx, dongle_control_rx) = std::sync::mpsc::channel();
    let (cache_manager_tx, cache_manager_rx) = std::sync::mpsc::channel();
    let (led_tx, led_rx) = std::sync::mpsc::channel();
    let (status_led_tx, status_led_rx) = std::sync::mpsc::channel();

    let state = Arc::new(State::new(
        config,
        wifi,
        ap_ssid,
        TaskChannels {
            sse_tx,
            led_tx,
            dongle_control_tx,
            cache_manager_tx,
            status_led_tx,
        },
    ));

    // Start SSE server for PID value streaming (on port 81)
    {
        let state = state.clone();
        thread_util::spawn_named(
            c"sse_srv",
            thread_util::stack_size(c"sse_srv", 6144),
            SpiRam,
            move || {
                sse_server_task(&sse_rx, &state);
            },
        );
    }

    // Start OBD2 dongle task
    {
        let state = state.clone();
        thread_util::spawn_named(
            c"dongle",
            thread_util::stack_size(c"dongle", 8192),
            Internal,
            move || {
                dongle_task(&state, &dongle_control_rx);
            },
        );
    }

    // Start cache manager task
    {
        let state = state.clone();
        thread_util::spawn_named(
            c"cache_mgr",
            thread_util::stack_size(c"cache_mgr", 6144),
            Internal,
            move || {
                cache_manager_task(&state, &cache_manager_rx);
            },
        );
    }

    // Start the LED display task.
    // Pinned to Core 1, and the RMT driver is constructed inside the task so
    // its refill ISR is allocated there too, away from WiFi on Core 0 (see
    // init_led_controller).
    {
        let state = state.clone();
        thread_util::spawn_named_on_core(
            c"led_disp",
            esp_idf_hal::cpu::Core::Core1,
            thread_util::stack_size(c"led_disp", 4096),
            Internal,
            move || {
                let (led_gpio, brightness) = {
                    let cfg = state.config.lock().unwrap();
                    (cfg.led_gpio, cfg.brightness)
                };
                match init_led_controller(led_gpio, brightness, rev_rmt) {
                    Ok(led_controller) => led_display_task(&state, led_controller, led_rx),
                    // Don't propagate: a bad LED GPIO saved in NVS must not
                    // reboot-loop the device — keep it up so the config can
                    // be fixed via the web UI.
                    Err(e) => error!("LED controller init failed, rev strip disabled: {e:?}"),
                }
            },
        );
    }

    // Start OBD2 proxy
    {
        let state = state.clone();
        thread_util::spawn_named(
            c"obd2_proxy",
            thread_util::stack_size(c"obd2_proxy", 4096),
            Internal,
            move || {
                let proxy = Obd2Proxy::new(state);
                if let Err(e) = proxy.run() {
                    error!("OBD2 proxy error: {e:?}");
                }
            },
        );
    }
    info!("OBD2 proxy started");

    // Start WiFi connection manager
    {
        let state = state.clone();
        thread_util::spawn_named(
            c"wifi_mgr",
            thread_util::stack_size(c"wifi_mgr", 4096),
            SpiRam,
            move || {
                wifi_connection_manager(&state);
            },
        );
    }

    // Start status LED task.
    // Pinned to Core 1 and constructing its RMT driver in-task for the same
    // reason as the LED display task above.
    {
        let state = state.clone();
        thread_util::spawn_named_on_core(
            c"status_led",
            esp_idf_hal::cpu::Core::Core1,
            thread_util::stack_size(c"status_led", 4096),
            Internal,
            move || {
                let (status_gpio, boot_gpio, brightness, flicker_ms) = {
                    let cfg = state.config.lock().unwrap();
                    (
                        cfg.status_led_gpio,
                        cfg.boot_led_gpio,
                        cfg.status_led_brightness,
                        cfg.status_led_flicker_ms,
                    )
                };
                let controller = status_leds::StatusLedController::new(
                    status_gpio,
                    boot_gpio,
                    brightness,
                    flicker_ms,
                    status_rmt,
                );
                status_leds::run_status_led_task(controller, &status_led_rx);
            },
        );
    }

    // Start controls task (rotary encoder and/or buttons)
    {
        let state = state.clone();
        thread_util::spawn_named(
            c"controls",
            thread_util::stack_size(c"controls", 6144),
            Internal,
            move || {
                controls::controls_task(&state, encoder_driver);
            },
        );
    }

    // Register eventfd VFS — required by async-io-mini's reactor for polling.
    // Must be called before the HTTP server thread starts.
    esp_idf_svc::sys::esp!(unsafe {
        esp_idf_svc::sys::esp_vfs_eventfd_register(&esp_idf_svc::sys::esp_vfs_eventfd_config_t {
            max_fds: 5,
        })
    })
    .expect("eventfd VFS registration failed");

    // Start async HTTP server (blocks, so runs on its own thread)
    {
        let state = state.clone();
        thread_util::spawn_named(
            c"http_srv",
            thread_util::stack_size(c"http_srv", 81920),
            SpiRam,
            move || {
                web_server::start_server(&state, ap_ip);
            },
        );
    }

    state
}

fn main() -> Result<()> {
    // It is necessary to call this function once. Otherwise some patches to the runtime
    // implemented by esp-idf-sys might not link properly. See https://github.com/esp-rs/esp-idf-template/issues/71
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    // Mark the running OTA slot as valid so the bootloader won't roll back.
    // Must be called early — if the firmware crashes before this, the bootloader
    // reverts to the previous image on the next reset.
    if let Err(e) = ota::mark_running_slot_valid() {
        warn!("Failed to mark OTA slot valid: {e:?}");
    }

    info!("Starting tachtalk firmware...");

    // Register main task stack size for diagnostic output
    thread_util::register_stack_size(
        c"main",
        esp_idf_svc::sys::CONFIG_ESP_MAIN_TASK_STACK_SIZE as usize,
    );

    info!(
        "LWIP_MAX_SOCKETS: {}",
        esp_idf_svc::sys::CONFIG_LWIP_MAX_SOCKETS
    );

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let config = init_logging_and_config(nvs.clone())?;
    let encoder_driver = init_encoder(&config, peripherals.pcnt0);

    let (wifi, ap_ssid) = init_wifi(&config, peripherals.modem, sys_loop, nvs)?;

    // The LED RMT drivers are constructed inside their Core 1 tasks, not
    // here: the RMT refill ISR lands on the core that installs the driver,
    // and main runs on Core 0 with WiFi. Channel choice is part of the RMT
    // memory partition — see led_display.rs / status_leds.rs.
    let state = spawn_background_tasks(
        config,
        wifi,
        ap_ssid,
        peripherals.rmt.channel0,
        peripherals.rmt.channel2,
        encoder_driver,
    );

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

        // Heap memory stats
        heap_diag::log_heap_stats();

        // HTTP server stats
        info!(
            "HTTP: {} requests total",
            state.http_request_count.load(Ordering::Relaxed),
        );

        // OBD2 client stats
        let dongle_state = match state.dongle_tcp_state.load(Ordering::Relaxed) {
            obd2::DongleTcpState::Disconnected => "disconnected",
            obd2::DongleTcpState::Initializing => "initializing",
            obd2::DongleTcpState::Ready => "ready",
        };
        info!(
            "OBD2: {} | {} req total | {}/sec",
            dongle_state,
            state
                .polling_metrics
                .dongle_requests_total
                .load(Ordering::Relaxed),
            state
                .polling_metrics
                .dongle_requests_per_sec
                .load(Ordering::Relaxed),
        );

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
