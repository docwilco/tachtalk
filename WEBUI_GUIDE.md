# Web UI Configuration Guide

## Accessing the Web UI

1. Connect your device to the same WiFi network as the ESP32-S3
2. Find the IP address of the ESP32 (check serial monitor output during boot)
3. Open a web browser and navigate to: `http://<esp32-ip-address>`

Example: `http://192.168.1.100`

## Configuration Interface

### Main Screen

The web UI displays:
- Current configuration status
- List of RPM thresholds
- Blink configuration
- Total LED count setting
- Save/Reload buttons

### Configuring RPM Thresholds

Each threshold has three parameters:

#### 1. RPM Value
- The minimum RPM to activate this threshold
- Enter a whole number (e.g., 3000, 4000, 5000)
- Thresholds should be in ascending order for best results

#### 2. Color
- Click the color picker to select an RGB color
- Colors can be:
  - Green (0, 255, 0) for low RPM
  - Yellow (255, 255, 0) for mid RPM
  - Red (255, 0, 0) for high RPM
  - Or any custom color you prefer

#### 3. Number of LEDs
- How many LEDs to light up at this threshold
- Should generally increase with higher thresholds
- Cannot exceed the total number of LEDs

### Adding a Threshold

1. Click the "Add Threshold" button
2. A new threshold will appear with default values
3. Adjust the RPM, color, and LED count as needed

### Removing a Threshold

1. Locate the threshold you want to remove
2. Click the "Remove" button for that threshold
3. The threshold will be deleted immediately

### Blink Configuration

The blink feature makes all LEDs flash rapidly when RPM exceeds a certain value.

- **Blink RPM**: The RPM at which blinking starts
- Typically set above your highest threshold
- Blink rate is 4Hz (250ms on/off)

### Total LEDs Setting

- Enter the total number of LEDs in your strip
- This determines the maximum number of LEDs that can be lit
- Common values: 8, 16, 30, 60

## Saving Configuration

1. Make all desired changes to thresholds, colors, and settings
2. Click the "Save Configuration" button
3. A success message will appear if saved correctly
4. Changes take effect immediately

## Reloading Configuration

- Click "Reload" to refresh the UI with the current device configuration
- Useful if you've made changes but want to revert to the saved state
- Also helpful if multiple people are configuring the device

## Example Configurations

### Conservative Racing Setup
```
Threshold 1: 2500 RPM, Green, 2 LEDs
Threshold 2: 3500 RPM, Yellow, 4 LEDs
Threshold 3: 4500 RPM, Red, 6 LEDs
Blink: 5500 RPM
Total LEDs: 8
```

### Aggressive Racing Setup
```
Threshold 1: 4000 RPM, Green, 2 LEDs
Threshold 2: 5000 RPM, Yellow, 4 LEDs
Threshold 3: 6000 RPM, Red, 8 LEDs
Blink: 7000 RPM
Total LEDs: 8
```

### Street Driving Setup
```
Threshold 1: 2000 RPM, Blue, 2 LEDs
Threshold 2: 3000 RPM, Green, 4 LEDs
Threshold 3: 4000 RPM, Yellow, 6 LEDs
Blink: 5000 RPM
Total LEDs: 8
```

### Progressive Bar Effect
```
Threshold 1: 2000 RPM, Green, 2 LEDs
Threshold 2: 3000 RPM, Green, 4 LEDs
Threshold 3: 4000 RPM, Yellow, 6 LEDs
Threshold 4: 5000 RPM, Yellow, 8 LEDs
Threshold 5: 6000 RPM, Red, 10 LEDs
Blink: 7000 RPM
Total LEDs: 12
```

## Tips and Best Practices

### Threshold Spacing
- Space thresholds evenly for smooth transitions
- Typical spacing: 500-1000 RPM apart
- Adjust based on your engine's power band

### Color Choices
- Use green for safe/optimal RPM range
- Yellow for approaching shift point
- Red for shift now
- Keep colors distinct and easy to see

### LED Count Progression
- Increase LED count with each threshold
- Makes it easy to see RPM at a glance
- Avoid large gaps in LED count between thresholds

### Blink Setting
- Set 500-1000 RPM above your highest threshold
- Indicates over-rev or urgent shift needed
- Very visible even in peripheral vision

### Testing
1. Start with conservative values
2. Test in a safe environment
3. Adjust based on your preferences and vehicle
4. Save when you find a configuration you like

## Troubleshooting

### Changes Not Saving
- Check browser console for errors (F12)
- Verify network connection to ESP32
- Try reloading the page
- Check serial monitor for server errors

### Colors Look Wrong
- Verify LED strip is WS2812B compatible
- Check power supply voltage (should be 5V)
- Adjust brightness if colors seem dim

### LEDs Not Responding to Configuration
- Ensure configuration was saved successfully
- Check that RPM data is being received
- Verify LED strip is properly connected to GPIO48
- Review serial monitor for error messages

## Advanced Usage

### Multiple Configurations
Currently, only one configuration can be stored. To switch between setups:
1. Note down your current configuration
2. Update with new values
3. Save
4. To switch back, manually re-enter previous values

### Integration with RaceChroнo
1. Configure TachTalk as desired
2. Set RaceChroнo OBD2 connection to ESP32 IP:35000
3. LEDs will respond to real-time RPM data
4. Thresholds can be adjusted during breaks without reconnecting

### Using Without RaceChroнo
- TachTalk will automatically poll for RPM at 10Hz
- Connect Vgate dongle to your vehicle's OBD2 port
- Power on ESP32-S3
- LEDs will display RPM without any app connection required
