//! RPM-based LED controller and task
//!
//! This module handles:
//! - LED hardware control via WS2812 driver
//! - RPM visualization using shift-light patterns
//! - The main RPM/LED update task

use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use esp_idf_hal::gpio::OutputPin;
use esp_idf_hal::peripheral::Peripheral;
use esp_idf_hal::rmt::config::TransmitConfig;
use esp_idf_hal::rmt::{RmtChannel, TxRmtDriver};
use log::{debug, info, warn};
use smart_leds::{brightness, gamma, SmartLedsWrite, RGB8};
use tachtalk_shift_lights_lib::compute_led_state;
use ws2812_esp32_rmt_driver::Ws2812Esp32Rmt;

use crate::config::Config;
use crate::sse_server::SseMessage;
use crate::watchdog::WatchdogHandle;
use crate::State;

/// Default timeout when no blinking is active
const DEFAULT_TIMEOUT_MS: u64 = 100;

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
        // See: https://github.com/cat-in-136/ws2812-esp32-rmt-driver#the-led-is-sp32-flickers-sp32--sp32-s3--sp32-c6--sp32-h2
        let config = TransmitConfig::new().clock_divider(1).mem_block_num(2);
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

    pub fn update(&mut self, rpm: u32, config: &Config, timestamp_ms: u64) -> Result<()> {
        // Compute LED state using the library
        let led_state = compute_led_state(rpm, &config.thresholds, config.total_leds, timestamp_ms);

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
    if let Some(ms) = tachtalk_shift_lights_lib::compute_render_interval(&cfg.thresholds) {
        info!("LED render interval: {ms}ms (blinking active)");
        Some(u64::from(ms))
    } else {
        info!("LED render: event-driven only (no blinking)");
        None
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

    let watchdog = WatchdogHandle::register("rpm_led_task");
    let led_gpio = state.config.lock().unwrap().led_gpio;
    info!("RPM/LED task started (GPIO {led_gpio})");

    let mut current_rpm: Option<u32> = None;
    let mut last_rendered_rpm: Option<u32> = None;
    let mut last_blink_on: Option<bool> = None;

    let mut blink_interval_ms = compute_blink_interval(&state.config.lock().unwrap());

    loop {
        watchdog.feed();

        // Track whether we need to render this iteration
        let mut should_render = false;
        let mut should_render_on_timeout = false;

        // Compute timeout based on blink interval (or default if no blinking)
        let timeout_ms = match blink_interval_ms.map(time_until_next_deadline) {
            Some(blink_ms) => {
                should_render_on_timeout = true;
                blink_ms
            }
            None => DEFAULT_TIMEOUT_MS,
        };
        let timeout = Duration::from_millis(timeout_ms);

        // Wait for message or timeout
        match rpm_rx.recv_timeout(timeout) {
            Ok(RpmTaskMessage::Rpm(rpm)) => {
                if current_rpm != Some(rpm) {
                    current_rpm = Some(rpm);
                    should_render = true; // RPM changed
                }
                // Check for blink phase change (on/off transition)
                if let Some(interval) = blink_interval_ms {
                    let current_on = blink_phase_on(get_wallclock_ms(), interval);
                    if last_blink_on != Some(current_on) {
                        should_render = true;
                    }
                }
                debug!("Received RPM: {rpm}");
            }
            Ok(RpmTaskMessage::ConfigChanged) => {
                blink_interval_ms = compute_blink_interval(&state.config.lock().unwrap());
                last_blink_on = None; // Reset phase tracking on config change
                should_render = true; // Config changed, re-render
            }
            Ok(RpmTaskMessage::Brightness(brightness)) => {
                debug!("Received brightness: {brightness}");
                led_controller.set_brightness(brightness);
                should_render = true; // Re-render with new brightness
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

        // Update LEDs only when needed (RPM changed or blinking)
        if should_render {
            if let Some(rpm) = current_rpm {
                // Only send SSE if RPM actually changed
                if last_rendered_rpm != Some(rpm) {
                    let _ = state.sse_tx.send(SseMessage::RpmUpdate(rpm));
                    last_rendered_rpm = Some(rpm);
                }

                let timestamp_ms = get_wallclock_ms();

                // Update last blink phase after render
                if let Some(interval) = blink_interval_ms {
                    last_blink_on = Some(blink_phase_on(timestamp_ms, interval));
                }

                if let Ok(cfg) = state.config.lock() {
                    let _ = led_controller.update(rpm, &cfg, timestamp_ms);
                }
            }
        }
    }
}
