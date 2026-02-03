use anyhow::Result;
use esp_idf_hal::gpio::OutputPin;
use esp_idf_hal::peripheral::Peripheral;
use esp_idf_hal::rmt::RmtChannel;
use log::debug;
use smart_leds::{SmartLedsWrite, RGB8};
use std::time::Instant;
use tachtalk_shift_lights_lib::{compute_led_state, BlinkState};
use ws2812_esp32_rmt_driver::Ws2812Esp32Rmt;

use crate::config::Config;

pub struct LedController {
    driver: Ws2812Esp32Rmt<'static>,
    last_update_time: Instant,
    blink_state: BlinkState,
}

impl LedController {
    pub fn new<C: RmtChannel, P: OutputPin>(
        pin: impl Peripheral<P = P> + 'static,
        channel: impl Peripheral<P = C> + 'static,
    ) -> Result<Self> {
        debug!("Creating LED controller");
        let driver = Ws2812Esp32Rmt::new(channel, pin)?;

        Ok(Self {
            driver,
            last_update_time: Instant::now(),
            blink_state: BlinkState::default(),
        })
    }

    pub fn update(&mut self, rpm: u32, config: &Config) -> Result<()> {
        // Calculate time delta for blink state updates
        let now = Instant::now();
        let delta_ms = now.duration_since(self.last_update_time).as_millis() as u64;
        self.last_update_time = now;

        // Find blink interval from the active blinking threshold (if any)
        let matching: Vec<_> = config.thresholds.iter().filter(|t| rpm >= t.rpm).collect();
        if let Some(threshold) = matching.last() {
            if threshold.blink {
                let changed = self.blink_state.update(delta_ms, threshold.blink_ms);
                if changed {
                    debug!("LED blink state: {}", self.blink_state.is_on);
                }
            }
        }

        // Compute LED state using the library
        let led_state = compute_led_state(rpm, &config.thresholds, config.total_leds, &self.blink_state);

        if let Some(ref name) = led_state.active_threshold {
            if let Some(threshold) = matching.last() {
                debug!(
                    "LED update: RPM={rpm}, threshold='{}' ({}), leds={}-{}, color=({},{},{}), blink={}",
                    name,
                    threshold.rpm,
                    threshold.start_led,
                    threshold.end_led,
                    threshold.color.r,
                    threshold.color.g,
                    threshold.color.b,
                    threshold.blink
                );
            }
        }

        // Convert Rgb to RGB8 for the driver
        let leds: Vec<RGB8> = led_state
            .leds
            .iter()
            .map(|c| RGB8::new(c.r, c.g, c.b))
            .collect();

        self.write_leds(&leds)?;
        Ok(())
    }

    fn write_leds(&mut self, leds: &[RGB8]) -> Result<()> {
        self.driver.write(leds.iter().copied())?;
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
