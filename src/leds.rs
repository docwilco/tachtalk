use anyhow::Result;
use esp_idf_hal::gpio::OutputPin;
use esp_idf_hal::peripheral::Peripheral;
use esp_idf_hal::rmt::RmtChannel;
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
        let driver = Ws2812Esp32Rmt::new(channel, pin)?;

        Ok(Self {
            driver,
            last_blink_time: Instant::now(),
            blink_state: false,
        })
    }

    pub fn update(&mut self, rpm: u32, config: &Config) -> Result<()> {
        let mut leds = vec![RGB8::default(); config.total_leds];
        
        // Check if we should blink
        let should_blink = rpm >= config.blink_rpm;
        if should_blink {
            let now = Instant::now();
            if now.duration_since(self.last_blink_time).as_millis() >= 250 {
                self.blink_state = !self.blink_state;
                self.last_blink_time = now;
            }
            
            if !self.blink_state {
                // Turn off all LEDs during blink off state
                self.write_leds(&leds)?;
                return Ok(());
            }
        }

        // Find the highest threshold that is met
        let mut active_threshold_idx = None;
        for (idx, threshold) in config.thresholds.iter().enumerate() {
            if rpm >= threshold.rpm {
                active_threshold_idx = Some(idx);
            } else {
                break;
            }
        }

        // Light up LEDs based on active threshold
        if let Some(idx) = active_threshold_idx {
            let threshold = &config.thresholds[idx];
            let color: RGB8 = threshold.color.clone().into();
            for i in 0..threshold.num_leds.min(config.total_leds) {
                leds[i] = color;
            }
        }

        self.write_leds(&leds)?;
        Ok(())
    }

    fn write_leds(&mut self, leds: &[RGB8]) -> Result<()> {
        self.driver.write(leds.iter().copied())?;
        Ok(())
    }
}
