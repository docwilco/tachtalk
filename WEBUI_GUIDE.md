# Web UI Configuration Guide

## Accessing the Web UI

### Via TachTalk Access Point (Always Available)
The device's own access point is always active, even when connected to the dongle network:
1. Connect to the `TachTalk-XXXX` WiFi network (XXXX is derived from the device MAC)
2. A captive portal should redirect you automatically
3. If not, open a browser to: `http://10.15.25.1`

This is the recommended method because:
- Some OBD2 dongles don't allow devices to communicate with each other
- Fewer devices on the dongle WiFi leaves more processing power for OBD2 queries

### Via Dongle Network
When connected to the same network as the OBD2 dongle:
1. Connect your phone/laptop to the dongle's WiFi network
2. Access via mDNS: `http://tachtalk.local`
3. Or use the device's IP address (shown in serial output or the Web UI status)

**Note**: This may not work with all OBD2 dongles.

## Configuration Interface

### Main Screen

The web UI displays:
- Current RPM (real-time via Server-Sent Events)
- Brightness slider with live preview
- Connection status diagram
- WiFi configuration
- Access Point settings
- OBD2 configuration
- System settings
- RPM threshold configuration
- Debug information (collapsible)

### Connection Status Diagram

The visual diagram shows:
- **OBD2 Dongle**: Connection status to the Wi-Fi OBD2 dongle
- **TachTalk Device**: Central hub showing WiFi and TCP connections
- **OBD2 Clients**: Number of connected clients (e.g., RaceChrono)
- **Browser**: Your current Web UI connection

Click on nodes to see detailed network information (IP addresses, ports, signal strength).

### Brightness Control

- **Slider**: Adjust LED brightness from 0 (off) to 255 (full)
- **Save Button**: Persist brightness setting to NVS
- Changes apply immediately for preview, but require Save to persist

### WiFi Configuration

Configure connection to your OBD2 dongle's network:

- **SSID**: Network name (default: "V-LINK" for Vgate iCar 2)
- **Password**: Network password (leave empty for open networks)
- **IP Mode**: 
  - DHCP (Automatic) - recommended
  - Static IP - for fixed address configuration
- **Static IP Fields** (when Static IP selected):
  - IP Address (default: 192.168.0.20)
  - Gateway (default: 192.168.0.1)
  - Subnet Mask (default: 255.255.255.0)
  - DNS (optional)
- **Scan Networks**: Discover available WiFi networks
- **Save & Connect**: Apply settings and reboot

### Access Point Configuration

Configure the setup hotspot:

- **AP SSID**: Custom name (leave empty for auto: TachTalk-XXXX)
- **AP Password**: Secure the hotspot (leave empty for open network)

### OBD2 Configuration

Configure connection to the OBD2 dongle:

- **Dongle IP**: IP address of the OBD2 dongle (default: 192.168.0.10)
- **Dongle Port**: TCP port of the dongle (default: 35000)
- **Proxy Listen Port**: Port for RaceChrono to connect (default: 35000)
- **Timeout (ms)**: OBD2 response timeout (default: 4500ms, max: 4500ms)

### System Settings

- **Log Level**: off, error, warn, info, debug
- **Total LEDs**: Number of LEDs in your strip
- **LED GPIO Pin**: GPIO pin for WS2812B data (default: 48, requires restart)
- **Reboot Device**: Restart the ESP32-S3

### Configuring RPM Thresholds

Each threshold defines LED behavior at a specific RPM:

#### Threshold Fields
- **Name**: Human-readable label (e.g., "Green", "Shift")
- **RPM**: Minimum RPM to activate this threshold
- **Start LED**: First LED in the range (0-indexed)
- **End LED**: Last LED in the range (0-indexed)
- **Color**: RGB color picker
- **Blink**: Enable/disable blinking
- **Blink ms**: Blink interval in milliseconds

#### Threshold Logic
- Thresholds are evaluated in order
- The **last matching threshold** (by RPM) determines LED behavior
- Use multiple thresholds at the same RPM for different LED ranges

#### Managing Thresholds
- **Add Threshold**: Creates a new threshold with defaults
- **Remove**: Deletes a threshold
- **Move Up/Down**: Reorder thresholds

### Saving Configuration

1. Make all desired changes
2. Click **Save Configuration**
3. A success/error message appears
4. Changes take effect immediately
5. Settings persist across reboots (NVS storage)

