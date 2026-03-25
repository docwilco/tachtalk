//! Profile-driven LED display task
//!
//! This module handles:
//! - LED hardware control via WS2812 driver
//! - Multi-PID visualization using shift-light patterns
//! - Overlay stacking (e.g. coolant warning on top of RPM)
//! - The main LED update task

use std::collections::HashMap;
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
use tachtalk_shift_lights_lib::{apply_rules, bake_led_rules, compute_led_state, BakedLedRules};
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
pub enum LedTaskMessage {
    /// Decoded PID value update from cache manager
    PidValue { pid: u8, value: u32 },
    /// Config changed, recalculate render interval
    ConfigChanged,
    /// Brightness changed (0-255), apply immediately
    Brightness(u8),
}

/// Channel sender for messages to the LED task
pub type LedTaskSender = Sender<LedTaskMessage>;

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

/// Compute blink render interval from config as GCD across all active profiles.
/// Returns `None` when no profiles have blinking rules.
fn compute_blink_interval(cfg: &Config, triggered_enabled: &[bool]) -> Option<u64> {
    let mut combined: Option<u64> = None;

    // Active normal profile
    if let Some(profile) = cfg.active_normal_profile() {
        if let Some(ms) = tachtalk_shift_lights_lib::compute_render_interval(&profile.rules) {
            combined = Some(u64::from(ms));
        }
    }

    // All overlay profiles + enabled triggered profiles
    for (_i, profile) in cfg
        .overlay_profiles()
        .chain(cfg.enabled_triggered_profiles(triggered_enabled))
    {
        if let Some(ms) = tachtalk_shift_lights_lib::compute_render_interval(&profile.rules) {
            let ms = u64::from(ms);
            combined = Some(combined.map_or(ms, |prev| gcd(prev, ms)));
        }
    }

    if let Some(ms) = combined {
        info!("LED render interval: {ms}ms (blinking active)");
    } else {
        info!("LED render: event-driven only (no blinking)");
    }
    combined
}

/// Compute GCD of two non-zero values using Euclid's algorithm.
fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
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

/// (`main_profile`, overlays) baked rules ready for rendering.
type BakedProfiles = (Option<(u8, BakedLedRules)>, Vec<(u8, BakedLedRules)>);

/// Internal state for the LED display task.
struct LedDisplayTaskState {
    /// Per-PID decoded values with last-update timestamps
    pid_values: HashMap<u8, (u32, Instant)>,
    /// Last rendered value for the main profile's PID (for SSE dedup)
    last_rendered_main_value: Option<u32>,
    last_blink_on: Option<bool>,
    /// When set, use `preview_value` override until this timestamp (ms)
    preview_override_until: Option<u64>,
    blink_interval_ms: Option<u64>,
    /// Baked rules for active normal profile: (pid, baked)
    main_baked: Option<(u8, BakedLedRules)>,
    /// Baked rules for overlay profiles: [(pid, baked), ...]
    overlay_baked: Vec<(u8, BakedLedRules)>,
    stale_timeout: Option<Duration>,
}

impl LedDisplayTaskState {
    fn new(cfg: &Config, triggered_enabled: &[bool]) -> Self {
        let (main_baked, overlay_baked) = Self::bake_profiles(cfg, triggered_enabled);
        Self {
            pid_values: HashMap::new(),
            last_rendered_main_value: None,
            last_blink_on: None,
            preview_override_until: None,
            blink_interval_ms: compute_blink_interval(cfg, triggered_enabled),
            main_baked,
            overlay_baked,
            stale_timeout: stale_timeout_from_config(cfg),
        }
    }

    /// Bake the active normal profile and all overlay + enabled triggered profiles.
    /// When any triggered profile is enabled, suppress the normal profile so it
    /// doesn't show through underneath the triggered profile's LEDs.
    fn bake_profiles(cfg: &Config, triggered_enabled: &[bool]) -> BakedProfiles {
        let any_triggered = cfg
            .enabled_triggered_profiles(triggered_enabled)
            .next()
            .is_some();

        let main_baked = if any_triggered {
            None
        } else {
            cfg.active_normal_profile()
                .map(|p| (p.pid, bake_led_rules(&p.rules, cfg.total_leds)))
        };

        let overlay_baked: Vec<_> = cfg
            .overlay_profiles()
            .chain(cfg.enabled_triggered_profiles(triggered_enabled))
            .map(|(_i, p)| (p.pid, bake_led_rules(&p.rules, cfg.total_leds)))
            .collect();

        (main_baked, overlay_baked)
    }

