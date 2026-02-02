use anyhow::Result;
use esp_idf_hal::gpio::OutputPin;
use esp_idf_hal::peripheral::Peripheral;
use esp_idf_hal::rmt::config::TransmitConfig;
use esp_idf_hal::rmt::{PinState, Pulse, RmtChannel, TxRmtDriver, VariableLengthSignal};
use smart_leds::RGB8;
use std::time::{Duration, Instant};

use crate::config::Config;

const WS2812_T0H: u32 = 350;  // ns
const WS2812_T0L: u32 = 900;  // ns
const WS2812_T1H: u32 = 900;  // ns
const WS2812_T1L: u32 = 350;  // ns

pub struct LedController {
    driver: TxRmtDriver<'static>,
    _num_leds: usize,
    last_blink_time: Instant,
    blink_state: bool,
}

impl LedController {
    pub fn new<C: RmtChannel, P: OutputPin>(
        pin: impl Peripheral<P = P> + 'static,
        channel: impl Peripheral<P = C> + 'static,
    ) -> Result<Self> {
        let config = TransmitConfig::new().clock_divider(1);
        let driver = TxRmtDriver::new(channel, pin, &config)?;

        Ok(Self {
            driver,
            _num_leds: 8,
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
        // We'll use the ws2812-esp32-rmt-driver crate's functionality
        // For now, simplified implementation without the driver
        // In production, you would use the ws2812_esp32_rmt_driver crate directly
        
        let ticks_hz = self.driver.counter_clock()?;
        let t0h = Pulse::new_with_duration(ticks_hz, PinState::High, &Duration::from_nanos(WS2812_T0H as u64))?;
        let t0l = Pulse::new_with_duration(ticks_hz, PinState::Low, &Duration::from_nanos(WS2812_T0L as u64))?;
        let t1h = Pulse::new_with_duration(ticks_hz, PinState::High, &Duration::from_nanos(WS2812_T1H as u64))?;
        let t1l = Pulse::new_with_duration(ticks_hz, PinState::Low, &Duration::from_nanos(WS2812_T1L as u64))?;
        
        // 24 bits per LED (8 bits each for G, R, B) * 2 pulses per bit + 1 reset pulse
        let mut signal = VariableLengthSignal::new();
        
        for led in leds {
            // WS2812B expects GRB order
            let bytes = [led.g, led.r, led.b];
            for byte in bytes {
                for bit in (0..8).rev() {
                    let bit_set = (byte >> bit) & 1 == 1;
                    if bit_set {
                        signal.push(&[t1h, t1l])?;
                    } else {
                        signal.push(&[t0h, t0l])?;
                    }
                }
            }
        }

        // Add reset signal (low for at least 50us)
        let reset = Pulse::new_with_duration(ticks_hz, PinState::Low, &Duration::from_micros(50))?;
        let zero = Pulse::zero();
        signal.push(&[reset, zero])?;

        self.driver.start_blocking(&signal)?;
        
        Ok(())
    }
}
