# Status LED RGB Color Plan

The new PCB replaces the three single-color status LEDs with a chain of three
WS2812B RGB LEDs on GPIO 7, labeled (outward from the MCU) **WIFI**, **OBD**,
**CLIENT**. The initial port (see `status_leds.rs`) reproduces the old
semantics with fixed colors: WIFI = red, OBD = amber, CLIENT = green, each
either off, blinking, or solid.

Since the silkscreen now names each LED's *function*, color is freed up to
encode *state*. This plan proposes how to use it.

## Proposed color scheme

Pattern legend: `solid` = steady on, `blink` = 2 Hz square, `pulse` = brief
activity flicker between full color and ~30% dimmed (instead of full-off, so
the LED stays identifiable during heavy traffic).

### WIFI LED

| State | Color / pattern |
|---|---|
| STA disabled / not configured (AP only) | dim white solid |
| STA connecting / waiting for IP | blue blink |
| STA connected with IP | blue solid |
| STA connect failure (auth/timeout, will retry) | red blink |
| AP client currently associated | overlay: cyan pulse |

### OBD LED

| State | Color / pattern |
|---|---|
| Dongle TCP disconnected | off |
| TCP connecting / ELM327 init | amber blink |
| Ready, idle | green solid |
| Ready, request in flight | green pulse (existing flicker logic) |
| Repeated timeouts / protocol errors | red blink until next success |

### CLIENT LED

| State | Color / pattern |
|---|---|
| No clients | off |
| 1+ clients connected, idle | green solid |
| Client request activity | green pulse |
| More than one client | shift hue toward cyan/blue (e.g. 1 = green, 2 = cyan, 3+ = blue) |

### Global overrides

| Event | Effect |
|---|---|
| Boot | current 3-blink sequence, each LED in its own hue, boot LED (GPIO 46) blinking along |
| OTA update | purple chase across all three LEDs (distinct from any steady state) |
| Panic/fatal (future) | all three solid red; boot LED could serve as a driver-visible fault light |

Color-blindness note: color is never the *only* channel — position (labeled),
on/off, and blink pattern always disambiguate. Red vs. green states also
differ in pattern (blink vs. solid) wherever both can occur on the same LED.

## Implementation steps

1. **Pattern engine** — replace the per-LED `bool` outputs with a small
   `LedPattern { color: RGB8, mode: Solid | Blink(ms) | Pulse }` per LED.
   The existing event loop already computes wake deadlines for WiFi blink,
   activity flicker, and the OTA chase; generalize that to one deadline per
   LED derived from its pattern. `drive_leds()` then renders the three
   patterns into the `[RGB8; 3]` buffer and flushes (dirty-checked, as now).
2. **Map existing states** — express today's semantics in the new table
   (colors above) without adding any new producer messages. This lands the
   visible color change cheaply.
3. **Richer producer states** — extend the message enums where the new
   scheme needs distinctions that don't exist yet:
   - `WifiStaState`: add `AuthFailed` (from wifi.rs disconnect reason) and an
     AP-client-associated signal (AP staconnected/stadisconnected events).
   - `DongleState`: add an `Erroring` flag driven by the obd2 task's timeout
     counter (set after N consecutive failures, cleared on success).
   - `ClientCount` already carries the count for the hue shift.
4. **Optional polish**
   - Short gamma-correct fade (~100 ms, 3–4 steps) on color transitions to
     avoid harsh snaps; only ticks while a fade is active so the task stays
     event-driven.
   - Repurpose the boot LED (GPIO 46) after boot as a heartbeat or fault
     indicator.
   - Web UI: live preview of the status LED meanings on the status page
     (the SSE channel already carries wifi/dongle/client state).

Colors stay firmware constants; only brightness is user-configurable
(`status_led_brightness`, already added). If users ever want custom colors,
the pattern table is a single place to make configurable later.
