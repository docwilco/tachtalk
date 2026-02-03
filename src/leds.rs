use anyhow::Result;
use esp_idf_hal::gpio::OutputPin;
use esp_idf_hal::peripheral::Peripheral;
use esp_idf_hal::rmt::RmtChannel;
use log::debug;
use smart_leds::{SmartLedsWrite, RGB8};
use std::time::Instant;
use ws2812_esp32_rmt_driver::Ws2812Esp32Rmt;

use crate::config::Config;

pub struct LedController {
    driver: Ws2812Esp32Rmt<'static>,
    last_blink_time: Instant,
    blink_state: bool,
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
            last_blink_time: Instant::now(),
            blink_state: false,
        })
    }

    pub fn update(&mut self, rpm: u32, config: &Config) -> Result<()> {
        let mut leds = vec![RGB8::default(); config.total_leds];
        
        // Find all matching thresholds (evaluated in order)
        let matching: Vec<_> = config.thresholds.iter()
            .filter(|t| rpm >= t.rpm)
            .collect();

        let active_threshold = matching.last();

        // Apply the active threshold
        if let Some(threshold) = active_threshold {
            // Handle blinking
            if threshold.blink {
                let now = Instant::now();
                if now.duration_since(self.last_blink_time).as_millis() >= u128::from(threshold.blink_ms) {
                    self.blink_state = !self.blink_state;
                    self.last_blink_time = now;
                    debug!("LED blink state: {}", self.blink_state);
                }
                
                if !self.blink_state {
                    // During blink off state, show the threshold underneath (if any)
                    if matching.len() >= 2 {
                        let underneath = matching[matching.len() - 2];
                        let color: RGB8 = underneath.color.clone().into();
                        let start = underneath.start_led.min(config.total_leds);
                        let end = (underneath.end_led + 1).min(config.total_leds);
                        for led in &mut leds[start..end] {
                            *led = color;
                        }
                    }
                    self.write_leds(&leds)?;
                    return Ok(());
                }
            }

            // Light up LEDs for this threshold
            let color: RGB8 = threshold.color.clone().into();
            let start = threshold.start_led.min(config.total_leds);
            let end = (threshold.end_led + 1).min(config.total_leds); // end_led is inclusive
            
            debug!("LED update: RPM={rpm}, threshold='{}' ({}), leds={}-{}, color=({},{},{}), blink={}", 
                   threshold.name, threshold.rpm, start, threshold.end_led, 
                   color.r, color.g, color.b, threshold.blink);
            
            for led in &mut leds[start..end] {
                *led = color;
            }
        }

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
