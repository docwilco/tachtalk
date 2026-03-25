//! Physical controls task for brightness and profile switching
//!
//! Handles:
//! - Rotary encoder for brightness adjustment (PCNT hardware, interrupt-driven)
//! - Push button for cycling through profiles (GPIO interrupt)
//!
//! ## Encoder design
//!
//! The encoder uses 4x quadrature decoding (both channels counting on both
//! edges) with PCNT counter limits set to ±1. When the counter hits +1 or -1,
//! a hardware interrupt fires, an [`AtomicI32`] accumulator is updated, and
//! the controls task is woken via a FreeRTOS notification. This gives one
//! brightness step per quarter-cycle of the quadrature signal, ideal for
//! no-detent encoders.
//!
//! ## Hardware wiring
//!
//! Internal pull-ups (~45kΩ) are enabled automatically. For additional hardware
//! debounce in noisy environments (recommended for automotive), add an RC filter:
//!
//! ```text
//! Encoder/Button Pin ──── 10kΩ ──┬── GPIO (with internal pull-up)
//!                                │
//!                              104 (100nF)
//!                                │
//!                               GND
//! ```
//!
//! The button should connect the GPIO pin to GND when pressed.

use core::num::NonZero;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use esp_idf_hal::delay::TickType;
use esp_idf_hal::gpio::{AnyIOPin, Input, InputPin, InterruptType, PinDriver, Pull};
use esp_idf_hal::pcnt::{
    Pcnt, PcntChannelConfig, PcntControlMode, PcntCountMode, PcntDriver, PcntEvent, PinIndex,
};
use esp_idf_hal::peripheral::Peripheral;
use esp_idf_hal::task::notification::Notification;
use esp_idf_sys::{pcnt_evt_type_t_PCNT_EVT_H_LIM, pcnt_evt_type_t_PCNT_EVT_L_LIM};
use log::{debug, info, warn};

use crate::led_display::LedTaskMessage;
use crate::watchdog::WatchdogHandle;
use crate::State;

/// Minimum brightness value
const BRIGHTNESS_MIN: u8 = 0;

/// Maximum brightness value
const BRIGHTNESS_MAX: u8 = 255;

/// How long to wait after last encoder movement before saving to NVS
const NVS_SAVE_DELAY: Duration = Duration::from_millis(1500);

/// Timeout for the main loop notification wait. Controls how often the
/// watchdog is fed and pending NVS saves are checked when idle.
const IDLE_TIMEOUT: Duration = Duration::from_millis(500);

/// Minimum time between button presses (debounce)
const BUTTON_DEBOUNCE: Duration = Duration::from_millis(200);

/// Accumulated encoder delta, updated from the PCNT ISR.
/// Positive = clockwise (brighter), negative = counter-clockwise (dimmer).
static ENCODER_DELTA: AtomicI32 = AtomicI32::new(0);

/// Set by main button ISR when pressed.
static MAIN_BUTTON_PRESSED: AtomicBool = AtomicBool::new(false);

/// Bitmask of triggered-profile buttons that were pressed since last check.
/// Bit N corresponds to the Nth triggered-button slot (not the profile index).
static TRIGGERED_PRESSED: AtomicU32 = AtomicU32::new(0);

/// Maximum number of triggered-profile buttons (limited by `AtomicU32` bits).
const MAX_TRIGGERED_BUTTONS: usize = 32;

