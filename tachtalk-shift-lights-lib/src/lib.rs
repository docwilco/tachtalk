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
#[derive(Debug, Clone, Default)]
pub struct BlinkState {
    /// Whether LEDs are currently on (true) or off (false) during blink
    pub is_on: bool,
    /// Accumulated time in milliseconds since last toggle
    pub elapsed_ms: u64,
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
/// All matching thresholds (where `rpm >= threshold.rpm`) are applied cumulatively
/// in order. This allows different LED ranges to have different colors at the same
/// RPM. Later thresholds override earlier ones for overlapping LED ranges.
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

    if let Some(highest) = active_threshold {
        threshold_name = Some(highest.name.clone());
        is_blinking = highest.blink;
    }

    // Handle blinking: during blink OFF phase, exclude the highest threshold
    let thresholds_to_apply: &[&ThresholdConfig] = if is_blinking && !blink_state.is_on {
        // During blink off, apply all thresholds except the blinking one
        if matching.len() >= 2 {
            &matching[..matching.len() - 1]
        } else {
            &[]
        }
    } else {
        // Normal case: apply all matching thresholds
        &matching
    };

    // Apply all matching thresholds cumulatively (in order, so later ones override)
    for threshold in thresholds_to_apply {
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
    fn test_higher_threshold_cumulative_with_lower() {
        let thresholds = make_thresholds();
        let blink_state = BlinkState::default();
        let state = compute_led_state(5500, &thresholds, 8, &blink_state);

        assert_eq!(state.active_threshold, Some("Yellow".to_string()));
        
        // LEDs 0-4 should be yellow (Yellow overrides Green for 0-2, adds 3-4)
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

    /// Test cumulative thresholds with non-overlapping LED ranges (progressive shift light)
    #[test]
    fn test_cumulative_non_overlapping_ranges() {
        // Simulate a progressive shift light: blue 0-2, green 3-5, yellow 6-8, red 9-11
        let thresholds = vec![
            ThresholdConfig {
                name: "Off".to_string(),
                rpm: 0,
                start_led: 0,
                end_led: 11,
                color: RGB8::new(0, 0, 0),
                blink: false,
                blink_ms: 500,
            },
            ThresholdConfig {
                name: "Blue".to_string(),
                rpm: 1000,
                start_led: 0,
                end_led: 2,
                color: RGB8::new(0, 0, 255),
                blink: false,
                blink_ms: 500,
            },
            ThresholdConfig {
                name: "Green".to_string(),
                rpm: 1500,
                start_led: 3,
                end_led: 5,
                color: RGB8::new(0, 255, 0),
                blink: false,
                blink_ms: 500,
            },
            ThresholdConfig {
                name: "Yellow".to_string(),
                rpm: 2000,
                start_led: 6,
                end_led: 8,
                color: RGB8::new(255, 255, 0),
                blink: false,
                blink_ms: 500,
            },
            ThresholdConfig {
                name: "Red".to_string(),
                rpm: 2500,
                start_led: 9,
                end_led: 11,
                color: RGB8::new(255, 0, 0),
                blink: false,
                blink_ms: 500,
            },
        ];
        let blink_state = BlinkState::default();

        // At 1200 RPM: only blue should be on
        let state = compute_led_state(1200, &thresholds, 12, &blink_state);
        assert_eq!(state.active_threshold, Some("Blue".to_string()));
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255)); // blue
        assert_eq!(state.leds[2], RGB8::new(0, 0, 255)); // blue
        assert_eq!(state.leds[3], RGB8::default()); // off
        assert_eq!(state.leds[11], RGB8::default()); // off

        // At 1700 RPM: blue AND green should be on
        let state = compute_led_state(1700, &thresholds, 12, &blink_state);
        assert_eq!(state.active_threshold, Some("Green".to_string()));
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255)); // blue stays on
        assert_eq!(state.leds[2], RGB8::new(0, 0, 255)); // blue stays on
        assert_eq!(state.leds[3], RGB8::new(0, 255, 0)); // green
        assert_eq!(state.leds[5], RGB8::new(0, 255, 0)); // green
        assert_eq!(state.leds[6], RGB8::default()); // off

        // At 2200 RPM: blue, green, AND yellow should be on
        let state = compute_led_state(2200, &thresholds, 12, &blink_state);
        assert_eq!(state.active_threshold, Some("Yellow".to_string()));
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255)); // blue
        assert_eq!(state.leds[3], RGB8::new(0, 255, 0)); // green
        assert_eq!(state.leds[6], RGB8::new(255, 255, 0)); // yellow
        assert_eq!(state.leds[8], RGB8::new(255, 255, 0)); // yellow
        assert_eq!(state.leds[9], RGB8::default()); // off

        // At 3000 RPM: all colors should be on
        let state = compute_led_state(3000, &thresholds, 12, &blink_state);
        assert_eq!(state.active_threshold, Some("Red".to_string()));
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255)); // blue
        assert_eq!(state.leds[3], RGB8::new(0, 255, 0)); // green
        assert_eq!(state.leds[6], RGB8::new(255, 255, 0)); // yellow
        assert_eq!(state.leds[9], RGB8::new(255, 0, 0)); // red
        assert_eq!(state.leds[11], RGB8::new(255, 0, 0)); // red
    }

    /// Test progressive single-LED thresholds within a color zone
    #[test]
    fn test_progressive_single_led_thresholds() {
        // Each LED lights up at a different RPM
        let thresholds = vec![
            ThresholdConfig {
                name: "Off".to_string(),
                rpm: 0,
                start_led: 0,
                end_led: 2,
                color: RGB8::new(0, 0, 0),
                blink: false,
                blink_ms: 500,
            },
            ThresholdConfig {
                name: "Blue 1".to_string(),
                rpm: 1000,
                start_led: 0,
                end_led: 0,
                color: RGB8::new(0, 0, 255),
                blink: false,
                blink_ms: 500,
            },
            ThresholdConfig {
                name: "Blue 2".to_string(),
                rpm: 1167,
                start_led: 1,
                end_led: 1,
                color: RGB8::new(0, 0, 255),
                blink: false,
                blink_ms: 500,
            },
            ThresholdConfig {
                name: "Blue 3".to_string(),
                rpm: 1333,
                start_led: 2,
                end_led: 2,
                color: RGB8::new(0, 0, 255),
                blink: false,
                blink_ms: 500,
            },
        ];
        let blink_state = BlinkState::default();

        // At 1100 RPM: only LED 0
        let state = compute_led_state(1100, &thresholds, 3, &blink_state);
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255));
        assert_eq!(state.leds[1], RGB8::default());
        assert_eq!(state.leds[2], RGB8::default());

        // At 1200 RPM: LEDs 0 and 1
        let state = compute_led_state(1200, &thresholds, 3, &blink_state);
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255));
        assert_eq!(state.leds[1], RGB8::new(0, 0, 255));
        assert_eq!(state.leds[2], RGB8::default());

        // At 1400 RPM: all three LEDs
        let state = compute_led_state(1400, &thresholds, 3, &blink_state);
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255));
        assert_eq!(state.leds[1], RGB8::new(0, 0, 255));
        assert_eq!(state.leds[2], RGB8::new(0, 0, 255));
    }

    /// Test blink OFF shows all thresholds underneath (cumulative)
    #[test]
    fn test_blink_off_shows_all_underneath() {
        let thresholds = vec![
            ThresholdConfig {
                name: "Blue".to_string(),
                rpm: 1000,
                start_led: 0,
                end_led: 2,
                color: RGB8::new(0, 0, 255),
                blink: false,
                blink_ms: 500,
            },
            ThresholdConfig {
                name: "Green".to_string(),
                rpm: 1500,
                start_led: 3,
                end_led: 5,
                color: RGB8::new(0, 255, 0),
                blink: false,
                blink_ms: 500,
            },
            ThresholdConfig {
                name: "Shift".to_string(),
                rpm: 2000,
                start_led: 0,
                end_led: 5,
                color: RGB8::new(255, 0, 0),
                blink: true,
                blink_ms: 100,
            },
        ];

        // Blink ON: all red
        let blink_on = BlinkState { is_on: true, elapsed_ms: 0 };
        let state = compute_led_state(2500, &thresholds, 6, &blink_on);
        assert!(state.is_blinking);
        assert_eq!(state.leds[0], RGB8::new(255, 0, 0));
        assert_eq!(state.leds[5], RGB8::new(255, 0, 0));

        // Blink OFF: should show blue AND green underneath
        let blink_off = BlinkState { is_on: false, elapsed_ms: 0 };
        let state = compute_led_state(2500, &thresholds, 6, &blink_off);
        assert!(state.is_blinking);
        assert_eq!(state.active_threshold, Some("Shift".to_string()));
        // Blue LEDs 0-2
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255));
        assert_eq!(state.leds[2], RGB8::new(0, 0, 255));
        // Green LEDs 3-5
        assert_eq!(state.leds[3], RGB8::new(0, 255, 0));
        assert_eq!(state.leds[5], RGB8::new(0, 255, 0));
    }
}
