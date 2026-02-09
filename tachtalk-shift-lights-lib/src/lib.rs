//! Shift light rendering logic for TachTalk
//!
//! This library provides the core logic for determining which LEDs to light
//! based on RPM and LED rule configuration. It is hardware-agnostic and
//! can be tested without embedded hardware.

pub use rgb::RGB8;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// Maximum colors stored inline in `SmallVec` (avoids heap allocation for typical use)
const MAX_INLINE_COLORS: usize = 4;

/// LED rule configuration for shift lights (serialized to storage / sent over API).
///
/// When only `rpm_lower` is set, all LEDs in the range light up when RPM exceeds it.
/// When `rpm_upper` is also set, LEDs light up proportionally within the RPM range.
/// `start_led` can be greater than `end_led` for mirror effect (LEDs light from outside in).
///
/// Multiple colors create a gradient across the lit LEDs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedRule {
    pub name: String,
    /// Lower RPM bound (inclusive)
    pub rpm_lower: u32,
    /// Optional upper RPM bound for proportional LED lighting
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm_upper: Option<u32>,
    pub start_led: usize,
    pub end_led: usize,
    /// Colors for the LEDs - multiple colors create a gradient
    #[serde(default = "default_colors")]
    pub colors: SmallVec<[RGB8; MAX_INLINE_COLORS]>,
    pub blink: bool,
    #[serde(default = "default_blink_ms")]
    pub blink_ms: u32,
}

fn default_colors() -> SmallVec<[RGB8; MAX_INLINE_COLORS]> {
    smallvec::smallvec![RGB8::new(255, 0, 0)]
}

const fn default_blink_ms() -> u32 {
    500
}

/// Result of computing LED state from RPM
#[derive(Debug, Clone)]
pub struct LedState {
    /// RGB values for each LED
    pub leds: Vec<RGB8>,
    /// Whether any blinking rule is active
    pub has_blinking: bool,
}

/// A single LED rule with all values needed at render time pre-computed.
///
/// Created by [`bake_led_rules`] at configuration time. The clamped `start`/`end`
/// and `led_count` fields avoid per-frame bounds checking, and `gradient` holds
/// the pre-interpolated colors so the render loop is a simple lookup.
#[derive(Debug, Clone)]
pub struct BakedLedRule {
    rpm_lower: u32,
    rpm_upper: Option<u32>,
    /// First LED index, clamped to `[0, total_leds - 1]`.
    start: usize,
    /// Last LED index, clamped to `[0, total_leds - 1]`.
    end: usize,
    /// Number of LEDs in the range (inclusive), after clamping.
    led_count: usize,
    blink: bool,
    blink_ms: u32,
    /// Pre-computed RGB color for each LED position in the range.
    /// Index 0 corresponds to `start`, index 1 to the next LED toward `end`, etc.
    gradient: SmallVec<[RGB8; 16]>,
}

/// Collection of baked LED rules, ready for per-frame rendering.
///
/// Created via [`bake_led_rules`] when configuration changes, then passed
/// to [`compute_led_state`] on every render frame. Bundles `total_leds`
/// so the render call doesn't need it as a separate argument.
#[derive(Debug, Clone)]
pub struct BakedLedRules {
    rules: Vec<BakedLedRule>,
    total_leds: usize,
}

/// Bake a set of LED rules for fast per-frame rendering.
///
/// Call this once when configuration changes. The returned [`BakedLedRules`]
/// is then passed to [`compute_led_state`] on every render frame.
/// This pre-computes gradient colors, clamps LED indices to `total_leds`,
/// and caches `led_count` so the hot path avoids redundant work.
#[must_use]
pub fn bake_led_rules(rules: &[LedRule], total_leds: usize) -> BakedLedRules {
    let max_led = total_leds.saturating_sub(1);
    let baked = rules
        .iter()
        .map(|r| {
            let start = r.start_led.min(max_led);
            let end = r.end_led.min(max_led);
            let led_count = start.abs_diff(end) + 1;
            let gradient = precompute_gradient_colors(&r.colors, led_count);
            BakedLedRule {
                rpm_lower: r.rpm_lower,
                rpm_upper: r.rpm_upper,
                start,
                end,
                led_count,
                blink: r.blink,
                blink_ms: r.blink_ms,
                gradient,
            }
        })
        .collect();
    BakedLedRules {
        rules: baked,
        total_leds,
    }
}

/// Compute whether a blinking rule is in its "on" phase
#[inline]
fn is_blink_on(timestamp_ms: u64, blink_ms: u32) -> bool {
    // Avoid division by zero
    if blink_ms == 0 {
        return true;
    }
    (timestamp_ms / u64::from(blink_ms)) % 2 == 0
}

