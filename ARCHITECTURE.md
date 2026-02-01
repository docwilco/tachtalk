# TachTalk Architecture

## System Overview

```
┌─────────────┐         ┌──────────────┐         ┌─────────────┐
│ RaceChroно  │ <-----> │  ESP32-S3    │ <-----> │  Vgate iCar │
│     App     │  WiFi   │  TachTalk    │  WiFi   │  2 Dongle   │
└─────────────┘         └──────────────┘         └─────────────┘
                               │
                               │
                               ▼
                        ┌──────────────┐
                        │  WS2812B LED │
                        │    Strip     │
                        └──────────────┘
```

## Component Details

### Main Components

1. **OBD2 Proxy** (`src/obd2.rs`)
   - Listens on port 35000 for connections from RaceChroнo
   - Forwards requests to Vgate iCar 2 dongle at 192.168.0.10:35000
   - Extracts RPM data from OBD2 responses
   - Polls for RPM at 10Hz when idle

2. **LED Controller** (`src/leds.rs`)
   - Controls WS2812B LED strip via RMT peripheral
   - Updates LEDs based on current RPM and thresholds
   - Handles blinking at high RPM
   - Default pin: GPIO48

3. **Web Server** (`src/web_server.rs`)
   - Serves configuration UI on port 80
   - RESTful API for configuration management
   - GET/POST endpoints for config updates

4. **Configuration** (`src/config.rs`)
   - Manages RPM thresholds, colors, and LED counts
   - Serializable configuration structure
   - Default values for initial setup

## Data Flow

### Request Proxying
1. RaceChroнo sends OBD2 request → ESP32
2. ESP32 forwards request → Vgate iCar 2
3. Vgate responds with OBD2 data → ESP32
4. ESP32 extracts RPM from response
5. ESP32 updates LED strip based on RPM
6. ESP32 forwards response → RaceChroнo

### Idle Polling
1. When no requests received for >100ms:
2. ESP32 sends "010C\r" (RPM request) → Vgate
3. Vgate responds with RPM data
4. ESP32 updates LED strip
5. Repeat at 10Hz (every 100ms)

## LED Display Logic

```
RPM Range        │ LEDs Lit │ Color
─────────────────┼──────────┼────────
0 - 2999         │ None     │ Off
3000 - 3999      │ 2        │ Green
4000 - 4999      │ 4        │ Yellow
5000 - 5999      │ 6        │ Red
6000+            │ All      │ Blinking
```

## Configuration Options

### Thresholds
- RPM value
- RGB color (0-255 per channel)
- Number of LEDs to light up

### Blink Configuration
- Blink RPM threshold
- Blink rate: 250ms on/off (4Hz)

### Global Settings
- Total number of LEDs in strip

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
- GPIO48: WS2812B data line (RMT Channel 0)
- Power: 5V for LED strip, 3.3V for ESP32

### LED Strip Connection
```
ESP32 GPIO48 ──> LED Strip Data In
GND ──────────> LED Strip GND
5V ───────────> LED Strip VCC
```

### Network Configuration
- ESP32: DHCP client on your WiFi network
- Vgate: Should be at 192.168.0.10
- RaceChroнo: Configure to connect to ESP32 IP on port 35000

## Building and Flashing

See main README.md for detailed build instructions.

### Quick Start
```bash
export WIFI_SSID="your_network"
export WIFI_PASSWORD="your_password"
cargo build --release
cargo run --release
```

## Troubleshooting

### LEDs not working
- Check GPIO pin assignment in `src/main.rs`
- Verify LED strip power supply
- Check data line connection

### Can't connect to dongle
- Verify dongle IP (should be 192.168.0.10)
- Check WiFi connectivity
- Ensure dongle is powered on

### RaceChroнo can't connect
- Check ESP32 IP address in serial output
- Verify port 35000 is accessible
- Ensure WiFi network allows device-to-device communication

## Future Enhancements

- [ ] NVS storage for persistent configuration
- [ ] Multi-zone LED support
- [ ] Alternative display modes (progress bar, etc.)
- [ ] Support for additional OBD2 parameters
- [ ] Bluetooth support for configuration
- [ ] mDNS/Bonjour for easy discovery
