# Quick Start Guide

Get TachTalk up and running in 15 minutes!

## Prerequisites

- ESP32-S3 development board
- WS2812B LED strip (8+ LEDs)
- WiFi network
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

## Step 2: Configure WiFi (1 minute)

Create a `.env` file or set environment variables:

```bash
export WIFI_SSID="YourNetworkName"
export WIFI_PASSWORD="YourPassword"
```

## Step 3: Build and Flash (5 minutes)

```bash
# Clone the repository (if you haven't already)
git clone https://github.com/docwilco/tachtalk.git
cd tachtalk

# Build and flash to ESP32-S3
cargo run --release
```

The firmware will:
- Compile (first time takes 5-10 minutes)
- Flash to your ESP32-S3
- Start monitoring serial output

## Step 4: Wire the Hardware (2 minutes)

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

## Step 5: Configure (2 minutes)

1. Find your ESP32's IP address in the serial monitor output:
   ```
   WiFi IP Info: IpInfo { ip: 192.168.1.100, ... }
   ```

2. Open web browser to: `http://192.168.1.100`

3. Configure your shift light thresholds:
   - Add/remove thresholds
   - Set RPM values
   - Choose colors
   - Set number of LEDs per threshold
   - Configure blink RPM

4. Click "Save Configuration"

## Step 6: Connect to Vehicle

### Without RaceChroнo (Standalone Mode)
1. Plug Vgate iCar 2 into vehicle OBD2 port
2. Ensure Vgate is at IP 192.168.0.10
3. Turn on ignition
4. LEDs will start showing RPM automatically!

### With RaceChroнo (Proxy Mode)
1. Configure RaceChroнo OBD2 connection:
   - Type: WiFi/Network
   - IP: Your ESP32 IP (e.g., 192.168.1.100)
   - Port: 35000
2. Connect in RaceChroнo
3. LEDs show RPM based on your thresholds

## Verification Checklist

- [ ] ESP32-S3 boots successfully
- [ ] WiFi connects (check serial output)
- [ ] Web UI accessible at ESP32 IP address
- [ ] LED strip powers on
- [ ] LEDs respond to RPM changes
- [ ] Configuration changes save successfully

## Troubleshooting

### Build Fails
```bash
# Make sure environment is sourced
. ~/export-esp.sh

# Verify WiFi credentials are set
echo $WIFI_SSID
echo $WIFI_PASSWORD
```

### WiFi Won't Connect
- Check SSID and password are correct
- Ensure 2.4GHz WiFi (ESP32 doesn't support 5GHz)
- Check serial output for error messages

### LEDs Don't Light
- Verify wiring (see Step 4)
- Check LED strip power supply (5V, 2A+)
- Try accessing web UI and setting a low RPM threshold
- Check serial output for LED controller errors

### Can't Access Web UI
- Verify ESP32 IP from serial output
- Ensure computer is on same network
- Try pinging the ESP32 IP
- Check firewall settings

### LEDs Flicker or Wrong Colors
- Add 330Ω resistor on data line
- Use level shifter (3.3V to 5V)
- Add 1000μF capacitor to LED power
- See [WIRING_GUIDE.md](WIRING_GUIDE.md) for details

## Next Steps

- Read [WEBUI_GUIDE.md](WEBUI_GUIDE.md) for detailed configuration options
- Check [WIRING_GUIDE.md](WIRING_GUIDE.md) for enhanced wiring with level shifter
- Review [ARCHITECTURE.md](ARCHITECTURE.md) to understand how it works

## Example Configurations

### First-Time Setup (Recommended)
Start with these safe values:
- Threshold 1: 3000 RPM, Green, 2 LEDs
- Threshold 2: 4000 RPM, Yellow, 4 LEDs  
- Threshold 3: 5000 RPM, Red, 6 LEDs
- Blink: 6000 RPM
- Total LEDs: 8

### Racing Setup
For track use:
- Threshold 1: 4000 RPM, Green, 2 LEDs
- Threshold 2: 5500 RPM, Yellow, 5 LEDs
- Threshold 3: 6500 RPM, Red, 8 LEDs
- Blink: 7000 RPM
- Total LEDs: 8

## Getting Help

If you encounter issues:

1. **Check Serial Output**: Most issues show up here
   ```bash
   cargo run --release
   # Watch for error messages
   ```

2. **Verify Hardware**: 
   - All connections secure
   - Correct voltages (5V for LEDs, 3.3V logic)
   - No shorts or reversed polarity

3. **Review Documentation**:
   - [README.md](README.md) - Overview
   - [WIRING_GUIDE.md](WIRING_GUIDE.md) - Hardware setup
   - [WEBUI_GUIDE.md](WEBUI_GUIDE.md) - Configuration
   - [ARCHITECTURE.md](ARCHITECTURE.md) - How it works

4. **Common Issues**:
   - **Build errors**: Source ESP environment (`. ~/export-esp.sh`)
   - **WiFi errors**: Check credentials and 2.4GHz network
   - **LED issues**: Verify wiring and power supply
   - **Web UI errors**: Check IP address and network

## Success!

Once everything is working:
- LEDs should respond to engine RPM
- Web UI shows current configuration
- Configuration changes apply immediately
- System automatically polls RPM when idle

Enjoy your new shift lights! 🏁

## Tips for Best Experience

1. **Test Before Racing**: Verify all thresholds in a safe environment
2. **Mount Securely**: Ensure ESP32 and LEDs won't move while driving
3. **Protect from Heat**: Keep ESP32 away from engine heat
4. **Use Quality Wire**: Automotive-grade wire for permanent installation
5. **Add Fusing**: Use appropriate fuse for vehicle installation
6. **Document Settings**: Note your favorite configurations for different scenarios

## What's Next?

- Experiment with different color schemes
- Try multiple threshold configurations
- Fine-tune based on your engine's power band
- Share your setup with others!
