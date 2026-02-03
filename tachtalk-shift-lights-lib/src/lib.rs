//! Shift light rendering logic for TachTalk
//!
//! This library provides the core logic for determining which LEDs to light
//! based on RPM and threshold configuration. It is hardware-agnostic and
//! can be tested without embedded hardware.

pub use rgb::RGB8;
use serde::{Deserialize, Serialize};

/// Threshold configuration for shift lights
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThresholdConfig {
    pub name: String,
    pub rpm: u32,
    pub start_led: usize,
    pub end_led: usize,
    pub color: RGB8,
    pub blink: bool,
    #[serde(default = "default_blink_ms")]
    pub blink_ms: u32,
}

const fn default_blink_ms() -> u32 {
    500
}

/// Blink state tracker
#[derive(Debug, Clone)]
pub struct BlinkState {
    /// Whether LEDs are currently on (true) or off (false) during blink
    pub is_on: bool,
    /// Accumulated time in milliseconds since last toggle
    pub elapsed_ms: u64,
}

impl Default for BlinkState {
    fn default() -> Self {
        Self {
            is_on: false,
            elapsed_ms: 0,
        }
    }
}

impl BlinkState {
    /// Update blink state with elapsed time since last update
    /// Returns true if state changed
    pub fn update(&mut self, delta_ms: u64, blink_interval_ms: u32) -> bool {
        self.elapsed_ms += delta_ms;
        if self.elapsed_ms >= u64::from(blink_interval_ms) {
            self.is_on = !self.is_on;
            self.elapsed_ms = 0;
            true
        } else {
            false
        }
    }
}

/// Result of computing LED state from RPM
#[derive(Debug, Clone)]
pub struct LedState {
    /// RGB values for each LED
    pub leds: Vec<RGB8>,
    /// Whether we're currently in a blinking threshold
    pub is_blinking: bool,
    /// Name of the active threshold (if any)
    pub active_threshold: Option<String>,
}

