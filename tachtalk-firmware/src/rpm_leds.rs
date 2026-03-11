//! RPM-based LED controller and task
//!
//! This module handles:
//! - LED hardware control via WS2812 driver
//! - RPM visualization using shift-light patterns
//! - The main RPM/LED update task

use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::Result;
use esp_idf_hal::gpio::OutputPin;
use esp_idf_hal::peripheral::Peripheral;
use esp_idf_hal::rmt::config::TransmitConfig;
use esp_idf_hal::rmt::{RmtChannel, TxRmtDriver};
use log::{debug, info, warn};
use smart_leds::{brightness, gamma, SmartLedsWrite, RGB8};
use tachtalk_shift_lights_lib::{bake_led_rules, compute_led_state, BakedLedRules};
use ws2812_esp32_rmt_driver::Ws2812Esp32Rmt;

use crate::config::Config;
use crate::sse_server::SseMessage;
use crate::watchdog::WatchdogHandle;
use crate::State;

/// Default timeout when no blinking is active
const DEFAULT_TIMEOUT_MS: u64 = 100;

/// Duration to show `preview_rpm` after brightness change (ms)
const BRIGHTNESS_PREVIEW_DURATION_MS: u64 = 1000;

/// Get current wallclock time in milliseconds
fn get_wallclock_ms() -> u64 {
    // u64::MAX milliseconds = 584 million years, safe to truncate
    #[allow(clippy::cast_possible_truncation)]
    let ms = esp_idf_svc::systime::EspSystemTime.now().as_millis() as u64;
    ms
}

/// Compute time in ms until the next wallclock-aligned deadline
fn time_until_next_deadline(interval_ms: u64) -> u64 {
    let now_ms = get_wallclock_ms();
    interval_ms - (now_ms % interval_ms)
}

/// Compute whether we're in the on or off blink phase (true = on, false = off)
fn blink_phase_on(timestamp_ms: u64, interval_ms: u64) -> bool {
    (timestamp_ms / interval_ms) % 2 == 0
}

/// Messages sent to the LED task
#[derive(Debug, Clone)]
pub enum RpmTaskMessage {
    /// RPM update from client or poll
    Rpm(u32),
    /// Config changed, recalculate render interval
    ConfigChanged,
    /// Brightness changed (0-255), apply immediately
    Brightness(u8),
}

/// Channel sender for messages to the LED task
pub type RpmTaskSender = Sender<RpmTaskMessage>;

pub struct LedController {
    driver: Ws2812Esp32Rmt<'static>,
    brightness: u8,
}

impl LedController {
    pub fn new<C: RmtChannel, P: OutputPin>(
        pin: impl Peripheral<P = P> + 'static,
        channel: impl Peripheral<P = C> + 'static,
        initial_brightness: u8,
    ) -> Result<Self> {
        debug!("Creating LED controller with brightness {initial_brightness}");
        // Use multiple memory blocks to prevent flicker when WiFi is active.
        // The RMT peripheral can be interrupted by WiFi, causing timing issues.
        // With more memory blocks, the RMT has more buffer to handle interrupts.
        // 12 LEDs × 24 items/LED = 288 items; 4 blocks × 64 items = 256 items (close enough).
        // See: https://github.com/cat-in-136/ws2812-esp32-rmt-driver#the-led-is-sp32-flickers-sp32--sp32-s3--sp32-c6--sp32-h2
        let config = TransmitConfig::new().clock_divider(1).mem_block_num(4);
        let tx_driver = TxRmtDriver::new(channel, pin, &config)?;
        let driver = Ws2812Esp32Rmt::new_with_rmt_driver(tx_driver)?;

        Ok(Self {
            driver,
            brightness: initial_brightness,
        })
    }

    /// Set brightness level (0-255)
    pub fn set_brightness(&mut self, brightness: u8) {
        self.brightness = brightness;
    }

    pub fn update(&mut self, rpm: u32, baked: &BakedLedRules, timestamp_ms: u64) -> Result<()> {
        let led_state = compute_led_state(rpm, baked, timestamp_ms);

        self.write_leds(&led_state.leds)?;
        Ok(())
    }

    fn write_leds(&mut self, leds: &[RGB8]) -> Result<()> {
        // Apply gamma correction first, then brightness reduction
        // as recommended by smart-leds docs
        self.driver
            .write(brightness(gamma(leds.iter().copied()), self.brightness))?;
        Ok(())
    }