/// Compute LED colors from baked rule data.
///
/// This is the per-frame render function. Gradient colors and clamped LED
/// ranges are looked up from the [`BakedLedRules`] built at configuration
/// time by [`bake_led_rules`].
///
/// All matching rules (where `rpm >= rule.rpm_lower`) are applied cumulatively
/// in order. This allows different LED ranges to have different colors at the same
/// RPM. Later rules override earlier ones for overlapping LED ranges.
///
/// When a rule has `rpm_upper` set, LEDs light up proportionally:
/// - At `rpm_lower`: first LED lights up
/// - At `rpm_upper`: all LEDs light up
/// - Values in between light up a proportional number of LEDs
///
/// `start` can be greater than `end` for mirror effect (e.g., LEDs 7→4 lights
/// from outside in on a strip).
///
/// Blinking rules independently compute their on/off state based on the
/// provided timestamp, allowing multiple blinking rules with different
/// intervals to coexist.
///
/// # Arguments
/// * `rpm` - Current engine RPM
/// * `baked` - Baked LED rules from [`bake_led_rules`]
/// * `timestamp_ms` - Current time in milliseconds (for blink calculations)
///
/// # Returns
/// `LedState` containing the RGB values for each LED
#[must_use]
pub fn compute_led_state(
    rpm: u32,
    baked: &BakedLedRules,
    timestamp_ms: u64,
) -> LedState {
    let mut leds = vec![RGB8::default(); baked.total_leds];
    let mut has_blinking = false;

    // Apply all matching rules cumulatively (in order, so later ones override)
    for rule in &baked.rules {
        if rpm < rule.rpm_lower {
            continue;
        }

        if rule.blink {
            has_blinking = true;
            // Skip this rule during its "off" phase
            if !is_blink_on(timestamp_ms, rule.blink_ms) {
                continue;
            }
        }

        let leds_to_light = compute_leds_to_light(rpm, rule);

        for (i, &led_idx) in leds_to_light.iter().enumerate() {
            if led_idx < baked.total_leds {
                // Look up pre-computed gradient color; fall back to black if index
                // is out of range (e.g., clamped LED range vs unclamped gradient)
                leds[led_idx] = rule.gradient.get(i).copied().unwrap_or_default();
            }
        }
    }

    LedState {
        leds,
        has_blinking,
    }
}

/// Pre-compute the gradient color for every LED position in a range.
///
/// Given a list of gradient color stops and the total number of LED positions,
/// returns a [`SmallVec`] where entry `i` is the interpolated color at position `i`.
///
/// The gradient is divided into `colors.len() - 1` equal segments, each
/// linearly interpolating between two adjacent color stops. For example,
/// with stops `[red, green, blue]` across 5 positions:
///
/// ```text
///   pos 0: red       (start of segment 0)
///   pos 1: yellow    (midpoint of segment 0: red → green)
///   pos 2: green     (end of segment 0 / start of segment 1)
///   pos 3: cyan      (midpoint of segment 1: green → blue)
///   pos 4: blue      (end of segment 1)
/// ```
#[allow(clippy::cast_precision_loss)]
fn precompute_gradient_colors(colors: &[RGB8], total_positions: usize) -> SmallVec<[RGB8; 16]> {
    if total_positions == 0 {
        return SmallVec::new();
    }
    if colors.is_empty() {
        return smallvec::smallvec![RGB8::default(); total_positions];
    }
    if colors.len() == 1 || total_positions == 1 {
        return smallvec::smallvec![colors[0]; total_positions];
    }

    // Number of linear segments between adjacent color stops.
    // E.g., 3 colors = 2 segments: [c0→c1] and [c1→c2].
    let segment_count = colors.len() - 1;

    (0..total_positions)
        .map(|i| {
            // Normalized position in [0.0, 1.0] across the full LED range.
            // E.g., for 5 positions: 0/4=0.0, 1/4=0.25, 2/4=0.5, 3/4=0.75, 4/4=1.0.
            // These are small integers, so the f32 conversion is exact.
            let normalized = i as f32 / (total_positions - 1) as f32;

            // Scale to "segment space" [0.0, segment_count]. The integer part
            // selects which pair of color stops we interpolate between, and the
            // fractional part is how far through that segment we are.
            // E.g., with 2 segments and normalized=0.5: 0.5 * 2 = 1.0 →
            //   segment 1 at t=0.0 (exactly at the middle color stop).
            let in_segment_space = normalized * segment_count as f32;

            // Clamp segment index to valid range. This guards against
            // floating-point producing segment_count when normalized=1.0 exactly.
            // in_segment_space is non-negative and bounded by segment_count (a
            // small usize), so the truncation and sign loss are safe.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let segment_idx = (in_segment_space as usize).min(segment_count - 1);

            // Fractional position within this segment: 0.0 = at colors[segment_idx],
            // 1.0 = at colors[segment_idx + 1]. Clamped to guard against any
            // floating-point drift at the boundaries.
            let t = (in_segment_space - segment_idx as f32).clamp(0.0, 1.0);

            let c1 = colors[segment_idx];
            let c2 = colors[segment_idx + 1];

            RGB8::new(
                lerp_u8(c1.r, c2.r, t),
                lerp_u8(c1.g, c2.g, t),
                lerp_u8(c1.b, c2.b, t),
            )
        })
        .collect()
}

/// Linear interpolation between two `u8` color channel values.
///
/// Computes `a + (b - a) * t` in floating point, then rounds back to `u8`.
/// At `t=0.0` returns `a`, at `t=1.0` returns `b`, at `t=0.5` returns their midpoint.
///
/// # Panics
/// Debug-asserts that `t` is in `[0.0, 1.0]`.
#[inline]
fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    debug_assert!(
        (0.0..=1.0).contains(&t),
        "lerp_u8: t={t} outside [0.0, 1.0]"
    );
    let a_f = f32::from(a);
    let b_f = f32::from(b);
    // With a,b in [0,255] and t in [0.0,1.0], the result is in [0.0, 255.0],
    // so the cast to u8 cannot truncate or produce a negative value.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let result = (a_f + (b_f - a_f) * t).round() as u8;
    result
}

