# Quick Start Guide

Get TachTalk up and running in 15 minutes!

## Prerequisites

- ESP32-S3 development board
- WS2812B LED strip (1+ LEDs)
- Wi-Fi OBD2 dongle (e.g., Vgate iCar 2)
- Computer with Rust installed

## Step 1: Install Rust Tools (5 minutes)

```bash
# Install Rust if not already installed
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Install ESP-IDF tools
cargo install espup
espup install

# Source the ESP environment
. ~/export-esp.sh

# Install flashing tools
cargo install ldproxy espflash
```

## Step 2: Build and Flash (5 minutes)

```bash
# Clone the repository (if you haven't already)
git clone https://github.com/docwilco/tachtalk.git
cd tachtalk/tachtalk-firmware

# Build and flash to ESP32-S3
cargo run --release
```

The firmware will:
- Compile (first time takes 5-10 minutes)
- Flash to your ESP32-S3
- Start monitoring serial output

## Step 3: Wire the Hardware (2 minutes)

Connect your WS2812B LED strip:

```
ESP32-S3 GPIO48 ──> LED Strip DIN
ESP32-S3 GND    ──> LED Strip GND
5V Power Supply ──> LED Strip +5V
```

**Important:** 
- Connect all grounds together
- LED strip needs external 5V power (not from ESP32)
- See [WIRING_GUIDE.md](WIRING_GUIDE.md) for detailed instructions

## Step 4: Initial Configuration (3 minutes)

1. **Connect to the TachTalk WiFi hotspot**
   - Look for `TachTalk-XXXX` in your WiFi networks
   - Connect (no password by default)

2. **Open the configuration page**
   - A captive portal should redirect you automatically
   - If not, go to: `http://10.15.25.1`

3. **Configure WiFi to connect to your OBD2 dongle**
   - Enter the dongle's SSID (default: "V-LINK" for Vgate iCar 2)
   - Enter password if required
   - Click "Save & Connect"
   - The device will reboot and connect to the dongle network

4. **Reconnect to access the Web UI**
   - Connect your computer/phone to the TachTalk-XXXX WiFi access point
   - Access TachTalk at `http://10.15.25.1`

## Step 5: Configure Shift Lights (2 minutes)

In the Web UI:
- Adjust RPM thresholds for your vehicle
- Set colors for each threshold
- Configure LED ranges (start/end LED)
- Enable blink for shift warning
- Set brightness level
- Click "Save Configuration"

## Step 6: Connect to Vehicle

### Without RaceChrono (Standalone Mode)
1. Plug OBD2 dongle into vehicle OBD2 port
2. Turn on ignition
3. Power on ESP32-S3
4. Device connects to dongle WiFi automatically
5. LEDs will start showing RPM!

### With RaceChrono (Proxy Mode)
1. Connect your phone to the TachTalk-XXXX WiFi access point
2. Configure RaceChrono OBD2 connection:
   - Type: WiFi/Network
   - IP: 10.15.25.1 (configurable)
   - Port: 35000
3. Connect in RaceChrono
4. LEDs show RPM based on your thresholds while RaceChrono logs data

## Verification Checklist

- [ ] ESP32-S3 boots successfully (check serial output)
- [ ] TachTalk-XXXX WiFi hotspot appears
- [ ] Web UI accessible at 10.15.25.1 via AP
- [ ] Device connects to dongle WiFi after configuration
- [ ] Web UI accessible via tachtalk.local on dongle network (may not work on all dongles)
- [ ] LED strip powers on
- [ ] LEDs respond to RPM changes
- [ ] Configuration changes save successfully

## Troubleshooting

### Build Fails
```bash
# Make sure environment is sourced
. ~/export-esp.sh
```

### Can't Find TachTalk WiFi
- Check that ESP32 is powered
- Look for `TachTalk-` prefix in WiFi list
- Check serial output for boot messages

### WiFi Won't Connect to Dongle
- Verify SSID spelling (case-sensitive)
- Check password is correct
- Ensure dongle is powered on
- 2.4GHz only (ESP32 doesn't support 5GHz)

### LEDs Don't Light
- Verify wiring (see Step 3)
- Check LED strip power supply (5V, sufficient current)
- Verify GPIO pin in Web UI (System Settings → LED GPIO Pin)
- Check serial output for LED controller errors

### Can't Access Web UI
- **Via AP (recommended)**: Connect to TachTalk-XXXX, go to 10.15.25.1
- **Via dongle network**: Use device IP from serial output, or tachtalk.local
- Note: Some OBD2 dongles don't allow devices to communicate; use the AP instead
- Try pinging the device

### LEDs Flicker or Wrong Colors
- Add 330Ω resistor on data line
- Use level shifter (3.3V to 5V) for longer runs
- Add 1000μF capacitor to LED power
- See [WIRING_GUIDE.md](WIRING_GUIDE.md) for details

## Next Steps

- Read [WEBUI_GUIDE.md](WEBUI_GUIDE.md) for detailed configuration options
- Check [WIRING_GUIDE.md](WIRING_GUIDE.md) for enhanced wiring with level shifter
- Review [ARCHITECTURE.md](ARCHITECTURE.md) to understand how it works

## Example Configurations

### First-Time Setup (Testing)
Start with default thresholds and adjust based on your vehicle:
- Threshold at 1000 RPM: Blue (idle indicator)
- Threshold at 1500 RPM: Green
- Threshold at 2000 RPM: Yellow
- Threshold at 2500 RPM: Red
- Threshold at 3000 RPM: Blink (shift warning)

### Racing Setup
For track use, adjust RPM values to match your engine:
- Green: 80% of redline
- Yellow: 90% of redline
- Red: 95% of redline
- Blink: At or just before redline

## Getting Help

If you encounter issues:

1. **Check Serial Output**: Most issues show up here
   ```bash
   cd tachtalk-firmware
   espflash monitor
   ```

2. **Check Web UI Status**: Connection Status section shows network and OBD2 state

3. **Verify Hardware**: 
   - All connections secure
   - Correct voltages (5V for LEDs, 3.3V logic)
   - No shorts or reversed polarity

4. **Review Documentation**:
   - [README.md](README.md) - Overview
   - [WIRING_GUIDE.md](WIRING_GUIDE.md) - Hardware setup
   - [WEBUI_GUIDE.md](WEBUI_GUIDE.md) - Configuration
   - [ARCHITECTURE.md](ARCHITECTURE.md) - How it works

## Success!

Once everything is working:
- LEDs respond to engine RPM
- Web UI shows real-time RPM and connection status
- Configuration changes apply immediately
- Settings persist across reboots

Enjoy your new shift lights! 🏁

## Tips for Best Experience

1. **Test Before Racing**: Verify all thresholds in a safe environment
2. **Mount Securely**: Ensure ESP32 and LEDs won't move while driving
3. **Protect from Heat**: Keep ESP32 away from engine heat
4. **Use Quality Wire**: Automotive-grade wire for permanent installation
5. **Add Fusing**: Use appropriate fuse for vehicle installation
6. **Adjust Brightness**: Lower brightness for night driving to avoid distraction