    /// Blink the RGB LED purple 3 times (250ms each) as a boot indicator
    pub fn boot_animation(&mut self, total_leds: usize) -> Result<()> {
        use std::thread::sleep;
        use std::time::Duration;

        let purple = RGB8::new(128, 0, 128);
        let off = RGB8::default();
        let blink_duration = Duration::from_millis(250);

        for _ in 0..3 {
            // On
            let leds = vec![purple; total_leds];
            self.write_leds(&leds)?;
            sleep(blink_duration);

            // Off
            let leds = vec![off; total_leds];
            self.write_leds(&leds)?;
            sleep(blink_duration);
        }

        Ok(())
    }
}

/// Compute blink render interval from config (None = no blinking, event-driven only)
fn compute_blink_interval(cfg: &Config) -> Option<u64> {
    if let Some(ms) = tachtalk_shift_lights_lib::compute_render_interval(cfg.active_rules()) {
        info!("LED render interval: {ms}ms (blinking active)");
        Some(u64::from(ms))
    } else {
        info!("LED render: event-driven only (no blinking)");
        None
    }
}

/// Convert the `rpm_stale_timeout_ms` config value to an `Option<Duration>`.
/// Returns `None` when the feature is disabled (value 0).
fn stale_timeout_from_config(cfg: &Config) -> Option<Duration> {
    let ms = cfg.rpm_stale_timeout_ms;
    if ms == 0 {
        None
    } else {
        Some(Duration::from_millis(u64::from(ms)))
    }
}

/// Internal state for the RPM/LED task.
struct RpmLedTaskState {
    current_rpm: Option<u32>,
    last_rendered_rpm: Option<u32>,
    last_blink_on: Option<bool>,
    /// When set, use `preview_rpm` override until this timestamp (ms)
    preview_override_until: Option<u64>,
    /// Track when the last RPM update was received for staleness detection
    last_rpm_time: Option<Instant>,
    blink_interval_ms: Option<u64>,
    baked_rules: BakedLedRules,
    stale_timeout: Option<Duration>,
}

impl RpmLedTaskState {
    fn new(cfg: &Config) -> Self {
        Self {
            current_rpm: None,
            last_rendered_rpm: None,
            last_blink_on: None,
            preview_override_until: None,
            last_rpm_time: None,
            blink_interval_ms: compute_blink_interval(cfg),
            baked_rules: bake_led_rules(cfg.active_rules(), cfg.total_leds),
            stale_timeout: stale_timeout_from_config(cfg),
        }
    }

    /// Reload blink interval, baked rules, and stale timeout from config.
    fn reload_config(&mut self, cfg: &Config) {
        self.blink_interval_ms = compute_blink_interval(cfg);
        self.baked_rules = bake_led_rules(cfg.active_rules(), cfg.total_leds);
        self.stale_timeout = stale_timeout_from_config(cfg);
        self.last_blink_on = None; // Reset phase tracking on config change
    }

    /// Compute the receive timeout and whether to render on timeout.
    fn compute_timeout(&self) -> (Duration, bool) {
        let (timeout_ms, should_render_on_timeout) =
            match self.blink_interval_ms.map(time_until_next_deadline) {
                Some(blink_ms) => (blink_ms, true),
                None => (DEFAULT_TIMEOUT_MS, false),
            };

        // Also consider the RPM stale deadline
        let timeout_ms = if let (Some(stale_dur), Some(rpm_time)) =
            (self.stale_timeout, self.last_rpm_time)
        {
            let stale_deadline = rpm_time + stale_dur;
            let now = Instant::now();
            if now >= stale_deadline {
                0 // Already stale, handle immediately
            } else {
                let ms_until_stale = u64::try_from(stale_deadline.duration_since(now).as_millis())
                    .unwrap_or(u64::MAX);
                timeout_ms.min(ms_until_stale)
            }
        } else {
            timeout_ms
        };

        (Duration::from_millis(timeout_ms), should_render_on_timeout)
    }

    /// Check for RPM staleness; returns true if stale (caller should turn off LEDs).
    fn check_staleness(&mut self) -> bool {
        if let (Some(stale_dur), Some(rpm_time)) = (self.stale_timeout, self.last_rpm_time) {
            if Instant::now() >= rpm_time + stale_dur {
                self.current_rpm = None;
                self.last_rpm_time = None;
                self.last_rendered_rpm = None;
                return true;
            }
        }
        false
    }

    /// Handle an incoming RPM message; returns whether to render.
    fn handle_rpm(&mut self, rpm: u32) -> bool {
        self.last_rpm_time = Some(Instant::now());
        let mut should_render = false;
        if self.current_rpm != Some(rpm) {
            self.current_rpm = Some(rpm);
            should_render = true;
        }
        // Check for blink phase change (on/off transition)
        if let Some(interval) = self.blink_interval_ms {
            let current_on = blink_phase_on(get_wallclock_ms(), interval);
            if self.last_blink_on != Some(current_on) {
                should_render = true;
            }
        }
        debug!("Received RPM: {rpm}");
        should_render
    }