/// Initialize the PCNT driver for a rotary encoder (4x quadrature).
///
/// Configures both PCNT channels for full quadrature decoding (4 counts per
/// cycle). Counter limits are set to +1 / -1 so that every single count fires
/// a limit event. The ISR is registered separately by [`enable_encoder_isr`]
/// once the controls task is running on its own thread.
///
/// # Arguments
/// * `pcnt` - The PCNT peripheral unit to use
/// * `pin_a` - Encoder signal A (CLK)
/// * `pin_b` - Encoder signal B (DT)
pub fn init_encoder<'d, PCNT: Pcnt>(
    pcnt: impl Peripheral<P = PCNT> + 'd,
    pin_a: impl Peripheral<P = impl InputPin> + 'd,
    pin_b: impl Peripheral<P = impl InputPin> + 'd,
) -> Result<PcntDriver<'d>, esp_idf_hal::sys::EspError> {
    // Create PCNT driver with A on pin0, B on pin1
    let mut driver = PcntDriver::new(
        pcnt,
        Some(pin_a),                            // pin0 = A (pulse)
        Some(pin_b),                            // pin1 = B (control)
        None::<esp_idf_hal::gpio::AnyInputPin>, // pin2 unused
        None::<esp_idf_hal::gpio::AnyInputPin>, // pin3 unused
    )?;

    // Channel 0: count on A edges, B determines direction
    let ch0_config = PcntChannelConfig {
        lctrl_mode: PcntControlMode::Reverse, // B low → reverse
        hctrl_mode: PcntControlMode::Keep,    // B high → normal
        pos_mode: PcntCountMode::Decrement,   // A rising: decrement (CW = brighter)
        neg_mode: PcntCountMode::Increment,   // A falling: increment
        counter_h_lim: 1,
        counter_l_lim: -1,
    };
    driver.channel_config(
        esp_idf_hal::pcnt::PcntChannel::Channel0,
        PinIndex::Pin0, // A = pulse
        PinIndex::Pin1, // B = control
        &ch0_config,
    )?;

    // Channel 1: count on B edges, A determines direction (completes 4x decoding)
    let ch1_config = PcntChannelConfig {
        lctrl_mode: PcntControlMode::Keep,    // A low → normal
        hctrl_mode: PcntControlMode::Reverse, // A high → reverse
        pos_mode: PcntCountMode::Decrement,   // B rising: decrement
        neg_mode: PcntCountMode::Increment,   // B falling: increment
        counter_h_lim: 1,
        counter_l_lim: -1,
    };
    driver.channel_config(
        esp_idf_hal::pcnt::PcntChannel::Channel1,
        PinIndex::Pin1, // B = pulse
        PinIndex::Pin0, // A = control
        &ch1_config,
    )?;

    // Set glitch filter (in APB clock cycles, 80MHz = 12.5ns per cycle)
    // 1000 cycles = 12.5µs filter, helps with contact bounce
    driver.set_filter_value(1000)?;
    driver.filter_enable()?;

    // Enable limit events (ISR is registered later by enable_encoder_isr)
    driver.event_enable(PcntEvent::HighLimit)?;
    driver.event_enable(PcntEvent::LowLimit)?;

    // Clear and start counter
    driver.counter_clear()?;
    driver.counter_resume()?;

    Ok(driver)
}

/// Register the PCNT ISR that accumulates direction in [`ENCODER_DELTA`]
/// and wakes the controls task via `notification`.
///
/// Must be called from the controls task thread (the `Notification` is
/// bound to the current FreeRTOS task).
fn enable_encoder_isr(
    driver: &PcntDriver<'static>,
    notification: &Notification,
) -> Result<(), esp_idf_hal::sys::EspError> {
    let waker = notification.notifier();
    // SAFETY: The callback only does atomic ops and a FreeRTOS notify — both
    // ISR-safe. The notifier and atomic are 'static.
    unsafe {
        driver.subscribe(move |status| {
            if status & pcnt_evt_type_t_PCNT_EVT_H_LIM != 0 {
                ENCODER_DELTA.fetch_add(1, Ordering::Relaxed);
            }
            if status & pcnt_evt_type_t_PCNT_EVT_L_LIM != 0 {
                ENCODER_DELTA.fetch_sub(1, Ordering::Relaxed);
            }
            waker.notify(NonZero::new(1).unwrap());
        })?;
    }
    driver.intr_enable()?;
    Ok(())
}

/// Initialize the profile button with interrupt support.
///
/// Returns a tuple of (button driver, notification) for handling presses.
/// The button uses internal pull-up and triggers on falling edge (press).
fn init_button(button_pin: u8) -> Option<(PinDriver<'static, AnyIOPin, Input>, Notification)> {
    // SAFETY: We trust the user-configured GPIO pin number is valid
    let pin = unsafe { AnyIOPin::new(i32::from(button_pin)) };

    let mut button = match PinDriver::input(pin) {
        Ok(b) => b,
        Err(e) => {
            warn!("Failed to initialize button on GPIO {button_pin}: {e:?}");
            return None;
        }
    };

    if let Err(e) = button.set_pull(Pull::Up) {
        warn!("Failed to set button pull-up: {e:?}");
        return None;
    }

    if let Err(e) = button.set_interrupt_type(InterruptType::NegEdge) {
        warn!("Failed to set button interrupt type: {e:?}");
        return None;
    }

    Some((button, Notification::new()))
}

/// A triggered-profile button: tracks which profile it controls.
struct TriggeredButton {
    profile_index: usize,
    bit: u32,
    driver: PinDriver<'static, AnyIOPin, Input>,
}

