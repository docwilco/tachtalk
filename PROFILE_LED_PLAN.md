# Plan: Profile-Driven LED Display with PID Binding

## TL;DR

Generalize the LED display system from hardcoded RPM to profile-driven PID monitoring. Each profile binds to an OBD2 PID, and active profiles determine which PIDs are polled. Three profile types: Normal (one active, cycled via button), Overlay (always active, e.g. coolant warning), and Triggered (button-toggled with auto-disable, e.g. pit limiter). Rules use decoded PID values instead of hardcoded RPM.

## Decisions

- **One main + overlays**: Existing button cycles Normal profiles. Overlays always monitor. Triggered profiles have dedicated buttons.
- **Clean break**: Rename `rpm_lower/rpm_upper` → `value_lower/value_upper`, `preview_rpm` → `preview_value`. No migration.
- **Built-in PID decoders**: Hardcoded formulas for well-known Mode01 PIDs. No user-specified formulas.
- **Overlays always active**: Always poll their PID and evaluate rules (rules determine visual output).
- **Triggered profiles**: Start disabled. Button enables/disables. Auto-disable when decoded value exceeds threshold.
- **Speed limiter target**: Fixed in profile config (value thresholds in rules).
- **PID priority**: Configurable per profile (fast/slow).
- **Runtime enabled state**: Profile `enabled` state is NOT persisted. Overlays always enabled. Normal profiles use `active_profile`. Triggered profiles default to disabled.
- **Value type**: `u32` for decoded PID values (sufficient for RPM, speed, temp in practice).

## Phase 1: Data Model Changes (shift-lights-lib + config)

### Step 1: Extend `LedProfile` in `tachtalk-shift-lights-lib/src/lib.rs`

Add to `LedProfile`:
- `pid: u8` — OBD2 Mode01 PID (default: `0x0C` for RPM)
- `pid_priority: PidPriority` — Fast or Slow (new enum, default: Fast)
- `profile_type: ProfileType` — Normal / Overlay / Triggered (new enum, default: Normal)
- `button_pin: u8` — GPIO for Triggered profiles (0 = disabled, default: 0)
- `auto_disable_above: Option<u32>` — decoded value threshold for auto-disable

Rename in `LedProfile`:
- `preview_rpm` → `preview_value`

### Step 2: Rename rule thresholds in `LedRule` (same file)

- `rpm_lower` → `value_lower`
- `rpm_upper` → `value_upper`

### Step 3: Update `BakedLedRule` and render functions (same file)

- Rename `rpm_lower/rpm_upper` → `value_lower/value_upper` in `BakedLedRule`
- Rename `rpm` parameter → `value` in `compute_led_state` and `bake_led_rules`
- Add `apply_rules(value: u32, rules: &BakedLedRules, timestamp_ms: u64, leds: &mut [RGB8])` — paints onto existing buffer (for overlay stacking)
- Refactor `compute_led_state` to create buffer then call `apply_rules`

### Step 4: New `tachtalk-obd2-decoder-lib` crate

New crate at `tachtalk-obd2-decoder-lib/` in workspace root:
- `decode_pid_value(pid: u8, data: &[u8]) -> Option<u32>` — decodes raw PID response bytes to a numeric value
- Built-in formulas: 0x04 (load %, A*100/255), 0x05 (coolant °C, A-40→u32 clamp), 0x0C (RPM, (A*256+B)/4), 0x0D (speed km/h, A), 0x0F (intake temp, A-40), 0x11 (throttle %, A*100/255), 0x42 (voltage *1000, (A*256+B)), 0x46 (ambient temp, A-40), 0x5C (oil temp, A-40)
- `pid_name(pid: u8) -> Option<&'static str>` — human-readable names for known PIDs (for Web UI dropdown)
- `pid_unit(pid: u8) -> Option<&'static str>` — unit string (e.g. "RPM", "°C", "km/h")
- Return `None` for unknown PIDs
- Add as dependency of `tachtalk-firmware`, `tachtalk-test-firmware`, and `tachtalk-capture-decode`

### Step 5: Update `Config` in `tachtalk-firmware/src/config.rs`

- Update `default_profiles()` to produce:
  1. "RPM" — Normal, PID 0x0C, fast, current default rules, preview_value: 3000
  2. "Coolant Warning" — Overlay, PID 0x05, slow, rule: value_lower=110, all LEDs red, blink=true
- Remove old `active_rules()` helper (needs rethinking for multi-profile rendering)
- Add `active_normal_profile()` → `Option<&LedProfile>` (by active_profile index)
- Add `active_overlays()` → iterator over Overlay profiles
- Add `triggered_profiles()` → iterator over Triggered profiles

### Step 6: Add `PidPriority` and `ProfileType` enums

In shift-lights-lib (shared):
```
PidPriority { Fast, Slow }
ProfileType { Normal, Overlay, Triggered }
```
With Serialize/Deserialize, sensible defaults.

