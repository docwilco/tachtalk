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

/// Result of computing LED state from RPM
#[derive(Debug, Clone)]
pub struct LedState {
    /// RGB values for each LED
    pub leds: Vec<RGB8>,
    /// Whether any blinking threshold is active
    pub has_blinking: bool,
}

/// Compute whether a blinking threshold is in its "on" phase
#[inline]
fn is_blink_on(timestamp_ms: u64, blink_ms: u32) -> bool {
    // Avoid division by zero
    if blink_ms == 0 {
        return true;
    }
    (timestamp_ms / u64::from(blink_ms)) % 2 == 0
}

/// Compute LED colors based on RPM and threshold configuration
///
/// All matching thresholds (where `rpm >= threshold.rpm`) are applied cumulatively
/// in order. This allows different LED ranges to have different colors at the same
/// RPM. Later thresholds override earlier ones for overlapping LED ranges.
///
/// Blinking thresholds independently compute their on/off state based on the
/// provided timestamp, allowing multiple blinking thresholds with different
/// intervals to coexist.
///
/// # Arguments
/// * `rpm` - Current engine RPM
/// * `thresholds` - List of threshold configurations (evaluated in order)
/// * `total_leds` - Total number of LEDs in the strip
/// * `timestamp_ms` - Current time in milliseconds (for blink calculations)
///
/// # Returns
/// `LedState` containing the RGB values for each LED
#[must_use]
pub fn compute_led_state(
    rpm: u32,
    thresholds: &[ThresholdConfig],
    total_leds: usize,
    timestamp_ms: u64,
) -> LedState {
    let mut leds = vec![RGB8::default(); total_leds];

    // Find all matching thresholds (evaluated in order)
    let matching: Vec<_> = thresholds.iter().filter(|t| rpm >= t.rpm).collect();

    let mut has_blinking = false;

    // Apply all matching thresholds cumulatively (in order, so later ones override)
    for threshold in &matching {
        if threshold.blink {
            has_blinking = true;
            // Skip this threshold during its "off" phase
            if !is_blink_on(timestamp_ms, threshold.blink_ms) {
                continue;
            }
        }

        let color = threshold.color;
        let start = threshold.start_led.min(total_leds);
        let end = (threshold.end_led + 1).min(total_leds); // end_led is inclusive

        for led in &mut leds[start..end] {
            *led = color;
        }
    }

    LedState {
        leds,
        has_blinking,
    }
}