### Debug Section

Collapsible section with:
- **Memory Stats**: Free heap, minimum free heap
- **AT Commands**: Log of ELM327 AT commands received
- **OBD2 PIDs**: Log of PIDs requested by clients
- **Benchmark**: Performance testing tool

### Raw Config JSON

For advanced users:
- View/edit the complete configuration as JSON
- Format JSON for readability
- Useful for backup/restore or bulk edits

## Example Configurations

### Simple Shift Light (1 LED)
```
Threshold 1: Off - 0 RPM, LED 0-0, Black, No blink
Threshold 2: Green - 5000 RPM, LED 0-0, Green, No blink  
Threshold 3: Yellow - 6000 RPM, LED 0-0, Yellow, No blink
Threshold 4: Red - 6500 RPM, LED 0-0, Red, No blink
Threshold 5: Shift - 7000 RPM, LED 0-0, Blue, Blink 200ms
```

### Progressive Bar (8 LEDs)
```
Threshold 1: Off - 0 RPM, LED 0-7, Black, No blink
Threshold 2: Low - 3000 RPM, LED 0-1, Green, No blink
Threshold 3: Mid - 4000 RPM, LED 0-3, Green, No blink
Threshold 4: High - 5000 RPM, LED 0-5, Yellow, No blink
Threshold 5: Max - 6000 RPM, LED 0-7, Red, No blink
Threshold 6: Shift - 6500 RPM, LED 0-7, Red, Blink 150ms
```

### Center-Out Pattern (8 LEDs)
```
Threshold 1: Off - 0 RPM, LED 0-7, Black, No blink
Threshold 2: Start - 3000 RPM, LED 3-4, Green, No blink
Threshold 3: Expand - 4000 RPM, LED 2-5, Yellow, No blink
Threshold 4: Full - 5000 RPM, LED 1-6, Orange, No blink
Threshold 5: Max - 6000 RPM, LED 0-7, Red, No blink
Threshold 6: Shift - 6500 RPM, LED 0-7, White, Blink 100ms
```

## Tips and Best Practices

### Threshold Design
- Start with an "Off" threshold at 0 RPM to clear LEDs at idle
- Space thresholds evenly across your usable RPM range
- Set blink threshold 500-1000 RPM before redline
- Use distinct colors for quick recognition

### LED Range Configuration
- `start_led` and `end_led` are 0-indexed
- For a single LED, use the same value for both
- Ranges are inclusive: 0-2 lights LEDs 0, 1, and 2
- Ensure `total_leds` matches your actual strip length

### Color Choices
- Green: Safe/optimal RPM range
- Yellow/Orange: Approaching shift point
- Red: Shift now
- Blue/White: Over-rev warning (blinking)

### Blink Settings
- 100-200ms: Very fast, urgent
- 250-500ms: Moderate, noticeable
- 500-1000ms: Slow, subtle

## Troubleshooting

### Can't Access Web UI
- **Via AP (recommended)**: Connect to TachTalk-XXXX, go to 10.15.25.1
- **Via dongle network**: Check device IP in serial output, or try tachtalk.local
- Note: Some OBD2 dongles don't allow devices to communicate; use the AP instead
- Clear browser cache or try incognito mode

### Changes Not Saving
- Check the status message after clicking Save
- Look for error messages in the browser console (F12)
- Verify NVS storage is working (check serial output)

### Real-time Updates Not Working
- SSE connection may have dropped; refresh the page
- Check browser console for connection errors
- Ensure you're not behind a proxy that blocks SSE

### LEDs Not Responding
- Verify GPIO pin setting matches your wiring
- Check total_leds matches your strip
- Verify power supply is adequate
- Check serial output for LED controller errors

## API Endpoints

For programmatic access:

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/` | GET | Web UI HTML |
| `/api/config` | GET | Current configuration (JSON) |
| `/api/config` | POST | Update configuration |
| `/api/status` | GET | Connection status (WiFi, dongle, clients) |
| `/api/wifi/scan` | GET | Scan for WiFi networks |
| `/api/wifi` | POST | Connect to WiFi network |
| `/api/reboot` | POST | Reboot the device |
| `/api/benchmark` | POST | Run OBD2 benchmark |
| `/events` | GET | SSE stream for real-time updates |
