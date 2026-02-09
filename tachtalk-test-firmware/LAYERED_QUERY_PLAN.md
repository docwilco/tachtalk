# Plan: Layered Query Architecture for Test Firmware

## Summary

Add three toggles (`use_multi_pid`, `use_repeat`, `use_framing`) and a configurable `repeat_string` sent with the Start command alongside `query_mode`. Implement a layered architecture for polling modes where each layer adds one behavior. Remove `ResponseCountMethod` from config — ECU counting is determined by the framing layer.

## Layer Stack

Each layer is a concrete struct holding the next layer as a field. No traits needed yet.

```
QueryBuilder  →  Count  →  Framing  →  Repeat  →  Base
```

| Layer | Responsibility |
|-------|---------------|
| **Query Builder** | Two modes: **single** (one PID per command, round-robin) or **multi** (all PIDs combined into one command). Drives the 6:1 fast:slow polling loop. Passes `pid_count` down to Count layer. Validates shared service byte at start (multi mode). |
| **Count** | Three modes: `NoCount` (pass through), `AlwaysOne` (append `" 1"`), `AdaptiveCount` (learn ECU count, append learned count). Receives `pid_count` from Query Builder. |
| **Framing** | On init: sends `ATH1` or `ATH0`. On response: parses `(Option<CAN_ID>, obd_data)`. Verifies PCI byte matches actual data length, logs warning on mismatch. When off: returns `(None, raw_line)`. |
| **Repeat** | Tracks last command. When enabled and same command, sends configured repeat string (empty = bare CR per ELM327 spec, `"1"` = common WiFi dongle convention). Handles `?` fallback and marks `supports_repeat = Some(false)`. When disabled, passes through. |
| **Base** | Sends bytes with `\r`, reads until `>` prompt, returns raw response buffer. |

### Command path (top → down)

1. **Query Builder** builds `"010C49"` (multi-PID) or `"010C"` (single), sets `pid_count`
2. **Count** appends a learned count (e.g., `" 2"`), `" 1"` (always), or nothing
3. **Framing** is transparent on command path
4. **Repeat** substitutes the configured repeat string if same command, or passes through
5. **Base** sends bytes, reads response

### Response path (down → up)

1. **Base** returns raw bytes
2. **Repeat** passes through (remembers what was sent)
3. **Framing** parses each line into `(Option<CAN_ID>, obd_data)`, verifies PCI byte length
4. **Count** learns ECU count:
   - Framing on: count unique CAN IDs
   - Framing off, single-PID: count response lines
   - Framing off, multi-PID: walk each line using `pid_data_lengths` to count PID responses, sum across all lines, divide by `pid_count`
5. **Query Builder** receives response, updates metrics

## Response Parsing Details

### Framing format (ATH1 enabled)

Each response line: `{3-char CAN ID} {PCI byte} {OBD data}`

Example: `7E8 06 41 05 7B 0C 1A F8`
- CAN ID: `7E8` (11-bit ECU address)
- PCI byte: `06` (6 payload bytes follow)
- OBD data: `41 05 7B 0C 1A F8`

The PCI byte is verified against the actual data length. On mismatch, log a warning but continue processing.

### Multi-PID response variations

Request: `01 05 0C` (2 PIDs to potentially multiple ECUs)

**1. Each ECU responds in a single line (most common)**
```
7E8 06 41 05 7B 0C 1A F8
7E9 06 41 05 7B 0C 1A F8
```

**2. Each ECU splits across two lines**
```
7E8 03 41 05 7B
7E8 04 41 0C 1A F8
7E9 03 41 05 7B
7E9 04 41 0C 1A F8
```

**3. Mixed — one ECU fits in one line, the other splits**
```
7E8 06 41 05 7B 0C 1A F8
7E9 03 41 05 7B
7E9 04 41 0C 1A F8
```

**4. Interleaved (non-deterministic line order)**
```
7E8 03 41 05 7B
7E9 03 41 05 7B
7E8 04 41 0C 1A F8
7E9 04 41 0C 1A F8
```

### ECU counting strategy