/// Compute which LED indices should be lit for a baked rule.
///
/// Handles both forward ranges (start < end) and reverse ranges (start > end)
/// for mirror effects. When `rpm_upper` is set, computes proportional lighting.
///
/// Uses the pre-clamped `start`, `end`, and `led_count` from the baked rule.
fn compute_leds_to_light(rpm: u32, rule: &BakedLedRule) -> SmallVec<[usize; 16]> {
    let start = rule.start;
    let end = rule.end;
    let led_count = rule.led_count;

    // Determine how many LEDs to light based on RPM
    let active_count = match rule.rpm_upper {
        None => led_count, // No upper bound: all LEDs on when rule met
        Some(upper) if upper <= rule.rpm_lower => led_count, // Invalid range: all on
        Some(upper) => {
            // Proportional: divide RPM range into led_count equal buckets
            // Each bucket adds one more LED
            let rpm_range = upper - rule.rpm_lower;
            let rpm_progress = rpm.saturating_sub(rule.rpm_lower).min(rpm_range);
            // Formula: 1 + (rpm_progress * led_count) / rpm_range, capped at led_count
            let progress_scaled = u64::from(rpm_progress) * led_count as u64;
            // Safe truncation: result is bounded by led_count which fits in usize
            #[allow(clippy::cast_possible_truncation)]
            let count = 1 + (progress_scaled / u64::from(rpm_range)) as usize;
            count.min(led_count)
        }
    };

    // Generate LED indices based on direction
    if start <= end {
        // Forward: light from start toward end
        (start..start + active_count).collect()
    } else {
        // Reverse: light from start toward end (going down)
        (end + led_count - active_count..=start).rev().collect()
    }
}

