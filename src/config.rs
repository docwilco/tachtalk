use serde::{Deserialize, Serialize};
use smart_leds::RGB8;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThresholdConfig {
    pub rpm: u32,
    pub color: RGB8Color,
    pub num_leds: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RGB8Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl From<RGB8Color> for RGB8 {
    fn from(c: RGB8Color) -> Self {
        RGB8 { r: c.r, g: c.g, b: c.b }
    }
}

impl From<RGB8> for RGB8Color {
    fn from(c: RGB8) -> Self {
        RGB8Color { r: c.r, g: c.g, b: c.b }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub thresholds: Vec<ThresholdConfig>,
    pub blink_rpm: u32,
    pub total_leds: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            thresholds: vec![
                ThresholdConfig {
                    rpm: 3000,
                    color: RGB8Color { r: 0, g: 255, b: 0 },
                    num_leds: 2,
                },
                ThresholdConfig {
                    rpm: 4000,
                    color: RGB8Color { r: 255, g: 255, b: 0 },
                    num_leds: 4,
                },
                ThresholdConfig {
                    rpm: 5000,
                    color: RGB8Color { r: 255, g: 0, b: 0 },
                    num_leds: 6,
                },
            ],
            blink_rpm: 6000,
            total_leds: 8,
        }
    }
}

impl Config {
    pub fn load_or_default() -> Self {
        // For now, just return default
        // In the future, we could load from NVS
        Self::default()
    }

    pub fn save(&self) -> Result<(), anyhow::Error> {
        // For now, do nothing
        // In the future, we could save to NVS
        Ok(())
    }
}