    /// Reload blink interval, baked rules, and stale timeout from config.
    fn reload_config(&mut self, cfg: &Config, triggered_enabled: &[bool]) {
        self.blink_interval_ms = compute_blink_interval(cfg, triggered_enabled);
        let (main_baked, overlay_baked) = Self::bake_profiles(cfg, triggered_enabled);
        self.main_baked = main_baked;
        self.overlay_baked = overlay_baked;
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

        // Also consider staleness deadlines for all tracked PIDs
        let timeout_ms = if let Some(stale_dur) = self.stale_timeout {
            let now = Instant::now();
            self.pid_values
                .values()
                .fold(timeout_ms, |acc, &(_value, update_time)| {
                    let stale_deadline = update_time + stale_dur;
                    if now >= stale_deadline {
                        0
                    } else {
                        let ms_until_stale =
                            u64::try_from(stale_deadline.duration_since(now).as_millis())
                                .unwrap_or(u64::MAX);
                        acc.min(ms_until_stale)
                    }
                })
        } else {
            timeout_ms
        };

        (Duration::from_millis(timeout_ms), should_render_on_timeout)
    }

    /// Check for staleness of the main profile's PID; returns true if stale.
    fn check_main_staleness(&mut self) -> bool {
        let Some(stale_dur) = self.stale_timeout else {
            return false;
        };
        let Some((main_pid, _)) = &self.main_baked else {
            return false;
        };
        if let Some(&(_value, update_time)) = self.pid_values.get(main_pid) {
            if Instant::now() >= update_time + stale_dur {
                self.pid_values.remove(main_pid);
                self.last_rendered_main_value = None;
                return true;
            }
        }
        false
    }

    /// Handle an incoming PID value message; returns whether to render.
    fn handle_pid_value(&mut self, pid: u8, value: u32) -> bool {
        let now = Instant::now();
        let mut should_render = false;

        let old_value = self.pid_values.get(&pid).map(|&(v, _)| v);
        self.pid_values.insert(pid, (value, now));

        if old_value != Some(value) {
            should_render = true;
        }

        // Check for blink phase change
        if let Some(interval) = self.blink_interval_ms {
            let current_on = blink_phase_on(get_wallclock_ms(), interval);
            if self.last_blink_on != Some(current_on) {
                should_render = true;
            }
        }
        debug!("Received PID 0x{pid:02X} value: {value}");
        should_render
    }

    /// Handle a brightness change; returns whether to render.
    fn handle_brightness(
        &mut self,
        state: &State,
        led_controller: &mut LedController,
        brightness: u8,
    ) -> bool {
        debug!("Received brightness: {brightness}");
        led_controller.set_brightness(brightness);
        let duration_ms = u64::from(state.config.lock().unwrap().preview_duration_ms);
        self.preview_override_until = Some(get_wallclock_ms() + duration_ms);
        true
    }

    /// Get the value to render for the main profile (preview override or actual PID).
    fn get_main_render_value(&mut self, state: &State, timestamp_ms: u64) -> Option<u32> {
        if self
            .preview_override_until
            .is_some_and(|until| timestamp_ms < until)
        {
            // Use preview_value from active profile during brightness adjustment
            state
                .config
                .lock()
                .ok()
                .and_then(|cfg| cfg.active_normal_profile().map(|p| p.preview_value))
                .or_else(|| {
                    self.main_baked
                        .as_ref()
                        .and_then(|(pid, _)| self.pid_values.get(pid).map(|&(v, _)| v))
                })
        } else {
            self.preview_override_until = None;
            self.main_baked
                .as_ref()
                .and_then(|(pid, _)| self.pid_values.get(pid).map(|&(v, _)| v))
        }
    }

    /// Get a PID value if it's not stale.
    fn get_fresh_pid_value(&self, pid: u8) -> Option<u32> {
        let &(value, update_time) = self.pid_values.get(&pid)?;
        if let Some(stale_dur) = self.stale_timeout {
            if Instant::now() >= update_time + stale_dur {
                return None;
            }
        }
        Some(value)
    }

    /// Update the last blink phase after rendering.
    fn update_blink_phase(&mut self, timestamp_ms: u64) {
        if let Some(interval) = self.blink_interval_ms {
            self.last_blink_on = Some(blink_phase_on(timestamp_ms, interval));
        }
    }

