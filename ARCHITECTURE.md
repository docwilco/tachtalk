# TachTalk Architecture

## System Overview

```
┌─────────────┐         ┌──────────────┐         ┌─────────────┐
│ RaceChroно  │ <-----> │  ESP32-S3    │ <-----> │  Wi-Fi OBD2 │
│     App     │  WiFi   │  TachTalk    │  WiFi   │   Dongle    │
└─────────────┘         └──────────────┘         └─────────────┘
                               │
                               │
                               ▼
                        ┌──────────────┐
                        │  WS2812B LED │
                        │    Strip     │
                        └──────────────┘
```

## Operating Modes

### Access Point Mode (Setup)
On first boot or when no WiFi is configured, TachTalk creates a WiFi hotspot:
- SSID: `TachTalk-XXXX` (XXXX derived from MAC address, customizable)
- IP: `192.168.71.1`
- Captive portal redirects to configuration page
- Configure dongle WiFi credentials via Web UI

### Client Mode (Normal Operation)
After WiFi is configured, TachTalk connects to the dongle's network:
- Connects to configured SSID (default: "V-LINK")
- DHCP or static IP as configured
- mDNS advertises as `tachtalk.local`
- Proxies OBD2 traffic and controls LEDs

## Component Details

### Source Files

| File | Lines | Description |
|------|-------|-------------|
| `main.rs` | ~630 | Entry point, WiFi management, task spawning |
| `obd2.rs` | ~800 | OBD2 proxy, ELM327 emulation, RPM extraction |
| `web_server.rs` | ~530 | HTTP server, REST API, configuration endpoints |
| `config.rs` | ~330 | Configuration structures, NVS persistence |
| `sse_server.rs` | ~180 | Server-Sent Events for real-time Web UI updates |
| `dns.rs` | ~180 | Captive portal DNS server for AP mode |
| `cpu_stats.rs` | ~120 | CPU usage monitoring |
| `leds.rs` | ~75 | WS2812B LED control via RMT peripheral |
| `watchdog.rs` | ~65 | Task watchdog management |
| `thread_util.rs` | ~45 | Thread spawning utilities |

### Library Crates

| Crate | Description |
|-------|-------------|
| `tachtalk-elm327-lib` | ELM327 command parsing and response generation |
| `tachtalk-shift-lights-lib` | Threshold configuration types and LED logic |

### Main Components

1. **WiFi Connection Manager** (`src/main.rs`)
   - Manages AP and STA modes
   - Handles reconnection logic
   - Static IP or DHCP configuration

2. **OBD2 Proxy** (`src/obd2.rs`)
   - Listens on configurable port (default: 35000) for client connections
   - Connects to OBD2 dongle (default: 192.168.0.10:35000)
   - Extracts RPM data from OBD2 responses
   - Polls for RPM when idle
   - ELM327 AT command emulation

3. **LED Controller** (`src/leds.rs`)
   - Controls WS2812B LED strip via RMT peripheral
   - Updates LEDs based on current RPM and thresholds
   - Supports per-threshold blink with configurable rate
   - Configurable GPIO pin (default: GPIO48)
   - Brightness control (0-255)

4. **Web Server** (`src/web_server.rs`)
   - Serves configuration UI on port 80
   - RESTful API for configuration management
   - Real-time status via SSE
   - WiFi scanning endpoint

5. **SSE Server** (`src/sse_server.rs`)
   - Server-Sent Events on configurable port
   - Streams RPM, connection status, debug info
   - Powers real-time Web UI updates

6. **DNS Server** (`src/dns.rs`)
   - Active only in AP mode
   - Captive portal: responds to all queries with device IP
   - Enables automatic redirect to configuration page

7. **Configuration** (`src/config.rs`)
   - NVS-backed persistent storage
   - WiFi, IP, OBD2, LED, threshold settings
   - JSON serialization for Web UI

## Data Flow

### Request Proxying
1. RaceChroнo sends OBD2 request → ESP32 (port 35000)
2. ESP32 forwards request → OBD2 dongle
3. Dongle responds with OBD2 data → ESP32
4. ESP32 extracts RPM from response
5. ESP32 updates LED strip based on RPM
6. ESP32 forwards response → RaceChroнo

### Idle Polling
1. When no requests received for a timeout period:
2. ESP32 sends RPM request → OBD2 dongle
3. Dongle responds with RPM data
4. ESP32 updates LED strip

## Threshold Configuration

Each threshold defines:
- **Name**: Human-readable label
- **RPM**: Minimum RPM to activate
- **Start LED / End LED**: LED range to light (0-indexed)
- **Color**: RGB color
- **Blink**: Whether to blink at this threshold
- **Blink ms**: Blink interval in milliseconds

The highest matching threshold (by RPM) is active. Thresholds are evaluated in order, so the last matching threshold wins.

## Network Configuration

### Default Settings
- **Dongle SSID**: V-LINK
- **Dongle IP**: 192.168.0.10
- **Dongle Port**: 35000
- **Listen Port**: 35000
- **AP IP**: 192.168.71.1
- **AP SSID**: TachTalk-XXXX (auto-generated from MAC)

### Static IP Defaults (when not using DHCP)
- **IP**: 192.168.0.20
- **Gateway**: 192.168.0.1
- **Subnet**: 255.255.255.0

## OBD2 Protocol

### RPM Request
```
Request:  "010C\r"
Response: "41 0C XX XX\r"
```

### RPM Calculation
```
RPM = ((A * 256) + B) / 4
where A and B are the two hex bytes in response
```

## Hardware Setup

### Pinout
- GPIO48: WS2812B data line (configurable via Web UI)
- Power: 5V for LED strip, 3.3V for ESP32

### LED Strip Connection
```
ESP32 GPIO ───> LED Strip Data In
GND ──────────> LED Strip GND
5V ───────────> LED Strip VCC
```

## Building and Flashing

See main [README.md](README.md) for detailed build instructions.

### Quick Start
```bash
cd tachtalk-firmware
cargo build --release
cargo run --release
```

## Troubleshooting

### LEDs not working
- Check GPIO pin assignment in Web UI (System Settings)
- Verify LED strip power supply
- Check data line connection

### Can't connect to dongle
- Verify dongle IP in Web UI (OBD2 Configuration)
- Check WiFi connectivity (Connection Status shows SSID/IP)
- Ensure dongle is powered on

### RaceChroнo can't connect
- Check ESP32 IP address in Web UI or serial output
- Verify port 35000 is configured (OBD2 Configuration)
- Ensure devices are on the same network

### Can't access Web UI
- In AP mode: connect to TachTalk-XXXX, go to 192.168.71.1
- In client mode: use device IP or tachtalk.local

## Future Enhancements

- [ ] Multi-zone LED support
- [ ] Alternative display modes (progress bar, etc.)
- [ ] Support for additional OBD2 parameters
- [ ] Bluetooth support for configuration
- [ ] Over-the-air (OTA) updates
- [x] NVS storage for persistent configuration
- [x] mDNS/Bonjour for easy discovery (tachtalk.local)
