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
| No STA SSID configured (AP always active) | dim white solid |
| Searching / associating with the SSID | blue slow blink (~1 Hz) |
| L2 associated, waiting for DHCP | blue fast blink (~4 Hz) |
| STA connected with IP | blue solid |
| AP client currently associated | overlay: cyan pulse |

**Implemented.** `WifiStaState` is split into `NotConfigured`, `Searching`,
`AwaitingIp`, and `Connected`; the manager publishes `Searching` during
connect attempts (previously the LED went dark while associating), idles in
an AP-only loop when no SSID is configured, and polls the AP station list
each second for the cyan overlay pulse.

### OBD LED

| State | Color / pattern |
|---|---|
| No dongle connection (includes failed connect attempts) | red blink |
| TCP connecting / ELM327 init | amber blink |
| Ready, idle | green solid |
| Ready, request in flight | green pulse (existing flicker logic) |

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

1. **Pattern engine** — *done.* Blink/pulse/chase phases are derived from a
   wallclock reference in `status_leds.rs`, so the task wakes exactly at
   phase boundaries; per-LED colors render into the `[RGB8; 3]` buffer and
   flush dirty-checked.
2. **Map existing states** — *done.* WIFI, OBD, and CLIENT render the color
   table above, including the client-count hue shift and dim-pulse activity.
3. **Richer producer states**:
   - `WifiStaState` — *done*, see the WIFI section above.
   - OBD disconnected/unreachable — *done*: `Disconnected` renders as a red
     blink directly, so failed connect attempts are immediately visible; no
     extra producer state was needed.
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