/// Run the controls task (optional rotary encoder + buttons).
///
/// This task:
/// - Wakes on encoder ISR, button ISR, or triggered button ISR notifications
/// - Reads accumulated encoder delta from [`ENCODER_DELTA`]
/// - Sends brightness updates to the LED task
/// - Toggles triggered profiles via dedicated GPIO buttons
/// - Debounces NVS writes (waits for controls to settle before saving)
#[allow(clippy::needless_pass_by_value)] // drivers are intentionally moved into this task
pub fn controls_task(state: &Arc<State>, encoder: Option<PcntDriver<'static>>) {
    let watchdog = WatchdogHandle::register(c"controls");
    info!("Controls task started");

    // Create notification on this thread — Notification captures the current
    // FreeRTOS task handle and must be waited on from the same task.
    let notification = Notification::new();

    // Register encoder ISR if encoder is present
    if let Some(ref driver) = encoder {
        if let Err(e) = enable_encoder_isr(driver, &notification) {
            warn!("Failed to register encoder ISR: {e:?}");
        }
    }

    // Load initial brightness and acceleration config
    let (mut current_brightness, button_pin, accel_2x, accel_4x): (i16, u8, Duration, Duration) = {
        let cfg = state.config.lock().unwrap();
        (
            i16::from(cfg.brightness),
            cfg.button_pin,
            Duration::from_millis(u64::from(cfg.encoder_accel_2x_ms)),
            Duration::from_millis(u64::from(cfg.encoder_accel_4x_ms)),
        )
    };

    info!(
        "Encoder acceleration: 2x within {}ms, 4x within {}ms",
        accel_2x.as_millis(),
        accel_4x.as_millis()
    );

    // Track last encoder event for acceleration
    let mut last_encoder_event: Option<Instant> = None;

    // Track when config was last changed for NVS debounce
    let mut last_change: Option<Instant> = None;
    let mut pending_brightness_save = false;
    let mut pending_profile_save = false;

    // Initialize button if configured
    let mut button_state = if button_pin != 0 {
        info!("Initializing profile button on GPIO {button_pin}...");
        init_button(button_pin)
    } else {
        debug!("Profile button disabled (pin not configured)");
        None
    };

    // Track button press timing for debounce
    let mut last_button_press = Instant::now()
        .checked_sub(BUTTON_DEBOUNCE)
        .unwrap_or_else(Instant::now);

    // Initialize triggered-profile buttons
    let mut triggered_buttons = init_triggered_buttons(state, &notification);
    let mut last_triggered_press = Instant::now()
        .checked_sub(BUTTON_DEBOUNCE)
        .unwrap_or_else(Instant::now);

    let idle_ticks = TickType::from(IDLE_TIMEOUT).ticks();

    loop {
        watchdog.feed();

        // Re-register button ISR to share the same notification
        if let Some((ref mut button, ref _btn_notification)) = button_state {
            let waker = notification.notifier();
            let subscribe_ok = unsafe {
                button.subscribe_nonstatic(move || {
                    MAIN_BUTTON_PRESSED.store(true, Ordering::Relaxed);
                    waker.notify(NonZero::new(1).unwrap());
                })
            }
            .is_ok();

            if subscribe_ok {
                let _ = button.enable_interrupt();
            }
        }

        // Re-enable triggered button interrupts (GPIO interrupts auto-disable after firing)
        for tb in &mut triggered_buttons {
            let _ = tb.driver.enable_interrupt();
        }

        // Block until notified by encoder ISR, button ISR, or timeout
        notification.wait(idle_ticks);

        // Handle main button press (profile cycling)
        if MAIN_BUTTON_PRESSED.swap(false, Ordering::Relaxed)
            && last_button_press.elapsed() >= BUTTON_DEBOUNCE
        {
            last_button_press = Instant::now();
            handle_button_press(state, &mut last_change, &mut pending_profile_save);
        }

        // Handle triggered-profile button presses
        let pressed = TRIGGERED_PRESSED.swap(0, Ordering::Relaxed);
        if pressed != 0 && last_triggered_press.elapsed() >= BUTTON_DEBOUNCE {
            last_triggered_press = Instant::now();
            for tb in &triggered_buttons {
                if pressed & (1 << tb.bit) != 0 {
                    handle_triggered_button(state, tb.profile_index);
                }
            }
        }

        // Drain accumulated encoder delta and apply brightness
        if let Some(new_brightness) = process_encoder_delta(
            current_brightness,
            &mut last_encoder_event,
            accel_2x,
            accel_4x,
        ) {
            current_brightness = new_brightness;
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            let brightness_u8 = new_brightness as u8;

            info!("Encoder: brightness -> {brightness_u8}");

            let _ = state.led_tx.send(LedTaskMessage::Brightness(brightness_u8));
            last_change = Some(Instant::now());
            pending_brightness_save = true;
        }

        // Check if we should save to NVS (debounced)
        maybe_save_nvs(
            state,
            current_brightness,
            &mut pending_brightness_save,
            &mut pending_profile_save,
            &mut last_change,
        );
    }
}