/// Compute the optimal render interval for smooth blinking
///
/// This function finds the GCD of all blink intervals to ensure we hit
/// all blink transitions accurately when rendering at wallclock-aligned times.
///
/// # Arguments
/// * `thresholds` - List of threshold configurations
///
/// # Returns
/// `Some(interval_ms)` if blinking thresholds exist, `None` if no blinking
/// (meaning rendering only needs to happen when RPM changes)
#[must_use]
pub fn compute_render_interval(thresholds: &[ThresholdConfig]) -> Option<u32> {
    use num_integer::Integer;

    // Collect all blink intervals from blinking thresholds
    let blink_intervals: Vec<u32> = thresholds
        .iter()
        .filter(|t| t.blink && t.blink_ms > 0)
        .map(|t| t.blink_ms)
        .collect();

    if blink_intervals.is_empty() {
        return None;
    }

    // Find GCD of all intervals - this ensures we hit all transition points
    let interval_gcd = blink_intervals
        .iter()
        .copied()
        .reduce(|a, b| a.gcd(&b))
        .unwrap_or(500); // Fallback shouldn't happen due to is_empty check

    // Clamp to at least 10ms to avoid burning CPU
    Some(interval_gcd.max(10))
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
        let state = compute_led_state(2000, &thresholds, 8, 0);

        assert!(!state.has_blinking);
        assert!(state.leds.iter().all(|&led| led == RGB8::default()));
    }

    #[test]
    fn test_first_threshold() {
        let thresholds = make_thresholds();
        let state = compute_led_state(3500, &thresholds, 8, 0);

        assert!(!state.has_blinking);
        
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
        let state = compute_led_state(5500, &thresholds, 8, 0);

        // LEDs 0-4 should be yellow (Yellow overrides Green for 0-2, adds 3-4)
        assert_eq!(state.leds[0], RGB8::new(255, 255, 0));
        assert_eq!(state.leds[4], RGB8::new(255, 255, 0));
        // LEDs 5-7 should be off
        assert_eq!(state.leds[5], RGB8::default());
    }

    #[test]
    fn test_blink_on_phase() {
        let thresholds = make_thresholds();
        // timestamp 0 with blink_ms 100: (0 / 100) % 2 == 0 -> ON
        let state = compute_led_state(7500, &thresholds, 8, 0);

        assert!(state.has_blinking);
        
        // During blink ON, all LEDs should be red
        assert_eq!(state.leds[0], RGB8::new(255, 0, 0));
        assert_eq!(state.leds[7], RGB8::new(255, 0, 0));
    }

    #[test]
    fn test_blink_off_shows_underneath() {
        let thresholds = make_thresholds();
        // timestamp 100 with blink_ms 100: (100 / 100) % 2 == 1 -> OFF
        let state = compute_led_state(7500, &thresholds, 8, 100);

        assert!(state.has_blinking);
        
        // During blink OFF, should show Red threshold underneath (LEDs 0-7)
        assert_eq!(state.leds[0], RGB8::new(255, 0, 0));
        assert_eq!(state.leds[7], RGB8::new(255, 0, 0));
    }

    #[test]
    fn test_blink_timing() {
        let thresholds = vec![ThresholdConfig {
            name: "Blink".to_string(),
            rpm: 1000,
            start_led: 0,
            end_led: 0,
            color: RGB8::new(255, 0, 0),
            blink: true,
            blink_ms: 100,
        }];

        // t=0: ON (0/100 % 2 == 0)
        let state = compute_led_state(1500, &thresholds, 1, 0);
        assert_eq!(state.leds[0], RGB8::new(255, 0, 0));

        // t=50: still ON (50/100 % 2 == 0)
        let state = compute_led_state(1500, &thresholds, 1, 50);
        assert_eq!(state.leds[0], RGB8::new(255, 0, 0));

        // t=100: OFF (100/100 % 2 == 1)
        let state = compute_led_state(1500, &thresholds, 1, 100);
        assert_eq!(state.leds[0], RGB8::default());

        // t=150: still OFF (150/100 % 2 == 1)
        let state = compute_led_state(1500, &thresholds, 1, 150);
        assert_eq!(state.leds[0], RGB8::default());

        // t=200: ON again (200/100 % 2 == 0)
        let state = compute_led_state(1500, &thresholds, 1, 200);
        assert_eq!(state.leds[0], RGB8::new(255, 0, 0));
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
        let state = compute_led_state(1500, &thresholds, 8, 0);

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

        // At 1200 RPM: only blue should be on
        let state = compute_led_state(1200, &thresholds, 12, 0);
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255)); // blue
        assert_eq!(state.leds[2], RGB8::new(0, 0, 255)); // blue
        assert_eq!(state.leds[3], RGB8::default()); // off
        assert_eq!(state.leds[11], RGB8::default()); // off

        // At 1700 RPM: blue AND green should be on
        let state = compute_led_state(1700, &thresholds, 12, 0);
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255)); // blue stays on
        assert_eq!(state.leds[2], RGB8::new(0, 0, 255)); // blue stays on
        assert_eq!(state.leds[3], RGB8::new(0, 255, 0)); // green
        assert_eq!(state.leds[5], RGB8::new(0, 255, 0)); // green
        assert_eq!(state.leds[6], RGB8::default()); // off

        // At 2200 RPM: blue, green, AND yellow should be on
        let state = compute_led_state(2200, &thresholds, 12, 0);
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255)); // blue
        assert_eq!(state.leds[3], RGB8::new(0, 255, 0)); // green
        assert_eq!(state.leds[6], RGB8::new(255, 255, 0)); // yellow
        assert_eq!(state.leds[8], RGB8::new(255, 255, 0)); // yellow
        assert_eq!(state.leds[9], RGB8::default()); // off

        // At 3000 RPM: all colors should be on
        let state = compute_led_state(3000, &thresholds, 12, 0);
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

        // At 1100 RPM: only LED 0
        let state = compute_led_state(1100, &thresholds, 3, 0);
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255));
        assert_eq!(state.leds[1], RGB8::default());
        assert_eq!(state.leds[2], RGB8::default());

        // At 1200 RPM: LEDs 0 and 1
        let state = compute_led_state(1200, &thresholds, 3, 0);
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255));
        assert_eq!(state.leds[1], RGB8::new(0, 0, 255));
        assert_eq!(state.leds[2], RGB8::default());

        // At 1400 RPM: all three LEDs
        let state = compute_led_state(1400, &thresholds, 3, 0);
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

        // Blink ON (t=0): all red
        let state = compute_led_state(2500, &thresholds, 6, 0);
        assert!(state.has_blinking);
        assert_eq!(state.leds[0], RGB8::new(255, 0, 0));
        assert_eq!(state.leds[5], RGB8::new(255, 0, 0));

        // Blink OFF (t=100): should show blue AND green underneath
        let state = compute_led_state(2500, &thresholds, 6, 100);
        assert!(state.has_blinking);
        // Blue LEDs 0-2
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255));
        assert_eq!(state.leds[2], RGB8::new(0, 0, 255));
        // Green LEDs 3-5
        assert_eq!(state.leds[3], RGB8::new(0, 255, 0));
        assert_eq!(state.leds[5], RGB8::new(0, 255, 0));
    }

    /// Test that a "Black" threshold at the same RPM as Shift clears LEDs during blink OFF
    #[test]
    fn test_blink_off_with_black_underneath() {
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
                name: "Black".to_string(),
                rpm: 2000,
                start_led: 0,
                end_led: 5,
                color: RGB8::new(0, 0, 0), // All off
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

        // Blink ON (t=0): all red
        let state = compute_led_state(2500, &thresholds, 6, 0);
        assert!(state.has_blinking);
        assert_eq!(state.leds[0], RGB8::new(255, 0, 0));
        assert_eq!(state.leds[5], RGB8::new(255, 0, 0));

        // Blink OFF (t=100): Black threshold should override blue/green, all LEDs off
        let state = compute_led_state(2500, &thresholds, 6, 100);
        assert!(state.has_blinking);
        // All LEDs should be black (off)
        for i in 0..6 {
            assert_eq!(state.leds[i], RGB8::new(0, 0, 0), "LED {i} should be off");
        }
    }

    /// Test multiple independent blinking thresholds
    #[test]
    fn test_multiple_independent_blinks() {
        let thresholds = vec![
            ThresholdConfig {
                name: "Slow Blink".to_string(),
                rpm: 1000,
                start_led: 0,
                end_led: 2,
                color: RGB8::new(0, 0, 255),
                blink: true,
                blink_ms: 200, // 200ms period
            },
            ThresholdConfig {
                name: "Fast Blink".to_string(),
                rpm: 1000,
                start_led: 3,
                end_led: 5,
                color: RGB8::new(255, 0, 0),
                blink: true,
                blink_ms: 100, // 100ms period
            },
        ];

        // t=0: both ON (0/200 % 2 == 0, 0/100 % 2 == 0)
        let state = compute_led_state(1500, &thresholds, 6, 0);
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255)); // slow blink on
        assert_eq!(state.leds[3], RGB8::new(255, 0, 0)); // fast blink on

        // t=100: slow ON, fast OFF (100/200 % 2 == 0, 100/100 % 2 == 1)
        let state = compute_led_state(1500, &thresholds, 6, 100);
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255)); // slow blink still on
        assert_eq!(state.leds[3], RGB8::default()); // fast blink off

        // t=200: slow OFF, fast ON (200/200 % 2 == 1, 200/100 % 2 == 0)
        let state = compute_led_state(1500, &thresholds, 6, 200);
        assert_eq!(state.leds[0], RGB8::default()); // slow blink off
        assert_eq!(state.leds[3], RGB8::new(255, 0, 0)); // fast blink on

        // t=300: slow OFF, fast OFF (300/200 % 2 == 1, 300/100 % 2 == 1)
        let state = compute_led_state(1500, &thresholds, 6, 300);
        assert_eq!(state.leds[0], RGB8::default()); // slow blink off
        assert_eq!(state.leds[3], RGB8::default()); // fast blink off
    }
}