- **Framing on**: count unique CAN IDs — reliable regardless of split/interleave.
- **Framing off, single-PID**: count `"41"` occurrences or non-empty response lines — each line is one ECU's response.
- **Framing off, multi-PID**: walk each response line using `pid_data_lengths` (from `CountLayer`) to count how many PID responses it contains — each PID response is `41 {PID} {data_bytes}` where the data byte count is known. Sum across all lines, divide by the number of queried PIDs. This works regardless of whether ECUs pack all PIDs on one line or split across multiple lines.

## PID Data Lengths

Multi-PID support requires knowing the response data byte count for each PID. This is needed for:
- **Response parsing**: walking through a multi-PID response line to extract individual PID values.
- **ECU counting** (framing off): counting PID responses per line to derive ECU count.
- **Validation**: verifying response completeness.

Add a const lookup table covering all standard Mode 01 PIDs. OBD-II Mode 01 PIDs have fixed response sizes defined by SAE J1979.

```rust
/// Response data byte count for each Mode 01 PID (excludes service byte and PID byte).
/// Indexed by PID byte. `0` = unknown/unsupported PID.
/// Source: SAE J1979 / ISO 15031-5.
#[rustfmt::skip]
const MODE01_PID_DATA_LENGTHS: [u8; 256] = {
    let mut t = [0u8; 256];
    // 0x00-0x20: PIDs supported bitmasks and basic engine data
    t[0x00] = 4; // PIDs supported [01-20]
    t[0x01] = 4; // Monitor status since DTCs cleared
    t[0x02] = 2; // Freeze DTC
    t[0x03] = 2; // Fuel system status
    t[0x04] = 1; // Calculated engine load
    t[0x05] = 1; // Engine coolant temperature
    t[0x06] = 1; // Short term fuel trim — Bank 1
    t[0x07] = 1; // Long term fuel trim — Bank 1
    t[0x08] = 1; // Short term fuel trim — Bank 2
    t[0x09] = 1; // Long term fuel trim — Bank 2
    t[0x0A] = 1; // Fuel pressure
    t[0x0B] = 1; // Intake manifold absolute pressure
    t[0x0C] = 2; // Engine RPM
    t[0x0D] = 1; // Vehicle speed
    t[0x0E] = 1; // Timing advance
    t[0x0F] = 1; // Intake air temperature
    t[0x10] = 2; // MAF air flow rate
    t[0x11] = 1; // Throttle position
    t[0x12] = 1; // Commanded secondary air status
    t[0x13] = 1; // O2 sensors present (2 banks)
    t[0x14] = 2; // O2 sensor 1 — voltage & trim
    t[0x15] = 2; // O2 sensor 2
    t[0x16] = 2; // O2 sensor 3
    t[0x17] = 2; // O2 sensor 4
    t[0x18] = 2; // O2 sensor 5
    t[0x19] = 2; // O2 sensor 6
    t[0x1A] = 2; // O2 sensor 7
    t[0x1B] = 2; // O2 sensor 8
    t[0x1C] = 1; // OBD standards this vehicle conforms to
    t[0x1D] = 1; // O2 sensors present (4 banks)
    t[0x1E] = 1; // Auxiliary input status
    t[0x1F] = 2; // Run time since engine start
    // 0x20-0x40
    t[0x20] = 4; // PIDs supported [21-40]
    t[0x21] = 2; // Distance traveled with MIL on
    t[0x22] = 2; // Fuel rail pressure (relative to manifold vacuum)
    t[0x23] = 2; // Fuel rail gauge pressure (diesel/GDI)
    t[0x24] = 4; // O2 sensor 1 — equiv ratio & voltage
    t[0x25] = 4; // O2 sensor 2
    t[0x26] = 4; // O2 sensor 3
    t[0x27] = 4; // O2 sensor 4
    t[0x28] = 4; // O2 sensor 5
    t[0x29] = 4; // O2 sensor 6
    t[0x2A] = 4; // O2 sensor 7
    t[0x2B] = 4; // O2 sensor 8
    t[0x2C] = 1; // Commanded EGR
    t[0x2D] = 1; // EGR error
    t[0x2E] = 1; // Commanded evaporative purge
    t[0x2F] = 1; // Fuel tank level input
    t[0x30] = 1; // Warm-ups since codes cleared
    t[0x31] = 2; // Distance traveled since codes cleared
    t[0x32] = 2; // Evap system vapor pressure
    t[0x33] = 1; // Absolute barometric pressure
    t[0x34] = 4; // O2 sensor 1 — equiv ratio & current
    t[0x35] = 4; // O2 sensor 2
    t[0x36] = 4; // O2 sensor 3
    t[0x37] = 4; // O2 sensor 4
    t[0x38] = 4; // O2 sensor 5
    t[0x39] = 4; // O2 sensor 6
    t[0x3A] = 4; // O2 sensor 7
    t[0x3B] = 4; // O2 sensor 8
    t[0x3C] = 2; // Catalyst temperature: Bank 1, Sensor 1
    t[0x3D] = 2; // Catalyst temperature: Bank 2, Sensor 1
    t[0x3E] = 2; // Catalyst temperature: Bank 1, Sensor 2
    t[0x3F] = 2; // Catalyst temperature: Bank 2, Sensor 2
    // 0x40-0x60
    t[0x40] = 4; // PIDs supported [41-60]
    t[0x41] = 4; // Monitor status this drive cycle
    t[0x42] = 2; // Control module voltage
    t[0x43] = 2; // Absolute load value
    t[0x44] = 2; // Fuel-air commanded equivalence ratio
    t[0x45] = 1; // Relative throttle position
    t[0x46] = 1; // Ambient air temperature
    t[0x47] = 1; // Absolute throttle position B
    t[0x48] = 1; // Absolute throttle position C
    t[0x49] = 1; // Accelerator pedal position D
    t[0x4A] = 1; // Accelerator pedal position E
    t[0x4B] = 1; // Accelerator pedal position F
    t[0x4C] = 1; // Commanded throttle actuator
    t[0x4D] = 2; // Time run with MIL on
    t[0x4E] = 2; // Time since trouble codes cleared
    t[0x4F] = 4; // Max values (equiv ratio, O2 voltage, O2 current, intake pressure)
    t[0x50] = 4; // Max air flow rate from MAF sensor
    t[0x51] = 1; // Fuel type
    t[0x52] = 1; // Ethanol fuel %
    t[0x53] = 2; // Absolute evap system vapor pressure
    t[0x54] = 2; // Evap system vapor pressure
    t[0x55] = 2; // Short term secondary O2 trim — Bank 1 & 3
    t[0x56] = 2; // Long term secondary O2 trim — Bank 1 & 3
    t[0x57] = 2; // Short term secondary O2 trim — Bank 2 & 4
    t[0x58] = 2; // Long term secondary O2 trim — Bank 2 & 4
    t[0x59] = 2; // Fuel rail absolute pressure
    t[0x5A] = 1; // Relative accelerator pedal position
    t[0x5B] = 1; // Hybrid battery pack remaining life
    t[0x5C] = 1; // Engine oil temperature
    t[0x5D] = 2; // Fuel injection timing
    t[0x5E] = 2; // Engine fuel rate
    t[0x5F] = 1; // Emission requirements
    // 0x60-0x80
    t[0x60] = 4; // PIDs supported [61-80]
    t[0x61] = 1; // Driver's demand engine — percent torque
    t[0x62] = 1; // Actual engine — percent torque
    t[0x63] = 2; // Engine reference torque
    t[0x64] = 5; // Engine percent torque data
    t[0x65] = 2; // Auxiliary input / output supported
    // 0x80-0xA0
    t[0x80] = 4; // PIDs supported [81-A0]
    // 0xA0-0xC0
    t[0xA0] = 4; // PIDs supported [A1-C0]
    // 0xC0-0xE0
    t[0xC0] = 4; // PIDs supported [C1-E0]
    t
};

/// Look up the response data byte count for a Mode 01 PID.
/// Returns 0 for unknown/unsupported PIDs.
const fn pid_data_length(pid: u8) -> u8 {
    MODE01_PID_DATA_LENGTHS[pid as usize]
}
```