    /// Render LEDs: base layer from main profile + overlay stacking.
    fn render(&mut self, state: &State, led_controller: &mut LedController) {
        let timestamp_ms = get_wallclock_ms();
        let render_value = self.get_main_render_value(state, timestamp_ms);

        if let Some(main_value) = render_value {
            // Send SSE update if actual main PID value changed (not during preview)
            if self.preview_override_until.is_none() {
                let current_main_value = self
                    .main_baked
                    .as_ref()
                    .and_then(|(pid, _)| self.pid_values.get(pid).map(|&(v, _)| v));
                if self.last_rendered_main_value != current_main_value {
                    if let Some(value) = current_main_value {
                        let pid = self.main_baked.as_ref().map_or(0, |(p, _)| *p);
                        let _ = state.sse_tx.send(SseMessage::PidValueUpdate { pid, value });
                    }
                    self.last_rendered_main_value = current_main_value;
                }
            }

            self.update_blink_phase(timestamp_ms);

            // Render main profile as base layer
            if let Some((_pid, ref baked)) = self.main_baked {
                let base = compute_led_state(main_value, baked, timestamp_ms);
                let mut leds = base.leds;
                let mut has_blinking = base.has_blinking;

                // Stack overlays on top
                for &(overlay_pid, ref overlay_baked) in &self.overlay_baked {
                    if let Some(overlay_value) = self.get_fresh_pid_value(overlay_pid) {
                        apply_rules(
                            overlay_value,
                            overlay_baked,
                            timestamp_ms,
                            &mut leds,
                            &mut has_blinking,
                        );
                    }
                }

                let _ = led_controller.write_leds(&leds);
            }
        } else {
            // No main PID data — still render overlays if any have data
            let total_leds = state.config.lock().unwrap().total_leds;
            let has_overlay_data = self
                .overlay_baked
                .iter()
                .any(|(pid, _)| self.get_fresh_pid_value(*pid).is_some());

            if has_overlay_data {
                let mut leds = vec![RGB8::default(); total_leds];
                let mut has_blinking = false;

                for &(overlay_pid, ref overlay_baked) in &self.overlay_baked {
                    if let Some(overlay_value) = self.get_fresh_pid_value(overlay_pid) {
                        apply_rules(
                            overlay_value,
                            overlay_baked,
                            timestamp_ms,
                            &mut leds,
                            &mut has_blinking,
                        );
                    }
                }
                let _ = led_controller.write_leds(&leds);
            } else {
                let off = vec![RGB8::default(); total_leds];
                let _ = led_controller.write_leds(&off);
            }
        }
    }
}

/// Run the LED display task.
///
/// This task:
/// - Receives decoded PID values from cache manager via channel
/// - Renders the active normal profile as a base layer
/// - Stacks overlay profiles on top (e.g. coolant temperature warning)
/// - Sends PID values to SSE clients
// Receiver is intentionally moved into this task for exclusive ownership
#[allow(clippy::needless_pass_by_value)]
pub fn led_display_task(
    state: &Arc<State>,
    mut led_controller: LedController,
    led_rx: Receiver<LedTaskMessage>,
) {
    // Boot animation: blink purple 3 times
    {
        let total_leds = state.config.lock().unwrap().total_leds;
        if let Err(e) = led_controller.boot_animation(total_leds) {
            warn!("Boot animation failed: {e}");
        }
    }

    let watchdog = WatchdogHandle::register(c"led_display");
    let led_gpio = state.config.lock().unwrap().led_gpio;
    info!("LED display task started (GPIO {led_gpio})");

    let mut task_state = {
        let cfg = state.config.lock().unwrap();
        let te = state.triggered_enabled.lock().unwrap();
        LedDisplayTaskState::new(&cfg, &te)
    };

    loop {
        watchdog.feed();

        let (timeout, should_render_on_timeout) = task_state.compute_timeout();
        let mut should_render = false;

        // Wait for message or timeout
        match led_rx.recv_timeout(timeout) {
            Ok(LedTaskMessage::PidValue { pid, value }) => {
                should_render = task_state.handle_pid_value(pid, value);
            }
            Ok(LedTaskMessage::ConfigChanged) => {
                let cfg = state.config.lock().unwrap();
                let te = state.triggered_enabled.lock().unwrap();
                task_state.reload_config(&cfg, &te);
                should_render = true;
            }
            Ok(LedTaskMessage::Brightness(brightness)) => {
                should_render =
                    task_state.handle_brightness(state, &mut led_controller, brightness);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if should_render_on_timeout {
                    should_render = true;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                warn!("LED channel disconnected, exiting task");
                break;
            }
        }

        // Check for main PID staleness — turn off LEDs if no update within timeout
        if task_state.check_main_staleness() {
            let total_leds = state.config.lock().unwrap().total_leds;
            let off = vec![RGB8::default(); total_leds];
            let _ = led_controller.write_leds(&off);
            continue;
        }

        // Force render when brightness preview expires
        if task_state
            .preview_override_until
            .is_some_and(|until| get_wallclock_ms() >= until)
        {
            should_render = true;
        }

        // Render LEDs: base layer from main profile + overlay stacking
        if should_render {
            task_state.render(state, &mut led_controller);
        }
    }
}