    /// Handle a brightness change; returns whether to render.
    fn handle_brightness(&mut self, led_controller: &mut LedController, brightness: u8) -> bool {
        debug!("Received brightness: {brightness}");
        led_controller.set_brightness(brightness);
        self.preview_override_until = Some(get_wallclock_ms() + BRIGHTNESS_PREVIEW_DURATION_MS);
        true
    }

    /// Determine which RPM to render (preview override or actual).
    fn get_render_rpm(&mut self, state: &State, timestamp_ms: u64) -> Option<u32> {
        if self
            .preview_override_until
            .is_some_and(|until| timestamp_ms < until)
        {
            // Use preview_rpm from active profile during brightness adjustment
            state
                .config
                .lock()
                .ok()
                .and_then(|cfg| cfg.profiles.get(cfg.active_profile).map(|p| p.preview_rpm))
                .or(self.current_rpm)
        } else {
            self.preview_override_until = None; // Clear expired override
            self.current_rpm
        }
    }

    /// Update the last blink phase after rendering.
    fn update_blink_phase(&mut self, timestamp_ms: u64) {
        if let Some(interval) = self.blink_interval_ms {
            self.last_blink_on = Some(blink_phase_on(timestamp_ms, interval));
        }
    }
}

/// Run the LED update task.
///
/// This task:
/// - Receives RPM values from cache manager via channel
/// - Updates LEDs based on current RPM
/// - Sends RPM to SSE clients
///
/// Note: RPM polling is now handled by the cache manager task,
/// which always keeps RPM in the fast polling queue.
// Receiver is intentionally moved into this task for exclusive ownership
#[allow(clippy::needless_pass_by_value)]
pub fn rpm_led_task(
    state: &Arc<State>,
    mut led_controller: LedController,
    rpm_rx: Receiver<RpmTaskMessage>,
) {
    // Boot animation: blink purple 3 times
    {
        let total_leds = state.config.lock().unwrap().total_leds;
        if let Err(e) = led_controller.boot_animation(total_leds) {
            warn!("Boot animation failed: {e}");
        }
    }

    let watchdog = WatchdogHandle::register(c"rpm_led_task");
    let led_gpio = state.config.lock().unwrap().led_gpio;
    info!("RPM/LED task started (GPIO {led_gpio})");

    let mut task_state = RpmLedTaskState::new(&state.config.lock().unwrap());

    loop {
        watchdog.feed();

        let (timeout, should_render_on_timeout) = task_state.compute_timeout();
        let mut should_render = false;

        // Wait for message or timeout
        match rpm_rx.recv_timeout(timeout) {
            Ok(RpmTaskMessage::Rpm(rpm)) => {
                should_render = task_state.handle_rpm(rpm);
            }
            Ok(RpmTaskMessage::ConfigChanged) => {
                task_state.reload_config(&state.config.lock().unwrap());
                should_render = true;
            }
            Ok(RpmTaskMessage::Brightness(brightness)) => {
                should_render = task_state.handle_brightness(&mut led_controller, brightness);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if should_render_on_timeout {
                    should_render = true;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                warn!("RPM channel disconnected, exiting task");
                break;
            }
        }

        // Check for RPM staleness — turn off LEDs if no RPM update within timeout
        if task_state.check_staleness() {
            let total_leds = state.config.lock().unwrap().total_leds;
            let off = vec![RGB8::default(); total_leds];
            let _ = led_controller.write_leds(&off);
            continue;
        }

        // Force render when brightness preview expires (to clear LEDs back to current state)
        if task_state
            .preview_override_until
            .is_some_and(|until| get_wallclock_ms() >= until)
        {
            should_render = true;
        }

        // Update LEDs only when needed (RPM changed or blinking)
        if should_render {
            let timestamp_ms = get_wallclock_ms();
            let render_rpm = task_state.get_render_rpm(state, timestamp_ms);

            if let Some(rpm) = render_rpm {
                // Only send SSE if actual RPM changed (not during preview override)
                if task_state.preview_override_until.is_none()
                    && task_state.last_rendered_rpm != task_state.current_rpm
                {
                    if let Some(actual_rpm) = task_state.current_rpm {
                        let _ = state.sse_tx.send(SseMessage::RpmUpdate(actual_rpm));
                    }
                    task_state.last_rendered_rpm = task_state.current_rpm;
                }

                task_state.update_blink_phase(timestamp_ms);
                let _ = led_controller.update(rpm, &task_state.baked_rules, timestamp_ms);
            } else {
                // No RPM data — ensure LEDs are off
                let total_leds = state.config.lock().unwrap().total_leds;
                let off = vec![RGB8::default(); total_leds];
                let _ = led_controller.write_leds(&off);
            }
        }
    }
}