Returns `0` for unknown PIDs, which is treated as an error in multi-PID parsing (unknown PID → cannot walk the response). PIDs above 0x65 that aren't "supported" bitmask PIDs are left at 0 — extend as needed.

## Implementation Steps

### Step 1: Define `StartOptions` in `src/config.rs`

Add a new struct:
```rust
#[derive(Debug, Clone, Deserialize)]
pub struct StartOptions {
    #[serde(default)]
    pub query_mode: QueryMode,
    #[serde(default)]
    pub use_multi_pid: bool,
    #[serde(default)]
    pub use_repeat: bool,
    #[serde(default)]
    pub repeat_string: String,  // e.g., "" for bare CR (ELM327 spec), "1" for WiFi dongle convention
    #[serde(default)]
    pub use_framing: bool,
}
```

Add `pid_data_length()` lookup and `MODE01_PID_DATA_LENGTHS` const table (see PID Data Lengths section above).

Remove `ResponseCountMethod` enum and the `response_count_method` field from `TestConfig`.

**Rejected combinations** (return HTTP 400 from the start handler):
- `use_multi_pid` + PIDs with different service bytes.

### Step 2: Update `TestControlMessage` and start handler in `src/web_server.rs`

- Change `TestControlMessage::Start(QueryMode)` to `TestControlMessage::Start(StartOptions)`.
- Update the `/api/test/start` handler to deserialize `StartOptions` from the request body.
- Remove `response_count_method` from config API serialization/deserialization.

