# TachTalk

Firmware for proxying OBD2 requests between RaceChroно and a Vgate iCar 2 Wi-Fi OBD2 dongle, extracting RPM data to display on WS2812B LED shift lights.

## Features

- **OBD2 Proxy**: Proxies requests between RaceChroнo app and Vgate iCar 2 Wi-Fi dongle
- **RPM Extraction**: Extracts RPM data from OBD2 requests/responses
- **Shift Lights**: Controls WS2812B LED strip based on configurable RPM thresholds
- **10Hz Polling**: Automatically polls for RPM at 10Hz when no active requests
- **Web UI**: Configure RPM thresholds, colors, LED counts, and blink threshold
- **ESP32-S3**: Optimized for ESP32-S3 hardware

## Hardware Requirements

- ESP32-S3 development board
- WS2812B LED strip
- Vgate iCar 2 Wi-Fi OBD2 dongle
- WiFi network

## Pin Configuration

- GPIO48: WS2812B LED data line (configurable in `src/main.rs`)

## Setup

### Prerequisites

1. Install Rust: https://rustup.rs/
2. Install ESP-IDF tools:
   ```bash
   cargo install espup
   espup install
   . ~/export-esp.sh
   ```
3. Install additional tools:
   ```bash
   cargo install ldproxy
   cargo install espflash
   ```

### Build Configuration

1. Copy the environment file and configure WiFi:
   ```bash
   cp .env.example .env
   # Edit .env with your WiFi credentials
   ```

2. Set environment variables:
   ```bash
   export WIFI_SSID="your_ssid"
   export WIFI_PASSWORD="your_password"
   ```

### Building

```bash
cargo build --release
```

### Flashing

```bash
cargo run --release
```

Or manually:
```bash
espflash flash target/xtensa-esp32s3-espidf/release/tachtalk
```

## Usage

1. Power on the ESP32-S3 with TachTalk firmware
2. The device will connect to your configured WiFi network
3. Access the web UI at `http://<device-ip>` (check serial output for IP address)
4. Configure your RPM thresholds, colors, and LED settings
5. Point RaceChroнo to connect to the ESP32-S3 IP on port 35000
6. The Vgate iCar 2 dongle should be at 192.168.0.10:35000 (default)

## Configuration

The web UI allows you to configure:

- **RPM Thresholds**: Multiple thresholds with different colors
- **Colors**: RGB color for each threshold
- **Number of LEDs**: How many LEDs to light up at each threshold
- **Blink RPM**: RPM at which all LEDs start blinking
- **Total LEDs**: Total number of LEDs in the strip

## Default Configuration

- Threshold 1: 3000 RPM, Green (0,255,0), 2 LEDs
- Threshold 2: 4000 RPM, Yellow (255,255,0), 4 LEDs
- Threshold 3: 5000 RPM, Red (255,0,0), 6 LEDs
- Blink: 6000 RPM
- Total LEDs: 8

## License

See LICENSE file for details.