/// Compute the optimal render interval for smooth blinking
///
/// This function finds the GCD of all blink intervals to ensure we hit
/// all blink transitions accurately when rendering at wallclock-aligned times.
///
/// # Arguments
/// * `rules` - List of LED rule configurations
///
/// # Returns
/// `Some(interval_ms)` if blinking rules exist, `None` if no blinking
/// (meaning rendering only needs to happen when RPM changes)
#[must_use]
pub fn compute_render_interval(rules: &[LedRule]) -> Option<u32> {
    use num_integer::Integer;

    // Collect all blink intervals from blinking rules
    let blink_intervals: Vec<u32> = rules
        .iter()
        .filter(|r| r.blink && r.blink_ms > 0)
        .map(|r| r.blink_ms)
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

    /// Convenience wrapper that bakes rules and computes LED state in one call.
    ///
    /// Bakes on every call — use [`bake_led_rules`] + [`compute_led_state`]
    /// in production code.
    fn compute_led_state_simple(
        rpm: u32,
        rules: &[LedRule],
        total_leds: usize,
        timestamp_ms: u64,
    ) -> LedState {
        let baked = bake_led_rules(rules, total_leds);
        compute_led_state(rpm, &baked, timestamp_ms)
    }

    fn make_rules() -> Vec<LedRule> {
        vec![
            LedRule {
                name: "Green".to_string(),
                rpm_lower: 3000,
                rpm_upper: None,
                start_led: 0,
                end_led: 2,
                colors: smallvec::smallvec![RGB8::new(0, 255, 0)],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Yellow".to_string(),
                rpm_lower: 5000,
                rpm_upper: None,
                start_led: 0,
                end_led: 4,
                colors: smallvec::smallvec![RGB8::new(255, 255, 0)],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Red".to_string(),
                rpm_lower: 6500,
                rpm_upper: None,
                start_led: 0,
                end_led: 7,
                colors: smallvec::smallvec![RGB8::new(255, 0, 0)],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Blink".to_string(),
                rpm_lower: 7000,
                rpm_upper: None,
                start_led: 0,
                end_led: 7,
                colors: smallvec::smallvec![RGB8::new(255, 0, 0)],
                blink: true,
                blink_ms: 100,
            },
        ]
    }

    #[test]
    fn test_no_rule_active() {
        let rules = make_rules();
        let state = compute_led_state_simple(2000, &rules, 8, 0);

        assert!(!state.has_blinking);
        assert!(state.leds.iter().all(|&led| led == RGB8::default()));
    }

    #[test]
    fn test_first_rule() {
        let rules = make_rules();
        let state = compute_led_state_simple(3500, &rules, 8, 0);

        assert!(!state.has_blinking);
        
        // LEDs 0-2 should be green
        assert_eq!(state.leds[0], RGB8::new(0, 255, 0));
        assert_eq!(state.leds[1], RGB8::new(0, 255, 0));
        assert_eq!(state.leds[2], RGB8::new(0, 255, 0));
        // LEDs 3-7 should be off
        assert_eq!(state.leds[3], RGB8::default());
    }

    #[test]
    fn test_higher_rule_cumulative_with_lower() {
        let rules = make_rules();
        let state = compute_led_state_simple(5500, &rules, 8, 0);

        // LEDs 0-4 should be yellow (Yellow overrides Green for 0-2, adds 3-4)
        assert_eq!(state.leds[0], RGB8::new(255, 255, 0));
        assert_eq!(state.leds[4], RGB8::new(255, 255, 0));
        // LEDs 5-7 should be off
        assert_eq!(state.leds[5], RGB8::default());
    }

    #[test]
    fn test_blink_on_phase() {
        let rules = make_rules();
        // timestamp 0 with blink_ms 100: (0 / 100) % 2 == 0 -> ON
        let state = compute_led_state_simple(7500, &rules, 8, 0);

        assert!(state.has_blinking);
        
        // During blink ON, all LEDs should be red
        assert_eq!(state.leds[0], RGB8::new(255, 0, 0));
        assert_eq!(state.leds[7], RGB8::new(255, 0, 0));
    }

    #[test]
    fn test_blink_off_shows_underneath() {
        let rules = make_rules();
        // timestamp 100 with blink_ms 100: (100 / 100) % 2 == 1 -> OFF
        let state = compute_led_state_simple(7500, &rules, 8, 100);

        assert!(state.has_blinking);
        
        // During blink OFF, should show Red rule underneath (LEDs 0-7)
        assert_eq!(state.leds[0], RGB8::new(255, 0, 0));
        assert_eq!(state.leds[7], RGB8::new(255, 0, 0));
    }

    #[test]
    fn test_blink_timing() {
        let rules = vec![LedRule {
            name: "Blink".to_string(),
            rpm_lower: 1000,
            rpm_upper: None,
            start_led: 0,
            end_led: 0,
            colors: smallvec::smallvec![RGB8::new(255, 0, 0)],
            blink: true,
            blink_ms: 100,
        }];

        // t=0: ON (0/100 % 2 == 0)
        let state = compute_led_state_simple(1500, &rules, 1, 0);
        assert_eq!(state.leds[0], RGB8::new(255, 0, 0));

        // t=50: still ON (50/100 % 2 == 0)
        let state = compute_led_state_simple(1500, &rules, 1, 50);
        assert_eq!(state.leds[0], RGB8::new(255, 0, 0));

        // t=100: OFF (100/100 % 2 == 1)
        let state = compute_led_state_simple(1500, &rules, 1, 100);
        assert_eq!(state.leds[0], RGB8::default());

        // t=150: still OFF (150/100 % 2 == 1)
        let state = compute_led_state_simple(1500, &rules, 1, 150);
        assert_eq!(state.leds[0], RGB8::default());

        // t=200: ON again (200/100 % 2 == 0)
        let state = compute_led_state_simple(1500, &rules, 1, 200);
        assert_eq!(state.leds[0], RGB8::new(255, 0, 0));
    }

    #[test]
    fn test_led_bounds_clamping() {
        let rules = vec![LedRule {
            name: "OutOfBounds".to_string(),
            rpm_lower: 1000,
            rpm_upper: None,
            start_led: 5,
            end_led: 100, // Way beyond total LEDs
            colors: smallvec::smallvec![RGB8::new(255, 0, 0)],
            blink: false,
            blink_ms: 500,
        }];
        let state = compute_led_state_simple(1500, &rules, 8, 0);

        // Should clamp to available LEDs (5-7)
        assert_eq!(state.leds[5], RGB8::new(255, 0, 0));
        assert_eq!(state.leds[7], RGB8::new(255, 0, 0));
        assert_eq!(state.leds.len(), 8);
    }

    /// Test cumulative rules with non-overlapping LED ranges (progressive shift light)
    #[test]
    fn test_cumulative_non_overlapping_ranges() {
        // Simulate a progressive shift light: blue 0-2, green 3-5, yellow 6-8, red 9-11
        let rules = vec![
            LedRule {
                name: "Off".to_string(),
                rpm_lower: 0,
                rpm_upper: None,
                start_led: 0,
                end_led: 11,
                colors: smallvec::smallvec![RGB8::new(0, 0, 0)],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Blue".to_string(),
                rpm_lower: 1000,
                rpm_upper: None,
                start_led: 0,
                end_led: 2,
                colors: smallvec::smallvec![RGB8::new(0, 0, 255)],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Green".to_string(),
                rpm_lower: 1500,
                rpm_upper: None,
                start_led: 3,
                end_led: 5,
                colors: smallvec::smallvec![RGB8::new(0, 255, 0)],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Yellow".to_string(),
                rpm_lower: 2000,
                rpm_upper: None,
                start_led: 6,
                end_led: 8,
                colors: smallvec::smallvec![RGB8::new(255, 255, 0)],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Red".to_string(),
                rpm_lower: 2500,
                rpm_upper: None,
                start_led: 9,
                end_led: 11,
                colors: smallvec::smallvec![RGB8::new(255, 0, 0)],
                blink: false,
                blink_ms: 500,
            },
        ];

        // At 1200 RPM: only blue should be on
        let state = compute_led_state_simple(1200, &rules, 12, 0);
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255)); // blue
        assert_eq!(state.leds[2], RGB8::new(0, 0, 255)); // blue
        assert_eq!(state.leds[3], RGB8::default()); // off
        assert_eq!(state.leds[11], RGB8::default()); // off

        // At 1700 RPM: blue AND green should be on
        let state = compute_led_state_simple(1700, &rules, 12, 0);
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255)); // blue stays on
        assert_eq!(state.leds[2], RGB8::new(0, 0, 255)); // blue stays on
        assert_eq!(state.leds[3], RGB8::new(0, 255, 0)); // green
        assert_eq!(state.leds[5], RGB8::new(0, 255, 0)); // green
        assert_eq!(state.leds[6], RGB8::default()); // off

        // At 2200 RPM: blue, green, AND yellow should be on
        let state = compute_led_state_simple(2200, &rules, 12, 0);
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255)); // blue
        assert_eq!(state.leds[3], RGB8::new(0, 255, 0)); // green
        assert_eq!(state.leds[6], RGB8::new(255, 255, 0)); // yellow
        assert_eq!(state.leds[8], RGB8::new(255, 255, 0)); // yellow
        assert_eq!(state.leds[9], RGB8::default()); // off

        // At 3000 RPM: all colors should be on
        let state = compute_led_state_simple(3000, &rules, 12, 0);
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255)); // blue
        assert_eq!(state.leds[3], RGB8::new(0, 255, 0)); // green
        assert_eq!(state.leds[6], RGB8::new(255, 255, 0)); // yellow
        assert_eq!(state.leds[9], RGB8::new(255, 0, 0)); // red
        assert_eq!(state.leds[11], RGB8::new(255, 0, 0)); // red
    }

    /// Test progressive single-LED rules within a color zone
    #[test]
    fn test_progressive_single_led_rules() {
        // Each LED lights up at a different RPM
        let rules = vec![
            LedRule {
                name: "Off".to_string(),
                rpm_lower: 0,
                rpm_upper: None,
                start_led: 0,
                end_led: 2,
                colors: smallvec::smallvec![RGB8::new(0, 0, 0)],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Blue 1".to_string(),
                rpm_lower: 1000,
                rpm_upper: None,
                start_led: 0,
                end_led: 0,
                colors: smallvec::smallvec![RGB8::new(0, 0, 255)],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Blue 2".to_string(),
                rpm_lower: 1167,
                rpm_upper: None,
                start_led: 1,
                end_led: 1,
                colors: smallvec::smallvec![RGB8::new(0, 0, 255)],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Blue 3".to_string(),
                rpm_lower: 1333,
                rpm_upper: None,
                start_led: 2,
                end_led: 2,
                colors: smallvec::smallvec![RGB8::new(0, 0, 255)],
                blink: false,
                blink_ms: 500,
            },
        ];

        // At 1100 RPM: only LED 0
        let state = compute_led_state_simple(1100, &rules, 3, 0);
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255));
        assert_eq!(state.leds[1], RGB8::default());
        assert_eq!(state.leds[2], RGB8::default());

        // At 1200 RPM: LEDs 0 and 1
        let state = compute_led_state_simple(1200, &rules, 3, 0);
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255));
        assert_eq!(state.leds[1], RGB8::new(0, 0, 255));
        assert_eq!(state.leds[2], RGB8::default());

        // At 1400 RPM: all three LEDs
        let state = compute_led_state_simple(1400, &rules, 3, 0);
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255));
        assert_eq!(state.leds[1], RGB8::new(0, 0, 255));
        assert_eq!(state.leds[2], RGB8::new(0, 0, 255));
    }

    /// Test blink OFF shows all rules underneath (cumulative)
    #[test]
    fn test_blink_off_shows_all_underneath() {
        let rules = vec![
            LedRule {
                name: "Blue".to_string(),
                rpm_lower: 1000,
                rpm_upper: None,
                start_led: 0,
                end_led: 2,
                colors: smallvec::smallvec![RGB8::new(0, 0, 255)],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Green".to_string(),
                rpm_lower: 1500,
                rpm_upper: None,
                start_led: 3,
                end_led: 5,
                colors: smallvec::smallvec![RGB8::new(0, 255, 0)],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Shift".to_string(),
                rpm_lower: 2000,
                rpm_upper: None,
                start_led: 0,
                end_led: 5,
                colors: smallvec::smallvec![RGB8::new(255, 0, 0)],
                blink: true,
                blink_ms: 100,
            },
        ];

        // Blink ON (t=0): all red
        let state = compute_led_state_simple(2500, &rules, 6, 0);
        assert!(state.has_blinking);
        assert_eq!(state.leds[0], RGB8::new(255, 0, 0));
        assert_eq!(state.leds[5], RGB8::new(255, 0, 0));

        // Blink OFF (t=100): should show blue AND green underneath
        let state = compute_led_state_simple(2500, &rules, 6, 100);
        assert!(state.has_blinking);
        // Blue LEDs 0-2
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255));
        assert_eq!(state.leds[2], RGB8::new(0, 0, 255));
        // Green LEDs 3-5
        assert_eq!(state.leds[3], RGB8::new(0, 255, 0));
        assert_eq!(state.leds[5], RGB8::new(0, 255, 0));
    }

    /// Test that a "Black" rule at the same RPM as Shift clears LEDs during blink OFF
    #[test]
    fn test_blink_off_with_black_underneath() {
        let rules = vec![
            LedRule {
                name: "Blue".to_string(),
                rpm_lower: 1000,
                rpm_upper: None,
                start_led: 0,
                end_led: 2,
                colors: smallvec::smallvec![RGB8::new(0, 0, 255)],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Green".to_string(),
                rpm_lower: 1500,
                rpm_upper: None,
                start_led: 3,
                end_led: 5,
                colors: smallvec::smallvec![RGB8::new(0, 255, 0)],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Black".to_string(),
                rpm_lower: 2000,
                rpm_upper: None,
                start_led: 0,
                end_led: 5,
                colors: smallvec::smallvec![RGB8::new(0, 0, 0)], // All off
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Shift".to_string(),
                rpm_lower: 2000,
                rpm_upper: None,
                start_led: 0,
                end_led: 5,
                colors: smallvec::smallvec![RGB8::new(255, 0, 0)],
                blink: true,
                blink_ms: 100,
            },
        ];

        // Blink ON (t=0): all red
        let state = compute_led_state_simple(2500, &rules, 6, 0);
        assert!(state.has_blinking);
        assert_eq!(state.leds[0], RGB8::new(255, 0, 0));
        assert_eq!(state.leds[5], RGB8::new(255, 0, 0));

        // Blink OFF (t=100): Black rule should override blue/green, all LEDs off
        let state = compute_led_state_simple(2500, &rules, 6, 100);
        assert!(state.has_blinking);
        // All LEDs should be black (off)
        for i in 0..6 {
            assert_eq!(state.leds[i], RGB8::new(0, 0, 0), "LED {i} should be off");
        }
    }

    /// Test multiple independent blinking rules
    #[test]
    fn test_multiple_independent_blinks() {
        let rules = vec![
            LedRule {
                name: "Slow Blink".to_string(),
                rpm_lower: 1000,
                rpm_upper: None,
                start_led: 0,
                end_led: 2,
                colors: smallvec::smallvec![RGB8::new(0, 0, 255)],
                blink: true,
                blink_ms: 200, // 200ms period
            },
            LedRule {
                name: "Fast Blink".to_string(),
                rpm_lower: 1000,
                rpm_upper: None,
                start_led: 3,
                end_led: 5,
                colors: smallvec::smallvec![RGB8::new(255, 0, 0)],
                blink: true,
                blink_ms: 100, // 100ms period
            },
        ];

        // t=0: both ON (0/200 % 2 == 0, 0/100 % 2 == 0)
        let state = compute_led_state_simple(1500, &rules, 6, 0);
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255)); // slow blink on
        assert_eq!(state.leds[3], RGB8::new(255, 0, 0)); // fast blink on

        // t=100: slow ON, fast OFF (100/200 % 2 == 0, 100/100 % 2 == 1)
        let state = compute_led_state_simple(1500, &rules, 6, 100);
        assert_eq!(state.leds[0], RGB8::new(0, 0, 255)); // slow blink still on
        assert_eq!(state.leds[3], RGB8::default()); // fast blink off

        // t=200: slow OFF, fast ON (200/200 % 2 == 1, 200/100 % 2 == 0)
        let state = compute_led_state_simple(1500, &rules, 6, 200);
        assert_eq!(state.leds[0], RGB8::default()); // slow blink off
        assert_eq!(state.leds[3], RGB8::new(255, 0, 0)); // fast blink on

        // t=300: slow OFF, fast OFF (300/200 % 2 == 1, 300/100 % 2 == 1)
        let state = compute_led_state_simple(1500, &rules, 6, 300);
        assert_eq!(state.leds[0], RGB8::default()); // slow blink off
        assert_eq!(state.leds[3], RGB8::default()); // fast blink off
    }

    /// Test proportional LED lighting with `rpm_upper`
    #[test]
    fn test_proportional_leds_forward() {
        // 4 LEDs (0-3) with RPM range 1000-2000
        let rules = vec![LedRule {
            name: "Progressive".to_string(),
            rpm_lower: 1000,
            rpm_upper: Some(2000),
            start_led: 0,
            end_led: 3,
            colors: smallvec::smallvec![RGB8::new(0, 255, 0)],
            blink: false,
            blink_ms: 500,
        }];

        // At 1000 RPM: 1 LED (LED 0) - bucket 0-249
        let state = compute_led_state_simple(1000, &rules, 4, 0);
        assert_eq!(state.leds[0], RGB8::new(0, 255, 0));
        assert_eq!(state.leds[1], RGB8::default());
        assert_eq!(state.leds[2], RGB8::default());
        assert_eq!(state.leds[3], RGB8::default());

        // At 1249 RPM: still 1 LED - progress=249, bucket 0-249
        let state = compute_led_state_simple(1249, &rules, 4, 0);
        assert_eq!(state.leds[0], RGB8::new(0, 255, 0));
        assert_eq!(state.leds[1], RGB8::default());

        // At 1250 RPM: 2 LEDs (0-1) - progress=250, bucket 250-499
        let state = compute_led_state_simple(1250, &rules, 4, 0);
        assert_eq!(state.leds[0], RGB8::new(0, 255, 0));
        assert_eq!(state.leds[1], RGB8::new(0, 255, 0));
        assert_eq!(state.leds[2], RGB8::default());
        assert_eq!(state.leds[3], RGB8::default());

        // At 1500 RPM: 3 LEDs (0-2) - progress=500, bucket 500-749
        let state = compute_led_state_simple(1500, &rules, 4, 0);
        assert_eq!(state.leds[0], RGB8::new(0, 255, 0));
        assert_eq!(state.leds[1], RGB8::new(0, 255, 0));
        assert_eq!(state.leds[2], RGB8::new(0, 255, 0));
        assert_eq!(state.leds[3], RGB8::default());

        // At 1750 RPM: 4 LEDs - progress=750, bucket 750-1000
        let state = compute_led_state_simple(1750, &rules, 4, 0);
        assert_eq!(state.leds[0], RGB8::new(0, 255, 0));
        assert_eq!(state.leds[1], RGB8::new(0, 255, 0));
        assert_eq!(state.leds[2], RGB8::new(0, 255, 0));
        assert_eq!(state.leds[3], RGB8::new(0, 255, 0));

        // At 2000 RPM: all 4 LEDs
        let state = compute_led_state_simple(2000, &rules, 4, 0);
        assert_eq!(state.leds[0], RGB8::new(0, 255, 0));
        assert_eq!(state.leds[1], RGB8::new(0, 255, 0));
        assert_eq!(state.leds[2], RGB8::new(0, 255, 0));
        assert_eq!(state.leds[3], RGB8::new(0, 255, 0));

        // Above 2000 RPM: still all 4 LEDs (capped)
        let state = compute_led_state_simple(3000, &rules, 4, 0);
        assert_eq!(state.leds[0], RGB8::new(0, 255, 0));
        assert_eq!(state.leds[3], RGB8::new(0, 255, 0));
    }

    /// Test proportional LED lighting with reversed range (mirror effect)
    #[test]
    fn test_proportional_leds_reverse() {
        // LEDs 3->0 (reversed) with RPM range 1000-2000
        let rules = vec![LedRule {
            name: "Mirror".to_string(),
            rpm_lower: 1000,
            rpm_upper: Some(2000),
            start_led: 3,
            end_led: 0,
            colors: smallvec::smallvec![RGB8::new(255, 0, 0)],
            blink: false,
            blink_ms: 500,
        }];

        // At 1000 RPM: 1 LED (LED 3 - starts from the "outside")
        let state = compute_led_state_simple(1000, &rules, 4, 0);
        assert_eq!(state.leds[0], RGB8::default());
        assert_eq!(state.leds[1], RGB8::default());
        assert_eq!(state.leds[2], RGB8::default());
        assert_eq!(state.leds[3], RGB8::new(255, 0, 0));

        // At 1250 RPM: 2 LEDs (3, 2)
        let state = compute_led_state_simple(1250, &rules, 4, 0);
        assert_eq!(state.leds[0], RGB8::default());
        assert_eq!(state.leds[1], RGB8::default());
        assert_eq!(state.leds[2], RGB8::new(255, 0, 0));
        assert_eq!(state.leds[3], RGB8::new(255, 0, 0));

        // At 1500 RPM: 3 LEDs (3, 2, 1)
        let state = compute_led_state_simple(1500, &rules, 4, 0);
        assert_eq!(state.leds[0], RGB8::default());
        assert_eq!(state.leds[1], RGB8::new(255, 0, 0));
        assert_eq!(state.leds[2], RGB8::new(255, 0, 0));
        assert_eq!(state.leds[3], RGB8::new(255, 0, 0));

        // At 2000 RPM: all 4 LEDs
        let state = compute_led_state_simple(2000, &rules, 4, 0);
        assert_eq!(state.leds[0], RGB8::new(255, 0, 0));
        assert_eq!(state.leds[1], RGB8::new(255, 0, 0));
        assert_eq!(state.leds[2], RGB8::new(255, 0, 0));
        assert_eq!(state.leds[3], RGB8::new(255, 0, 0));
    }

    /// Test mirror effect for symmetric shift light (both sides light toward center)
    #[test]
    fn test_symmetric_mirror_shift_light() {
        // 8 LEDs: left side (0-3) lights right, right side (7-4) lights left
        let rules = vec![
            LedRule {
                name: "Left".to_string(),
                rpm_lower: 1000,
                rpm_upper: Some(2000),
                start_led: 0,
                end_led: 3,
                colors: smallvec::smallvec![RGB8::new(0, 255, 0)],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Right".to_string(),
                rpm_lower: 1000,
                rpm_upper: Some(2000),
                start_led: 7,
                end_led: 4,
                colors: smallvec::smallvec![RGB8::new(0, 255, 0)],
                blink: false,
                blink_ms: 500,
            },
        ];

        // At 1000 RPM: outer LEDs only (0 and 7)
        let state = compute_led_state_simple(1000, &rules, 8, 0);
        assert_eq!(state.leds[0], RGB8::new(0, 255, 0));
        assert_eq!(state.leds[1], RGB8::default());
        assert_eq!(state.leds[6], RGB8::default());
        assert_eq!(state.leds[7], RGB8::new(0, 255, 0));

        // At 1250 RPM: 2 on each side (0,1 and 6,7)
        let state = compute_led_state_simple(1250, &rules, 8, 0);
        assert_eq!(state.leds[0], RGB8::new(0, 255, 0));
        assert_eq!(state.leds[1], RGB8::new(0, 255, 0));
        assert_eq!(state.leds[2], RGB8::default());
        assert_eq!(state.leds[5], RGB8::default());
        assert_eq!(state.leds[6], RGB8::new(0, 255, 0));
        assert_eq!(state.leds[7], RGB8::new(0, 255, 0));

        // At 2000 RPM: all LEDs on
        let state = compute_led_state_simple(2000, &rules, 8, 0);
        for i in 0..8 {
            assert_eq!(state.leds[i], RGB8::new(0, 255, 0), "LED {i} should be on");
        }
    }

    /// Test gradient interpolation with two colors
    #[test]
    fn test_gradient_two_colors() {
        let rules = vec![LedRule {
            name: "Gradient".to_string(),
            rpm_lower: 1000,
            rpm_upper: None,
            start_led: 0,
            end_led: 4,
            colors: smallvec::smallvec![RGB8::new(255, 0, 0), RGB8::new(0, 0, 255)],
            blink: false,
            blink_ms: 500,
        }];

        let state = compute_led_state_simple(1500, &rules, 5, 0);

        // First LED should be red
        assert_eq!(state.leds[0], RGB8::new(255, 0, 0));
        // Last LED should be blue
        assert_eq!(state.leds[4], RGB8::new(0, 0, 255));
        // Middle LED should be purple (50% blend)
        assert_eq!(state.leds[2], RGB8::new(128, 0, 128));
        // LED 1 should be 75% red, 25% blue
        assert_eq!(state.leds[1], RGB8::new(191, 0, 64));
        // LED 3 should be 25% red, 75% blue
        assert_eq!(state.leds[3], RGB8::new(64, 0, 191));
    }

    /// Test that gradient is static - colors don't shift as RPM increases
    /// With 5 LEDs, RPM range 1000-2000, and RPM at 1500:
    /// - 3 LEDs should be lit (proportional)
    /// - LED 2 (middle of full range) should have midway color, not end color
    #[test]
    fn test_gradient_static_with_proportional() {
        let rules = vec![LedRule {
            name: "Static Gradient".to_string(),
            rpm_lower: 1000,
            rpm_upper: Some(2000),
            start_led: 0,
            end_led: 4,
            colors: smallvec::smallvec![RGB8::new(255, 0, 0), RGB8::new(0, 0, 255)],
            blink: false,
            blink_ms: 500,
        }];

        let state = compute_led_state_simple(1500, &rules, 5, 0);

        // At 1500 RPM (halfway), 3 LEDs should be lit: 0, 1, 2
        // LED 0: first color (red)
        assert_eq!(state.leds[0], RGB8::new(255, 0, 0));
        // LED 1: 25% through gradient (75% red, 25% blue)
        assert_eq!(state.leds[1], RGB8::new(191, 0, 64));
        // LED 2: 50% through gradient (midway color - purple)
        // This is the key assertion: LED 2 should be purple, NOT blue
        assert_eq!(state.leds[2], RGB8::new(128, 0, 128));
        // LEDs 3 and 4 should be off (black)
        assert_eq!(state.leds[3], RGB8::new(0, 0, 0));
        assert_eq!(state.leds[4], RGB8::new(0, 0, 0));
    }

    /// Test gradient with three colors (red -> green -> blue)
    #[test]
    fn test_gradient_three_colors() {
        let rules = vec![LedRule {
            name: "Rainbow".to_string(),
            rpm_lower: 1000,
            rpm_upper: None,
            start_led: 0,
            end_led: 4,
            colors: smallvec::smallvec![RGB8::new(255, 0, 0), RGB8::new(0, 255, 0), RGB8::new(0, 0, 255)],
            blink: false,
            blink_ms: 500,
        }];

        let state = compute_led_state_simple(1500, &rules, 5, 0);

        // First LED should be red
        assert_eq!(state.leds[0], RGB8::new(255, 0, 0));
        // Middle LED (index 2) should be green
        assert_eq!(state.leds[2], RGB8::new(0, 255, 0));
        // Last LED should be blue
        assert_eq!(state.leds[4], RGB8::new(0, 0, 255));
        // LED 1 should be between red and green
        assert_eq!(state.leds[1], RGB8::new(128, 128, 0));
        // LED 3 should be between green and blue
        assert_eq!(state.leds[3], RGB8::new(0, 128, 128));
    }

    /// Test single color acts as solid (no gradient)
    #[test]
    fn test_gradient_single_color() {
        let rules = vec![LedRule {
            name: "Solid".to_string(),
            rpm_lower: 1000,
            rpm_upper: None,
            start_led: 0,
            end_led: 2,
            colors: smallvec::smallvec![RGB8::new(255, 128, 0)],
            blink: false,
            blink_ms: 500,
        }];

        let state = compute_led_state_simple(1500, &rules, 3, 0);

        // All LEDs should be the same color
        assert_eq!(state.leds[0], RGB8::new(255, 128, 0));
        assert_eq!(state.leds[1], RGB8::new(255, 128, 0));
        assert_eq!(state.leds[2], RGB8::new(255, 128, 0));
    }
}
