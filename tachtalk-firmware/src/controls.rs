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
use std::sync::atomic::{AtomicI32, Ordering};
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

use crate::rpm_leds::RpmTaskMessage;
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

/// Run the controls task (rotary encoder + optional button).
///
/// This task:
/// - Wakes on encoder ISR or button ISR notifications
/// - Reads accumulated encoder delta from [`ENCODER_DELTA`]
/// - Sends brightness updates to the LED task
/// - Debounces NVS writes (waits for controls to settle before saving)
#[allow(clippy::needless_pass_by_value)] // driver is intentionally moved into this task
pub fn controls_task(state: &Arc<State>, driver: PcntDriver<'static>) {
    let watchdog = WatchdogHandle::register(c"controls");
    info!("Controls task started");

    // Create notification on this thread — Notification captures the current
    // FreeRTOS task handle and must be waited on from the same task.
    let notification = Notification::new();

    // Now register the encoder ISR (needs the notification for waking us)
    if let Err(e) = enable_encoder_isr(&driver, &notification) {
        warn!("Failed to register encoder ISR: {e:?}");
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

    let idle_ticks = TickType::from(IDLE_TIMEOUT).ticks();

    loop {
        watchdog.feed();

        // Re-register button ISR to share the same notification
        if let Some((ref mut button, ref _btn_notification)) = button_state {
            let waker = notification.notifier();
            let subscribe_ok = unsafe {
                button.subscribe_nonstatic(move || {
                    waker.notify(NonZero::new(2).unwrap());
                })
            }
            .is_ok();

            if subscribe_ok {
                let _ = button.enable_interrupt();
            }
        }

        // Block until notified by encoder ISR, button ISR, or timeout
        let notified = notification.wait(idle_ticks);

        // Handle button press (notification value 2)
        if notified == Some(NonZero::new(2).unwrap())
            && last_button_press.elapsed() >= BUTTON_DEBOUNCE
        {
            last_button_press = Instant::now();
            handle_button_press(state, &mut last_change, &mut pending_profile_save);
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

            let _ = state.rpm_tx.send(RpmTaskMessage::Brightness(brightness_u8));
            last_change = Some(Instant::now());
            pending_brightness_save = true;
        }

        // Check if we should save to NVS (debounced)
        if pending_brightness_save || pending_profile_save {
            if let Some(changed_at) = last_change {
                if changed_at.elapsed() >= NVS_SAVE_DELAY {
                    let mut cfg_guard = state.config.lock().unwrap();

                    if pending_brightness_save {
                        // Safe cast: current_brightness is in 0..=255 range
                        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                        let brightness_u8 = current_brightness as u8;
                        debug!("Controls: saving brightness {brightness_u8} to NVS");
                        cfg_guard.brightness = brightness_u8;
                    }

                    if pending_profile_save {
                        debug!(
                            "Controls: saving active_profile {} to NVS",
                            cfg_guard.active_profile
                        );
                    }

                    if let Err(e) = cfg_guard.save() {
                        warn!("Failed to save config to NVS: {e}");
                    }

                    pending_brightness_save = false;
                    pending_profile_save = false;
                    last_change = None;
                }
            }
        }
    }
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

/// Handle a button press by cycling to the next profile.
fn handle_button_press(
    state: &Arc<State>,
    last_change: &mut Option<Instant>,
    pending_profile_save: &mut bool,
) {
    let mut cfg_guard = state.config.lock().unwrap();
    let num_profiles = cfg_guard.profiles.len();

    if num_profiles == 0 {
        warn!("No profiles configured, button press ignored");
        return;
    }

    // Cycle to next profile (wrapping)
    let old_profile = cfg_guard.active_profile;
    cfg_guard.active_profile = (old_profile + 1) % num_profiles;

    let new_profile_name = cfg_guard
        .profiles
        .get(cfg_guard.active_profile)
        .map_or("Unknown", |p| &p.name);

    info!(
        "Button: switching profile {} -> {} ('{}')",
        old_profile, cfg_guard.active_profile, new_profile_name
    );

    // Re-bake LED rules for the new profile, then trigger LED preview
    let brightness = cfg_guard.brightness;
    drop(cfg_guard); // Release lock before sending

    let _ = state.rpm_tx.send(RpmTaskMessage::ConfigChanged);
    let _ = state.rpm_tx.send(RpmTaskMessage::Brightness(brightness));

    // Mark for NVS save
    *last_change = Some(Instant::now());
    *pending_profile_save = true;
}