/// Debounced NVS save: waits for controls to settle before persisting.
fn maybe_save_nvs(
    state: &Arc<State>,
    current_brightness: i16,
    pending_brightness_save: &mut bool,
    pending_profile_save: &mut bool,
    last_change: &mut Option<Instant>,
) {
    if !*pending_brightness_save && !*pending_profile_save {
        return;
    }
    let Some(changed_at) = *last_change else {
        return;
    };
    if changed_at.elapsed() < NVS_SAVE_DELAY {
        return;
    }

    let mut cfg_guard = state.config.lock().unwrap();

    if *pending_brightness_save {
        // Safe cast: current_brightness is in 0..=255 range
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let brightness_u8 = current_brightness as u8;
        debug!("Controls: saving brightness {brightness_u8} to NVS");
        cfg_guard.brightness = brightness_u8;
    }

    if *pending_profile_save {
        debug!(
            "Controls: saving active_profile {} to NVS",
            cfg_guard.active_profile
        );
    }

    if let Err(e) = cfg_guard.save() {
        warn!("Failed to save config to NVS: {e}");
    }

    *pending_brightness_save = false;
    *pending_profile_save = false;
    *last_change = None;
}

/// Process accumulated encoder delta with acceleration.
///
/// Returns `Some(new_brightness)` if brightness changed, `None` otherwise.
fn process_encoder_delta(
    current_brightness: i16,
    last_event: &mut Option<Instant>,
    accel_2x: Duration,
    accel_4x: Duration,
) -> Option<i16> {
    let delta = ENCODER_DELTA.swap(0, Ordering::Relaxed);
    if delta == 0 {
        return None;
    }

    // Determine acceleration multiplier based on time since last event
    let (multiplier, elapsed_ms) = if let Some(prev) = *last_event {
        let elapsed = prev.elapsed();
        let ms = elapsed.as_millis();
        if accel_4x > Duration::ZERO && elapsed <= accel_4x {
            (4, Some(ms))
        } else if accel_2x > Duration::ZERO && elapsed <= accel_2x {
            (2, Some(ms))
        } else {
            (1, Some(ms))
        }
    } else {
        (1, None)
    };
    *last_event = Some(Instant::now());

    let effective_delta = delta.saturating_mul(multiplier);
    if let Some(ms) = elapsed_ms {
        debug!("Encoder: raw delta={delta}, multiplier={multiplier}x, effective={effective_delta}, dt={ms}ms");
    } else {
        debug!("Encoder: raw delta={delta}, multiplier={multiplier}x, effective={effective_delta}, dt=first");
    }

    let new_brightness = current_brightness.saturating_add(
        i16::try_from(effective_delta).unwrap_or(if effective_delta > 0 {
            i16::MAX
        } else {
            i16::MIN
        }),
    );
    let clamped = new_brightness.clamp(i16::from(BRIGHTNESS_MIN), i16::from(BRIGHTNESS_MAX));

    if clamped == current_brightness {
        None
    } else {
        Some(clamped)
    }
}

/// Handle a button press by cycling to the next Normal profile.
fn handle_button_press(
    state: &Arc<State>,
    last_change: &mut Option<Instant>,
    pending_profile_save: &mut bool,
) {
    let mut cfg_guard = state.config.lock().unwrap();

    if cfg_guard.profiles.is_empty() {
        warn!("No profiles configured, button press ignored");
        return;
    }

    let old_profile = cfg_guard.active_profile;
    let new_profile = cfg_guard.cycle_to_next_normal_profile();

    if new_profile == old_profile {
        debug!("No other Normal profiles to cycle to");
        return;
    }

    let new_profile_name = cfg_guard
        .profiles
        .get(new_profile)
        .map_or("Unknown", |p| &p.name);

    info!("Button: switching profile {old_profile} -> {new_profile} ('{new_profile_name}')");

    // Re-bake LED rules for the new profile, then trigger LED preview
    let brightness = cfg_guard.brightness;
    drop(cfg_guard); // Release lock before sending

    state.notify_profile_change();
    let _ = state.led_tx.send(LedTaskMessage::Brightness(brightness));

    // Mark for NVS save
    *last_change = Some(Instant::now());
    *pending_profile_save = true;
}

