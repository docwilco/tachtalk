//! Rotary encoder task for brightness control
//!
//! Uses ESP32's PCNT (Pulse Counter) hardware for efficient quadrature decoding.
//! The PCNT peripheral handles the signal processing in hardware, providing
//! inherent debouncing through its glitch filter (12.5µs).
//!
//! ## Hardware wiring
//!
//! Internal pull-ups (~45kΩ) are enabled automatically. For additional hardware
//! debounce in noisy environments (recommended for automotive), add an RC filter:
//!
//! ```text
//! Encoder Pin ──── 10kΩ ──┬── GPIO (with internal pull-up)
//!                         │
//!                       104 (100nF)
//!                         │
//!                        GND
//! ```
//!
//! ## TODO
//!
//! - Push button support for profile selection

use std::sync::Arc;
use std::time::{Duration, Instant};

use esp_idf_hal::gpio::InputPin;
use esp_idf_hal::pcnt::{Pcnt, PcntChannelConfig, PcntControlMode, PcntCountMode, PcntDriver, PinIndex};
use esp_idf_hal::peripheral::Peripheral;
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

/// Run the rotary encoder task.
///
/// This task:
/// - Polls the PCNT counter for rotation
/// - Sends brightness updates to the LED task
/// - Debounces NVS writes (waits for encoder to settle before saving)
#[allow(clippy::needless_pass_by_value)] // driver is intentionally moved into this task
pub fn encoder_task(state: &Arc<State>, driver: PcntDriver<'static>) {
    let watchdog = WatchdogHandle::register("encoder");
    info!("Rotary encoder task started");

    // Load initial brightness from config
    let mut current_brightness: i16 = i16::from(state.config.lock().unwrap().brightness);
    let mut last_count: i16 = driver.get_counter_value().unwrap_or(0);

    // Track when brightness was last changed for NVS debounce
    let mut last_change: Option<Instant> = None;
    let mut pending_save = false;

    loop {
        watchdog.feed();
        std::thread::sleep(POLL_INTERVAL);

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
                pending_save = true;
            }
        }

        // Check if we should save to NVS (debounced)
        if pending_save {
            if let Some(changed_at) = last_change {
                if changed_at.elapsed() >= NVS_SAVE_DELAY {
                    // Safe cast: current_brightness is in 0..=255 range
                    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                    let brightness_u8 = current_brightness as u8;

                    debug!("Encoder: saving brightness {brightness_u8} to NVS");
                    let mut cfg_guard = state.config.lock().unwrap();
                    cfg_guard.brightness = brightness_u8;
                    if let Err(e) = cfg_guard.save() {
                        warn!("Failed to save brightness to NVS: {e}");
                    }

                    pending_save = false;
                    last_change = None;
                }
            }
        }
    }
}
