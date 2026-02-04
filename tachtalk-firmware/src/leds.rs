use anyhow::Result;
use esp_idf_hal::gpio::OutputPin;
use esp_idf_hal::peripheral::Peripheral;
use esp_idf_hal::rmt::RmtChannel;
use log::debug;
use smart_leds::{SmartLedsWrite, RGB8};
use tachtalk_shift_lights_lib::compute_led_state;
use ws2812_esp32_rmt_driver::Ws2812Esp32Rmt;

use crate::config::Config;

pub struct LedController {
    driver: Ws2812Esp32Rmt<'static>,
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
        })
    }

    pub fn update(&mut self, rpm: u32, config: &Config, timestamp_ms: u64) -> Result<()> {
        // Compute LED state using the library
        let led_state = compute_led_state(rpm, &config.thresholds, config.total_leds, timestamp_ms);

        self.write_leds(&led_state.leds)?;
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