/// Initialize GPIO buttons for all Triggered profiles that have `button_pin > 0`.
///
/// Each button's ISR sets a bit in [`TRIGGERED_PRESSED`] and wakes the task via
/// the shared `notification`.
fn init_triggered_buttons(state: &Arc<State>, notification: &Notification) -> Vec<TriggeredButton> {
    let cfg = state.config.lock().unwrap();
    let mut buttons = Vec::new();

    for (idx, profile) in cfg.profiles.iter().enumerate() {
        if profile.profile_type != tachtalk_shift_lights_lib::ProfileType::Triggered {
            continue;
        }
        if profile.button_pin == 0 {
            continue;
        }
        if buttons.len() >= MAX_TRIGGERED_BUTTONS {
            warn!(
                "Too many triggered buttons (max {MAX_TRIGGERED_BUTTONS}), skipping '{}'",
                profile.name
            );
            break;
        }

        // Safe: MAX_TRIGGERED_BUTTONS is 32, which fits in u32
        #[allow(clippy::cast_possible_truncation)]
        let bit = buttons.len() as u32;
        let pin_num = profile.button_pin;

        info!(
            "Initializing triggered button for '{}' on GPIO {pin_num} (bit {bit})",
            profile.name
        );

        let pin = unsafe { AnyIOPin::new(i32::from(pin_num)) };
        let mut driver = match PinDriver::input(pin) {
            Ok(d) => d,
            Err(e) => {
                warn!("Failed to init triggered button GPIO {pin_num}: {e:?}");
                continue;
            }
        };
        if driver.set_pull(Pull::Up).is_err()
            || driver.set_interrupt_type(InterruptType::NegEdge).is_err()
        {
            warn!("Failed to configure triggered button GPIO {pin_num}");
            continue;
        }

        // Register ISR that sets the bit and wakes the task
        let waker = notification.notifier();
        let subscribe_ok = unsafe {
            driver.subscribe(move || {
                TRIGGERED_PRESSED.fetch_or(1 << bit, Ordering::Relaxed);
                waker.notify(NonZero::new(1).unwrap());
            })
        }
        .is_ok();

        if subscribe_ok {
            let _ = driver.enable_interrupt();
        } else {
            warn!("Failed to subscribe triggered button GPIO {pin_num}");
            continue;
        }

        buttons.push(TriggeredButton {
            profile_index: idx,
            bit,
            driver,
        });
    }

    if !buttons.is_empty() {
        info!("Initialized {} triggered button(s)", buttons.len());
    }
    buttons
}

/// Toggle a triggered profile's enabled state and notify the LED + OBD2 tasks.
fn handle_triggered_button(state: &Arc<State>, profile_index: usize) {
    let preview = {
        let mut enabled = state.triggered_enabled.lock().unwrap();
        // Grow if needed (config may have changed)
        if enabled.len() <= profile_index {
            enabled.resize(profile_index + 1, false);
        }
        enabled[profile_index] = !enabled[profile_index];
        let now_enabled = enabled[profile_index];

        let cfg = state.config.lock().unwrap();
        let name = cfg.profiles.get(profile_index).map_or("?", |p| &p.name);
        info!(
            "Triggered profile '{}' [{}] {}",
            name,
            profile_index,
            if now_enabled { "ENABLED" } else { "DISABLED" }
        );

        // When enabling, capture triggered profile's preview info
        // When disabling, capture brightness to trigger normal profile preview
        if now_enabled {
            cfg.profiles
                .get(profile_index)
                .map(|p| (Some((p.pid, p.preview_value)), cfg.brightness))
        } else {
            Some((None, cfg.brightness))
        }
    };

    // Notify LED + OBD2 tasks to re-bake (includes/excludes this triggered profile)
    state.notify_profile_change();

    // Show a brief LED preview so the driver sees visual confirmation
    if let Some((triggered_preview, brightness)) = preview {
        // When enabling: inject the triggered profile's PID value for overlay preview
        if let Some((pid, preview_value)) = triggered_preview {
            let _ = state.led_tx.send(LedTaskMessage::PidValue {
                pid,
                value: preview_value,
            });
        }
        // Brightness triggers the preview timer, which shows either the triggered
        // profile's injected value or the normal profile's preview_value
        let _ = state.led_tx.send(LedTaskMessage::Brightness(brightness));
    }
}