### Step 3: Update UI in `src/index.html`

- Add three checkboxes (`useMultiPid`, `useRepeat`, `useFraming`) grouped with the Mode `<select>` dropdown.
- Add a text input for `repeatString` next to the `useRepeat` checkbox (default empty, shown only when repeat is enabled). Empty means bare CR per ELM327 spec; `1` is the common WiFi dongle convention.
- Update `startTest()` to include `use_multi_pid`, `use_repeat`, `repeat_string`, `use_framing` in the JSON body.
- In `updateModeUI()`, hide all three checkboxes when mode is `pipelined` or `raw_capture`.
- Remove the `ResponseCountMethod` dropdown from the config form.
- Remove `response_count_method` from `populateForm()` and config save logic.

### Step 4: Implement Base layer in `src/obd2.rs`

Extract the existing `execute_command` function into a `Base` struct:

```rust
struct Base {
    stream: TcpStream,
    timeout: Duration,
}

impl Base {
    fn execute(&mut self, command: &[u8]) -> Result<Obd2Buffer, String> {
        // existing send + read-until-`>` logic
        // Obd2Buffer = SmallVec<[u8; 32]> — stack-allocated for typical responses
    }
}
```

`Base` also provides a `connect_and_init` constructor that performs TCP connect + general AT init (`ATZ`, `ATE0`, `ATS0`, `ATL0`) before returning. These general init commands are sent through `Base::execute` directly — no other layer is involved. The `ATH` command is **not** sent here; that is the Framing layer's responsibility.

### Step 5: Implement Repeat layer in `src/obd2.rs`

```rust
struct RepeatLayer {
    base: Base,
    enabled: bool,
    repeat_string: SmallVec<[u8; 2]>,  // e.g., b"" for bare CR, b"1" for WiFi dongle convention
    last_command: Option<SmallVec<[u8; 16]>>,  // last command bytes sent
    supports_repeat: Option<bool>,
}
```

- `RepeatLayer::execute(&mut self, command: &[u8]) -> Result<Obd2Buffer, String>`
- When `enabled` and command matches `last_command` and `supports_repeat != Some(false)`: send `self.repeat_string` via `self.base` (Base appends `\r`, so empty string produces bare CR). On `?` in response, set `supports_repeat = Some(false)`, retry with full command.
- When disabled or different command: pass through to `self.base`, update `last_command`.

### Step 6: Implement Framing layer in `src/obd2.rs`

```rust
struct FramingLayer {
    repeat: RepeatLayer,
    enabled: bool,
}

struct ParsedLine {
    ecu_id: Option<SmallVec<[u8; 3]>>,  // e.g., b"7E8" — 3 ASCII bytes, stack-allocated
    data: SmallVec<[u8; 8]>,            // OBD data bytes after PCI — fits single CAN frame
}
```

