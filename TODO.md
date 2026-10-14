# TODO

## Web Server
- [x] Handle larger HTTP requests in config POST endpoints in a streaming manner (currently limited to 8KB buffer in main firmware, 4KB in test firmware). OTA upload already streams correctly.

## Security
- [ ] Review session management — current `SessionStore` uses in-memory `Vec<[u8; 16]>` with FIFO eviction (max 5). Consider: session expiry/TTL, binding sessions to client IP, clearing all sessions on password change, and whether `MAX_SESSIONS=5` is appropriate for the use case.

## Features
- [x] Multi-zone LED support / patterns
- [x] Alternative display modes (progress bar, etc.)
- [x] Support for additional OBD2 parameters (coolant temp, speed, etc.)
- [x] Over-the-air (OTA) updates
- [x] Data logging capability
- [ ] Bluetooth configuration option
- [x] Port capture dissection display from test firmware to main firmware Web UI (main firmware has capture status/controls but not record-level content viewing)

## OBD2 Proxy
- [ ] Pipelined dongle queries — port the pipelining strategy from the removed test firmware's Mode 4 (git history at v0.3.1, `tachtalk-test-firmware/src/obd2.rs`) to the main firmware's dongle task for higher query throughput
- [ ] Properly parse client commands into mode, PIDs, and optional response count — currently `wire_command_to_pid` only extracts the first PID and ignores both multi-PID requests (e.g., `010C0D05`) and the trailing response count hint (e.g., `010C2`, which `strip_response_count` drops from the cache key). Needs a `ParsedClientCommand { mode, pids, max_responses }` struct so that: (a) multi-PID requests return combined responses, (b) response count limits how many ECU lines are returned.
- [ ] Support 29-bit extended CAN IDs — currently `ReplyLine::ecu` in `tachtalk-elm327-lib` is `u16` (11-bit only), and its reply parser and `ATH1` header writer hardcode a 3 hex char CAN ID. Needs widening to `u32` and detection from protocol or line length.

## Performance
- [x] Pre-compute LED color gradients per threshold once on config change, not every render cycle (`interpolate_color` is called per-LED per-frame in `compute_led_state`)
- [ ] Rewrite `ClientSession`'s reply parsing (`parse_lines` in `tachtalk-elm327-lib`) to operate on `&[u8]` directly: it still goes through `String::from_utf8_lossy` and `.split('\r')` before decoding hex pairs.

## Infrastructure
- [x] Upgrade to ESP-IDF 5.4
- [x] Switch PCNT encoder to interrupt-driven using limit events (4x quadrature, ISR-based)
- [ ] Investigate moving more allocations to PSRAM (N16R8 has 8MB). Currently only the capture buffer is large enough to auto-place in PSRAM (`CONFIG_SPIRAM_MALLOC_ALWAYSINTERNAL=4096`). Consider web server request buffers, JSON serialization buffers, WiFi scan results, etc.
- [x] Use deku crate (or similar) for binary capture file encoding/decoding

## Completed
- [x] NVS storage for persistent configuration
- [x] mDNS/Bonjour for easy discovery (tachtalk.local)
- [x] Access Point mode for initial setup
- [x] Captive portal for seamless configuration
- [x] Server-Sent Events for real-time Web UI