## Phase 2: LED Task Generalization (*depends on Phase 1*)

### Step 7: Rename module `rpm_leds.rs` → `led_display.rs` and extend message types

- Rename file `rpm_leds.rs` → `led_display.rs`, update `mod rpm_leds` → `mod led_display` in `main.rs`
- `RpmTaskMessage` → `LedTaskMessage`
- `Rpm(u32)` → `PidValue { pid: u8, value: u32 }`
- Keep `ConfigChanged` and `Brightness(u8)`
- Update type alias `RpmTaskSender` → `LedTaskSender`
- Rename task function `rpm_led_task` → `led_display_task`
- Update all references in `main.rs`, `controls.rs`, `obd2.rs`, `sse_server.rs`

### Step 8: Rework LED task state in `led_display.rs`

- Replace `current_rpm: Option<u32>` with `pid_values: HashMap<u8, (u32, Instant)>` — per-PID value + last update time
- Replace single `baked_rules` with:
  - `main_baked: Option<(u8, BakedLedRules)>` — (pid, baked) for active normal profile
  - `overlay_baked: SmallVec<(u8, BakedLedRules), 4>` — [(pid, baked), ...] for overlays
- Staleness check per PID using `last_update_time`
- On `ConfigChanged`: re-read config, rebake main profile + all overlays

### Step 9: Update render pipeline in `led_display.rs`

1. Get main profile's PID value from `pid_values`. If stale → black LEDs.
2. `compute_led_state(value, main_baked, ts)` → base LED buffer
3. For each overlay: get its PID value. If stale → skip. Else `apply_rules(value, overlay_baked, ts, &mut leds)`.
4. Brightness + gamma + write (unchanged)
5. Compute blink interval as GCD across ALL active profiles' blink intervals

### Step 10: Update SSE streaming

- Send active PID values (not just RPM) — extend SSE data to include profile name + value
- Update `shared_rpm` → `shared_pid_values: Mutex<HashMap<u8, u32>>` or keep RPM separately for backward compat with `/api/rpm`

## Phase 3: OBD2 PID Management (*depends on Phase 1, parallel with Phase 2*)

### Step 11: Remove hardcoded RPM_PID

In `tachtalk-firmware/src/obd2.rs`:
- Remove `const RPM_PID: u8 = 0x0C`
- Change `PollingState::default()` to start with empty `fast_pids` instead of `[RPM_PID]`

### Step 12: Add profile PID registration

- Add concept of "pinned PIDs" to `PollingState` — PIDs required by profiles that aren't auto-demoted/removed
- New field: `pinned_pids: HashMap<Pid, PidPriority>` — profile-required PIDs with their priorities
- `set_pid_priority` respects pinned state: client can upgrade pinned slow→fast, but auto-demotion won't demote below pinned priority
- Maintenance loop skips pinned PIDs for demotion/removal

### Step 13: Add profile PID management messages

- New `DongleMessage::SetProfilePids(SmallVec<(Pid, PidPriority), 4>)` — sent when active profiles change
- dongle_task handles: updates `pinned_pids`, adds/removes PIDs from polling queues
- If a PID is unpinned and has no client demand, it enters normal demotion cycle

### Step 14: Make cache_manager forward decoded PID values

In `cache_manager_task` (obd2.rs):
- Replace hardcoded RPM extraction block with generic profile PID extraction
- On each `DongleResponse`: check if PID is needed by any active profile (read config)
- If so: call `decode_pid_value(pid, data)`, send `LedTaskMessage::PidValue { pid, value }` to LED task
- Also update shared state for SSE/API

### Step 15: Send profile PIDs on startup and profile change

- On dongle connect: compute active profile PIDs from config, send `SetProfilePids`
- On `ConfigChanged` received by cache_manager: recompute and send `SetProfilePids`
- Controls task sends `ConfigChanged` when profiles change (already does for normal cycling)

## Phase 4: Controls & GPIO Buttons (*depends on Phase 1 and 3*)

### Step 16: Add runtime profile enabled state to `State`

- Add `triggered_profile_enabled: Mutex<SmallVec<bool, 4>>` to `State` — indexed parallel to `config.profiles`, only meaningful for Triggered profiles
- Initialize all to `false` on startup
- Reset on config change from web UI

### Step 17: Extend controls task for multiple buttons