- `FramingLayer::init(&mut self)`: sends `ATH1` if enabled, `ATH0` if disabled, through the layer stack. This is the **only** layer that sends AT commands after the general init. Called after Base's `connect_and_init` and layer stack construction.
- `FramingLayer::execute(&mut self, command: &[u8]) -> Result<Obd2Buffer, String>` — delegates to `self.repeat`, returns raw response.
- `FramingLayer::parse_response(&self, response: &[u8]) -> Vec<ParsedLine>` — splits response into lines, for each:
  - **Enabled**: extracts 3-char CAN ID, PCI byte, remaining data. Verifies PCI byte matches actual data length; logs warning on mismatch.
  - **Disabled**: returns `ParsedLine { ecu_id: None, data: raw_line_bytes }`.

### Step 7: Implement Count layer in `src/obd2.rs`

```rust
struct CountLayer {
    framing: FramingLayer,
    mode: QueryMode,  // NoCount, AlwaysOne, or AdaptiveCount
    response_counts: HashMap<SmallVec<[u8; 16]>, u8>,  // command → learned ECU count
    pid_count: usize,       // set by Query Builder before each call
    pid_data_lengths: SmallVec<[u8; 6]>,  // response data byte count for each queried PID, in query order
}
```

- `CountLayer::execute(&mut self, command: &str) -> Result<(Obd2Buffer, Vec<ParsedLine>), String>`:
  - `NoCount`: pass command through unchanged.
  - `AlwaysOne`: append `" 1"` to command.
  - `AdaptiveCount`: if count learned for this command string, append it. Otherwise send bare, then learn from response:
    - Framing on: count unique `ecu_id` values from `parse_response()`.
    - Framing off, single-PID: count `"41"` occurrences in the response.
    - Framing off, multi-PID: walk each response line using `self.pid_data_lengths` to count PID responses per line (each is `41 {PID} {N data bytes}`), sum across all lines, divide by `pid_count`.

### Step 8: Implement Query Builder layer in `src/obd2.rs`

```rust
struct QueryBuilder {
    count: CountLayer,
    fast_pids: SmallVec<[String; 4]>,   // e.g., ["010C", "0149"]
    slow_pids: SmallVec<[String; 4]>,   // e.g., ["0105"]
    use_multi_pid: bool,
}
```

- `QueryBuilder::new(...)`: validates all PIDs share the same service byte (first 2 chars). Returns error if not. Looks up `pid_data_length()` for each PID and stores in `CountLayer::pid_data_lengths`. Returns error for PIDs with data length 0 (unknown) when `use_multi_pid` is true.
- `QueryBuilder::build_command(&self, pids: &[String]) -> (SmallVec<[u8; 16]>, usize)`:
  - **Single mode**: takes one PID, returns `("010C", 1)`.
  - **Multi mode**: concatenates PID bytes after shared service byte, returns `("010C49", 2)`.
- Drives the polling loop:
  - **Single mode**: existing 6:1 round-robin over individual PIDs.
  - **Multi mode**: fast cycle sends one combined fast-PIDs command; slow cycle sends one combined fast+slow command. 6:1 ratio = 6 fast-only calls to 1 all-PIDs call.
- Sets `self.count.pid_count` before each call.

### Step 9: Refactor `run_polling_test` in `src/obd2.rs`

Replace the existing `run_polling_test` with:
1. Create `Base` via `Base::connect_and_init(...)` — TCP connect + general AT init (`ATZ`, `ATE0`, `ATS0`, `ATL0`).
2. Wrap in layer stack from `StartOptions` + config snapshot:
   ```
   QueryBuilder { count: CountLayer { framing: FramingLayer { repeat: RepeatLayer { base } } } }
   ```
3. Call `framing.init()` to send the appropriate `ATH` command.
4. Delegate to `QueryBuilder::run_polling_loop(...)` which contains the fast/slow cycling and metrics updates.
5. `run_pipeline_test` and `run_capture_test` remain separate, using `Base` directly.