/// Compute LED colors based on RPM and threshold configuration
///
/// # Arguments
/// * `rpm` - Current engine RPM
/// * `thresholds` - List of threshold configurations (evaluated in order)
/// * `total_leds` - Total number of LEDs in the strip
/// * `blink_state` - Current blink state (on/off phase)
///
/// # Returns
/// `LedState` containing the RGB values for each LED
#[must_use]
pub fn compute_led_state(
    rpm: u32,
    thresholds: &[ThresholdConfig],
    total_leds: usize,
    blink_state: &BlinkState,
) -> LedState {
    let mut leds = vec![RGB8::default(); total_leds];

    // Find all matching thresholds (evaluated in order)
    let matching: Vec<_> = thresholds.iter().filter(|t| rpm >= t.rpm).collect();

    let active_threshold = matching.last();

    let mut is_blinking = false;
    let mut threshold_name = None;

    // Apply the active threshold
    if let Some(threshold) = active_threshold {
        threshold_name = Some(threshold.name.clone());

        // Handle blinking
        if threshold.blink {
            is_blinking = true;

            if !blink_state.is_on {
                // During blink off state, show the threshold underneath (if any)
                if matching.len() >= 2 {
                    let underneath = matching[matching.len() - 2];
                    let color = underneath.color;
                    let start = underneath.start_led.min(total_leds);
                    let end = (underneath.end_led + 1).min(total_leds);
                    for led in &mut leds[start..end] {
                        *led = color;
                    }
                }
                return LedState {
                    leds,
                    is_blinking,
                    active_threshold: threshold_name,
                };
            }
        }

        // Light up LEDs for this threshold
        let color = threshold.color;
        let start = threshold.start_led.min(total_leds);
        let end = (threshold.end_led + 1).min(total_leds); // end_led is inclusive

        for led in &mut leds[start..end] {
            *led = color;
        }
    }

    LedState {
        leds,
        is_blinking,
        active_threshold: threshold_name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_thresholds() -> Vec<ThresholdConfig> {
        vec![
            ThresholdConfig {
                name: "Green".to_string(),
                rpm: 3000,
                start_led: 0,
                end_led: 2,
                color: RGB8::new(0, 255, 0),
                blink: false,
                blink_ms: 500,
            },
            ThresholdConfig {
                name: "Yellow".to_string(),
                rpm: 5000,
                start_led: 0,
                end_led: 4,
                color: RGB8::new(255, 255, 0),
                blink: false,
                blink_ms: 500,
            },
            ThresholdConfig {
                name: "Red".to_string(),
                rpm: 6500,
                start_led: 0,
                end_led: 7,
                color: RGB8::new(255, 0, 0),
                blink: false,
                blink_ms: 500,
            },
            ThresholdConfig {
                name: "Blink".to_string(),
                rpm: 7000,
                start_led: 0,
                end_led: 7,
                color: RGB8::new(255, 0, 0),
                blink: true,
                blink_ms: 100,
            },
        ]
    }

    #[test]
    fn test_no_threshold_active() {
        let thresholds = make_thresholds();
        let blink_state = BlinkState::default();
        let state = compute_led_state(2000, &thresholds, 8, &blink_state);

        assert!(state.active_threshold.is_none());
        assert!(!state.is_blinking);
        assert!(state.leds.iter().all(|&led| led == RGB8::default()));
    }

    #[test]
    fn test_first_threshold() {
        let thresholds = make_thresholds();
        let blink_state = BlinkState::default();
        let state = compute_led_state(3500, &thresholds, 8, &blink_state);

        assert_eq!(state.active_threshold, Some("Green".to_string()));
        assert!(!state.is_blinking);
        
        // LEDs 0-2 should be green
        assert_eq!(state.leds[0], RGB8::new(0, 255, 0));
        assert_eq!(state.leds[1], RGB8::new(0, 255, 0));
        assert_eq!(state.leds[2], RGB8::new(0, 255, 0));
        // LEDs 3-7 should be off
        assert_eq!(state.leds[3], RGB8::default());
    }

    #[test]
    fn test_higher_threshold_replaces_lower() {
        let thresholds = make_thresholds();
        let blink_state = BlinkState::default();
        let state = compute_led_state(5500, &thresholds, 8, &blink_state);

        assert_eq!(state.active_threshold, Some("Yellow".to_string()));
        
        // LEDs 0-4 should be yellow
        assert_eq!(state.leds[0], RGB8::new(255, 255, 0));
        assert_eq!(state.leds[4], RGB8::new(255, 255, 0));
        // LEDs 5-7 should be off
        assert_eq!(state.leds[5], RGB8::default());
    }

    #[test]
    fn test_blink_on_phase() {
        let thresholds = make_thresholds();
        let blink_state = BlinkState { is_on: true, elapsed_ms: 0 };
        let state = compute_led_state(7500, &thresholds, 8, &blink_state);

        assert_eq!(state.active_threshold, Some("Blink".to_string()));
        assert!(state.is_blinking);
        
        // During blink ON, all LEDs should be red
        assert_eq!(state.leds[0], RGB8::new(255, 0, 0));
        assert_eq!(state.leds[7], RGB8::new(255, 0, 0));
    }

    #[test]
    fn test_blink_off_shows_underneath() {
        let thresholds = make_thresholds();
        let blink_state = BlinkState { is_on: false, elapsed_ms: 0 };
        let state = compute_led_state(7500, &thresholds, 8, &blink_state);

        assert_eq!(state.active_threshold, Some("Blink".to_string()));
        assert!(state.is_blinking);
        
        // During blink OFF, should show Red threshold underneath (LEDs 0-7)
        assert_eq!(state.leds[0], RGB8::new(255, 0, 0));
        assert_eq!(state.leds[7], RGB8::new(255, 0, 0));
    }

    #[test]
    fn test_blink_state_update() {
        let mut blink_state = BlinkState::default();
        assert!(!blink_state.is_on);

        // Not enough time passed
        let changed = blink_state.update(50, 100);
        assert!(!changed);
        assert!(!blink_state.is_on);

        // Enough time passed - should toggle
        let changed = blink_state.update(50, 100);
        assert!(changed);
        assert!(blink_state.is_on);

        // Toggle again
        let changed = blink_state.update(100, 100);
        assert!(changed);
        assert!(!blink_state.is_on);
    }

    #[test]
    fn test_led_bounds_clamping() {
        let thresholds = vec![ThresholdConfig {
            name: "OutOfBounds".to_string(),
            rpm: 1000,
            start_led: 5,
            end_led: 100, // Way beyond total LEDs
            color: RGB8::new(255, 0, 0),
            blink: false,
            blink_ms: 500,
        }];
        let blink_state = BlinkState::default();
        let state = compute_led_state(1500, &thresholds, 8, &blink_state);

        // Should clamp to available LEDs (5-7)
        assert_eq!(state.leds[5], RGB8::new(255, 0, 0));
        assert_eq!(state.leds[7], RGB8::new(255, 0, 0));
        assert_eq!(state.leds.len(), 8);
    }
}
