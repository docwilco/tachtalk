# TachTalk

Firmware for proxying OBD2 requests between RaceChrono and a Wi-Fi OBD2 dongle (e.g., Vgate iCar 2), extracting RPM data to display on WS2812B LED shift lights.

## 🚀 Quick Start

New to TachTalk? Start here: **[QUICKSTART.md](QUICKSTART.md)**

Get up and running in 15 minutes with step-by-step instructions!

## Features

- **OBD2 Proxy**: Proxies requests between RaceChrono app and Wi-Fi OBD2 dongle
- **RPM Extraction**: Extracts RPM data from OBD2 requests/responses
- **Shift Lights**: Controls WS2812B LED strip based on configurable RPM thresholds
- **Automatic Polling**: Polls for RPM when no active requests
- **Web UI**: Full configuration interface with real-time status
- **Always-On Access Point**: TachTalk-XXXX hotspot always available for direct access
- **mDNS Discovery**: Access via `http://tachtalk.local` on the dongle network
- **NVS Storage**: Configuration persists across reboots
- **ESP32-S3**: Optimized for ESP32-S3 hardware

## Hardware Requirements

- ESP32-S3 development board
- WS2812B LED strip
- Wi-Fi OBD2 dongle (e.g., Vgate iCar 2)

## Pin Configuration

- GPIO48: WS2812B LED data line (configurable via Web UI)

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
   cargo install ldproxy espflash
   ```

### Building and Flashing

```bash
cd tachtalk-firmware
cargo run --release
```

Or manually:
```bash
cargo build --release
espflash flash target/xtensa-esp32s3-espidf/release/tachtalk
```

## First Boot Setup

1. **Power on** the ESP32-S3 with TachTalk firmware
2. **Connect to the WiFi hotspot** `TachTalk-XXXX` (where XXXX is derived from the device MAC address)
3. **Open a browser** to `http://10.15.25.1` (captive portal should redirect automatically)
4. **Configure WiFi** credentials for your OBD2 dongle's network (default: "V-LINK")
5. **Save & Connect** — the device will reboot and connect to the configured network
6. **Access the Web UI** via the device's AP (10.15.25.1), or on the dongle network via IP or `http://tachtalk.local`

**Note**: The TachTalk-XXXX access point remains active even after WiFi is configured. This ensures you can always access the Web UI directly, which is necessary because some OBD2 dongles don't allow connected devices to communicate with each other.

## Usage

### Standalone Mode (without RaceChrono)
1. Plug the OBD2 dongle into your vehicle's OBD2 port
2. Power on the ESP32-S3
3. The device connects to the dongle's WiFi network
4. LEDs display RPM automatically via polling

### Proxy Mode (with RaceChrono)
1. Connect your phone to the TachTalk-XXXX WiFi access point
2. Configure RaceChrono to connect to `10.15.25.1` on port 35000
3. The device proxies OBD2 requests and extracts RPM for the shift lights
4. Both RaceChrono and shift lights work simultaneously

## Web UI Configuration

Access the configuration interface at `http://tachtalk.local` or the device IP. The Web UI allows you to configure:

- **RPM Thresholds**: Multiple thresholds with name, RPM value, LED range, color, and blink settings
- **Brightness**: Global LED brightness control (0-255)
- **WiFi Settings**: SSID, password, DHCP or static IP configuration
- **Access Point Settings**: Custom AP SSID and password
- **OBD2 Settings**: Dongle IP, port, listen port, timeout
- **System Settings**: Log level, total LEDs, LED GPIO pin
- **Connection Status**: Real-time view of OBD2 dongle and client connections

## Default Thresholds

| Name   | RPM   | Color  | Blink |
|--------|-------|--------|-------|
| Off    | 0     | Black  | No    |
| Blue   | 1000  | Blue   | No    |
| Green  | 1500  | Green  | No    |
| Yellow | 2000  | Yellow | No    |
| Red    | 2500  | Red    | No    |
| Off    | 3000  | Black  | No    |
| Shift  | 3000  | Blue   | Yes   |

## Documentation

- **[QUICKSTART.md](QUICKSTART.md)** - Get started in 15 minutes
- **[WIRING_GUIDE.md](WIRING_GUIDE.md)** - Hardware wiring and connections
- **[WEBUI_GUIDE.md](WEBUI_GUIDE.md)** - Configuration interface guide
- **[ARCHITECTURE.md](ARCHITECTURE.md)** - System architecture and design

## License

See LICENSE file for details.
