# TODO

## Web Server
- [ ] Handle larger HTTP requests in a streaming manner (currently limited to 8KB buffer)

## Features
- [ ] Multi-zone LED support / patterns
- [ ] Alternative display modes (progress bar, etc.)
- [ ] Support for additional OBD2 parameters (coolant temp, speed, etc.)
- [ ] Over-the-air (OTA) updates
- [ ] Data logging capability
- [ ] Bluetooth configuration option

## Performance
- [x] Pre-compute LED color gradients per threshold once on config change, not every render cycle (`interpolate_color` is called per-LED per-frame in `compute_led_state`)

## Infrastructure
- [ ] Upgrade to ESP-IDF 5.4
- [ ] Switch PCNT encoder to event-based using `add_watch_step()` (requires ESP-IDF 5.4)
- [ ] Investigate moving more allocations to PSRAM (N16R8 has 8MB). Currently only the capture buffer in test firmware is large enough to auto-place in PSRAM (`CONFIG_SPIRAM_MALLOC_ALWAYSINTERNAL=4096`). Consider web server request buffers, JSON serialization buffers, WiFi scan results, etc.
- [ ] Use deku crate (or similar) for binary capture file encoding/decoding

## Test Firmware (`tachtalk-test-firmware`)

Stripped-down firmware for benchmarking OBD2 query strategies.

### Goals
- Compare request rates across different ELM327 querying methods
- Determine optimal strategy for real-world dongle communication
- Capture raw traffic for protocol analysis

### Query Modes
1. **NoCount**: Send PID as-is (`010C\r`) - baseline, dongle waits for adaptive timing
2. **AlwaysOne**: Append ` 1` (`010C 1\r`) - dongle returns after 1 response
3. **AdaptiveCount**: First request without count to detect ECU count, then use that count
4. **Pipelined**: Send multiple requests before waiting, up to configurable bytes on wire
5. **RawCapture**: Pure TCP proxy with traffic recording to PSRAM

### Implementation Steps

- [x] **Mock server**: Add 200ms delay when ` 1` not present in query
- [x] **Create crate**: Copy `tachtalk-firmware/` to `tachtalk-test-firmware/`, add to workspace
- [x] **Strip hardware**: Remove `rpm_leds.rs`, `controls.rs`, LED/encoder init from `main.rs`
- [x] **Config**: Add `fast_pids`, `slow_pids`, `query_mode`, `pipeline_bytes`, `response_count_method`
- [x] **Mode 3 config**: `ResponseCountMethod` enum (`CountResponseHeaders` vs `CountLines`)
- [x] **Rework OBD2**: Modes 1-4 polling/pipelined, Mode 5 TCP proxy with capture
- [x] **Capture format**: Binary records (timestamp_ms:u32 + type:u8 + length:u16 + data)
  - Type values: 0=client→dongle, 1=dongle→client, 2=connect, 3=disconnect
  - Connect/disconnect events have length=0 (no data bytes)
- [x] **Overflow behavior**: Configurable stop/wrap, default to stop
- [x] **Mode 5 PSRAM**: Enable SPIRAM in sdkconfig, large allocations auto-placed via `CONFIG_SPIRAM_USE_MALLOC`
- [x] **Simplify WebUI**: Mode selector, PID config, pipeline bytes, capture controls, live SSE metrics
- [x] **SSE server**: Streams `TestMetrics` (reqs/sec, totals, errors, capture status)
- [x] **Web server**: Test start/stop/status endpoints, config CRUD, WiFi scan
- [x] **Capture file header**: Generate 64-byte header (magic `TachTalk`, version, record count, etc.)
- [x] **Capture endpoints**: `GET /capture` (download binary), `POST /capture/clear`, `GET /capture/status`
- [ ] **Decode capture file**: Binary crate to print human-readable summary of capture contents (timestamps, types, data)
- [ ] **On-device testing**: Flash and validate against real OBD2 dongle

### Capture File Header (64 bytes)
| Offset | Size | Field |
|--------|------|-------|
| 0 | 8 | Magic: `TachTalk` |
| 8 | 2 | Version (u16, start at 1) |
| 10 | 2 | Header size (u16, allows expansion) |
| 12 | 4 | Record count (u32) |
| 16 | 4 | Total data length (u32) |
| 20 | 8 | Capture start (u64, Unix epoch ms or 0) |
| 28 | 4 | Uptime at start (u32, ms) |
| 32 | 4 | Dongle IP (u32, network order) |
| 36 | 2 | Dongle port (u16) |
| 38 | 2 | Flags (bit0: overflow, bit1: NTP synced) |
| 40 | 16 | Firmware version (null-terminated) |
| 56 | 8 | Reserved |

### Mode Control
- Unified "Start Test" / "Stop Test" button for all modes
- Starting:
  - Modes 1-4: Connects to dongle, begins polling loop with configured mode/PIDs/ratio
  - Mode 5: Starts listening on proxy port, begins capturing (dongle connection deferred)
- Stopping:
  - Modes 1-4: Stops polling, disconnects from dongle
  - Mode 5: Stops capturing, closes proxy listener, disconnects client and dongle (if connected)
- Changing mode while running: Stops current test, user must explicitly restart with new mode
- Metrics (all modes): Displayed in WebUI via SSE, reset on start
  - Requests/sec (modes 1-4) or bytes/sec (mode 5)
  - Total requests or total bytes captured
  - Errors count
  - Uptime since test start
  - Mode 5 additional: buffer usage %, records captured, client connected status
- Mode 5 specifics:
  - Single client only; additional connection attempts rejected while client connected
  - Dongle connection established when client connects, closed when client disconnects
  - Client connects/disconnects recorded in capture buffer as events
  - Download/clear capture available only while stopped

### Default Configuration
- Fast PIDs: `010C`, `0149`
- Slow PIDs: `0105`
- Fast:slow ratio: 6:1
- Pipeline bytes: 64
- Response count method: `CountResponseHeaders`
- Capture buffer: 4MB
- Overflow: stop

## Completed
- [x] NVS storage for persistent configuration
- [x] mDNS/Bonjour for easy discovery (tachtalk.local)
- [x] Access Point mode for initial setup
- [x] Captive portal for seamless configuration
- [x] Server-Sent Events for real-time Web UI