In `controls.rs`:
- Existing button cycling logic for Normal profiles stays
- For each Triggered profile with `button_pin > 0`: register GPIO interrupt
- Use shared atomic bitmask or individual atomics for ISR → task communication
- On button press for a Triggered profile:
  - Toggle `triggered_profile_enabled[i]`
  - If now enabled: add its PID to polling (via `cache_manager_tx` or `dongle_control_tx`)
  - If now disabled: remove its PID from polling
  - Send `LedTaskMessage::ConfigChanged` to LED task
  - Show preview (existing brightness preview pattern, using profile's `preview_value`)

### Step 18: Add auto-disable logic

In `cache_manager_task` (obd2.rs):
- When extracting a decoded PID value for a Triggered profile:
  - Check if `auto_disable_above` is set AND value exceeds threshold
  - If so: set `triggered_profile_enabled[i] = false`
  - Send `ConfigChanged` to LED task and update profile PIDs (unpin the PID)
  - Log the auto-disable event

## Phase 5: Web UI & API Updates (*depends on Phase 1*)

### Step 19: Update Web UI (index.html)

- Profile editor: add PID selector (dropdown of well-known PIDs with human names), PID priority toggle, profile type selector
- For Triggered profiles: button_pin field, auto_disable_above field
- Rename "RPM" labels to be PID-generic in rule editor (value_lower/value_upper)
- Show current PID value in live display (not just RPM)  
- Update SSE handler for new event format

### Step 20: Update API endpoints (web_server.rs)

- `GET /api/rpm` → keep for backward compat, but also add `GET /api/pid-values` returning all active PID values
- `POST /api/config` validation: validate button_pin conflicts, valid PID numbers
- `check_restart_needed()`: adding/removing button_pin GPIOs requires restart (GPIO reconfiguration)

## Phase 6: Test Firmware Adaptation (*parallel with others*)

### Step 21: Update tachtalk-test-firmware

- Adapt to shift-lights-lib API changes (renamed fields/functions)
- No need for full profile/overlay system — just update to compile

## Relevant Files

- `tachtalk-shift-lights-lib/src/lib.rs` — `LedProfile`, `LedRule`, `BakedLedRule`, `compute_led_state`, `bake_led_rules`. Core data model + render pipeline. Add `ProfileType`, `PidPriority` enums, `apply_rules` function.
- `tachtalk-obd2-decoder-lib/` — **New crate.** PID decoding (`decode_pid_value`), PID metadata (`pid_name`, `pid_unit`). Dependency of firmware crates and capture-decode.
- `tachtalk-firmware/src/config.rs` — `Config`, `default_profiles()`, validation. Update profile defaults, add helper methods.
- `tachtalk-firmware/src/obd2.rs` — `RPM_PID` constant (~line 27), `PollingState::default()` (~line 1148), RPM extraction in cache_manager (~line 1585), `DongleMessage` enum. Remove hardcoded RPM, add pinned PIDs, generic PID extraction.
- `tachtalk-firmware/src/rpm_leds.rs` → **renamed to `led_display.rs`** — `RpmTaskMessage`→`LedTaskMessage`, `RpmLedTaskState`, render loop. Rename module, extend for multi-PID, overlay rendering.
- `tachtalk-firmware/src/controls.rs` — `handle_button_press`, GPIO ISR setup. Add multi-button support, triggered profile toggling.
- `tachtalk-firmware/src/main.rs` — `State` struct, task spawning, channel wiring. Add runtime state, wire new messages.
- `tachtalk-firmware/src/web_server.rs` — API endpoints, config validation. Update for new fields, add pid-values API.
- `tachtalk-firmware/src/sse_server.rs` — SSE events. Extend with PID values.
- `tachtalk-firmware/src/index.html` — Web UI. Add profile type/PID/button config fields.
- `tachtalk-test-firmware/src/` — Multiple files. Adapt to lib API changes.

## Verification

1. `cargo clippy --all-targets --all-features --workspace --exclude tachtalk-firmware --exclude tachtalk-test-firmware -- -W clippy::pedantic` — host crates pass
2. `cd tachtalk-firmware && cargo clippy --all-targets --all-features -- -W clippy::pedantic` — firmware passes
3. `cd tachtalk-test-firmware && cargo clippy --all-targets --all-features -- -W clippy::pedantic` — test firmware passes
4. Unit tests in shift-lights-lib: test `apply_rules` overlay stacking, test renamed fields round-trip through serde. Unit tests in obd2-decoder-lib: test `decode_pid_value` for all supported PIDs
5. Manual test: flash firmware with default config → RPM display works identically to before (no regression)
6. Manual test: coolant overlay activates when coolant temp PID exceeds threshold
7. Manual test: wire a pit limiter button → toggle profile → speed LEDs display → auto-disable on threshold
8. Web UI: verify profile editor shows new fields, config saves/loads correctly
9. Verify backward compat: `/api/rpm` endpoint still returns RPM when RPM profile active

## Further Considerations

1. ~~**Triggered profile LED preview**: When a triggered profile is toggled on, briefly show its `preview_value` on LEDs (1s, same pattern as brightness preview). This gives the driver visual confirmation the profile activated.~~ **Done** — implemented in `handle_triggered_button()`. Sends `PidValue` + `Brightness` to trigger the preview timer. Duration is configurable via `preview_duration_ms`.
