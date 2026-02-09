//! Physical controls task for brightness and profile switching
//!
//! Handles:
//! - Rotary encoder for brightness adjustment (PCNT hardware)
//! - Push button for cycling through profiles (GPIO interrupt)
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
use std::sync::Arc;
use std::time::{Duration, Instant};

use esp_idf_hal::delay::TickType;
use esp_idf_hal::gpio::{AnyIOPin, Input, InputPin, InterruptType, PinDriver, Pull};
use esp_idf_hal::pcnt::{Pcnt, PcntChannelConfig, PcntControlMode, PcntCountMode, PcntDriver, PinIndex};
use esp_idf_hal::peripheral::Peripheral;
use esp_idf_hal::task::notification::Notification;
use log::{debug, info, warn};

use crate::rpm_leds::RpmTaskMessage;
use crate::watchdog::WatchdogHandle;
use crate::State;

/// Brightness step per encoder detent (encoder click)
const BRIGHTNESS_STEP: i16 = 8;

/// Minimum brightness value
const BRIGHTNESS_MIN: u8 = 0;

/// Maximum brightness value
const BRIGHTNESS_MAX: u8 = 255;

/// How long to wait after last encoder movement before saving to NVS
const NVS_SAVE_DELAY: Duration = Duration::from_millis(1500);

/// Encoder polling interval
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Minimum time between button presses (debounce)
const BUTTON_DEBOUNCE: Duration = Duration::from_millis(200);

/// Initialize the PCNT driver for a rotary encoder.
///
/// The encoder uses quadrature encoding with two signals (A and B):
/// - Channel 0: counts on A edges, uses B as direction control
/// - This gives one count per detent in each direction
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
        Some(pin_a), // pin0 = A (pulse)
        Some(pin_b), // pin1 = B (control)
        None::<esp_idf_hal::gpio::AnyInputPin>, // pin2 unused
        None::<esp_idf_hal::gpio::AnyInputPin>, // pin3 unused
    )?;

    // Configure channel 0 for quadrature decoding:
    // - Count on A edges
    // - Use B level to determine direction
    let channel_config = PcntChannelConfig {
        lctrl_mode: PcntControlMode::Reverse, // B low: reverse counting
        hctrl_mode: PcntControlMode::Keep,    // B high: normal counting
        pos_mode: PcntCountMode::Decrement,   // A rising: decrement (CW = brighter)
        neg_mode: PcntCountMode::Increment,   // A falling: increment
        counter_h_lim: i16::MAX,
        counter_l_lim: i16::MIN,
    };

    driver.channel_config(
        esp_idf_hal::pcnt::PcntChannel::Channel0,
        PinIndex::Pin0, // A = pulse
        PinIndex::Pin1, // B = control
        &channel_config,
    )?;

    // Set glitch filter (in APB clock cycles, 80MHz = 12.5ns per cycle)
    // 1000 cycles = 12.5µs filter, helps with contact bounce
    driver.set_filter_value(1000)?;
    driver.filter_enable()?;

    // Clear and start counter
    driver.counter_clear()?;
    driver.counter_resume()?;

    Ok(driver)
}

/// Initialize the profile button with interrupt support.
///
/// Returns a tuple of (button driver, notification) for handling presses.
/// The button uses internal pull-up and triggers on falling edge (press).
fn init_button(
    button_pin: u8,
) -> Option<(PinDriver<'static, AnyIOPin, Input>, Notification)> {
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
/// - Polls the PCNT counter for rotation (brightness adjustment)
/// - Handles button interrupts for profile switching
/// - Sends updates to the LED task
/// - Debounces NVS writes (waits for controls to settle before saving)
#[allow(clippy::needless_pass_by_value)] // driver is intentionally moved into this task
pub fn controls_task(state: &Arc<State>, driver: PcntDriver<'static>) {
    let watchdog = WatchdogHandle::register(c"controls");
    info!("Controls task started");

    // Load initial brightness from config
    let (mut current_brightness, button_pin): (i16, u8) = {
        let cfg = state.config.lock().unwrap();
        (i16::from(cfg.brightness), cfg.button_pin)
    };
    let mut last_count: i16 = driver.get_counter_value().unwrap_or(0);

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
    // Use checked_sub to avoid potential underflow (though unlikely at boot time)
    let mut last_button_press = Instant::now().checked_sub(BUTTON_DEBOUNCE).unwrap_or_else(Instant::now);

    // Convert poll interval to FreeRTOS ticks
    let poll_ticks = TickType::from(POLL_INTERVAL).ticks();

    loop {
        watchdog.feed();

        // Wait for either poll interval timeout or button notification
        let button_pressed = if let Some((ref mut button, ref notification)) = button_state {
            // Re-register the interrupt callback and enable it
            // SAFETY: The callback only notifies, no unsafe operations
            let waker = notification.notifier();
            let subscribe_ok = unsafe {
                button.subscribe_nonstatic(move || {
                    waker.notify(NonZero::new(1).unwrap());
                })
            }
            .is_ok();

            if subscribe_ok {
                let _ = button.enable_interrupt();
            }

            // Wait with timeout - returns Some if notified before timeout
            notification.wait(poll_ticks).is_some()
        } else {
            std::thread::sleep(POLL_INTERVAL);
            false
        };

        // Handle button press
        if button_pressed && last_button_press.elapsed() >= BUTTON_DEBOUNCE {
            last_button_press = Instant::now();
            handle_button_press(state, &mut last_change, &mut pending_profile_save);
        }

        // Check for encoder movement
        let count = driver.get_counter_value().unwrap_or(last_count);
        let delta = count.wrapping_sub(last_count);

        if delta != 0 {
            last_count = count;

            // Calculate new brightness
            let new_brightness = current_brightness.saturating_add(delta * BRIGHTNESS_STEP);
            let clamped = new_brightness
                .max(i16::from(BRIGHTNESS_MIN))
                .min(i16::from(BRIGHTNESS_MAX));

            if clamped != current_brightness {
                current_brightness = clamped;
                // Safe cast: clamped is in 0..=255 range
                #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                let brightness_u8 = clamped as u8;

                debug!("Encoder: brightness -> {brightness_u8}");

                // Send to LED task for immediate effect
                let _ = state.rpm_tx.send(RpmTaskMessage::Brightness(brightness_u8));

                // Mark that we need to save, but wait for settling
                last_change = Some(Instant::now());
                pending_brightness_save = true;
            }
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
                        debug!("Controls: saving active_profile {} to NVS", cfg_guard.active_profile);
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
