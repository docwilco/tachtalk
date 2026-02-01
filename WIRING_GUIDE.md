# Hardware Wiring Guide

## Components Required

### Main Components
- ESP32-S3 Development Board (e.g., ESP32-S3-DevKitC-1)
- WS2812B LED Strip (8+ LEDs recommended)
- Vgate iCar 2 Wi-Fi OBD2 Dongle
- 5V Power Supply (2A+ for LED strip)
- USB-C cable for ESP32-S3 programming

### Optional Components
- Level shifter (3.3V to 5V) for reliable LED data signal
- Capacitor (1000μF) for LED power supply smoothing
- Resistor (330-470Ω) for LED data line protection

## Pin Connections

### ESP32-S3 to WS2812B LED Strip

```
ESP32-S3          WS2812B Strip
──────────────────────────────
GPIO48    ──────> DIN (Data In)
GND       ──────> GND
5V*       ──────> +5V

* Note: 5V should come from external power supply, not ESP32
```

### Recommended Wiring with Level Shifter

```
ESP32-S3          Level Shifter         WS2812B Strip
─────────────────────────────────────────────────────
GPIO48    ───────> LV Input
3.3V      ───────> LV VCC
GND       ───────> GND    ───────────> GND
                  HV VCC  <────┐
                  HV Output ───┼──────> DIN
5V Supply ──────────────────────┴─────> +5V
```

## Detailed Wiring Diagram

```
                    ┌─────────────────┐
                    │   ESP32-S3      │
                    │   DevKit        │
                    │                 │
                    │  GPIO48  ───────┼──┐
                    │  GND     ───────┼──┼────┐
                    │  3.3V           │  │    │
                    └─────────────────┘  │    │
                                         │    │
                    ┌────────────────────┤    │
                    │  330Ω Resistor     │    │
                    └────────────────────┤    │
                                         │    │
                    ┌────────────────────┴────┴────┐
                    │      WS2812B LED Strip       │
                    │   DIN ──── GND ──── +5V      │
                    │    │        │         │      │
                    └────┼────────┼─────────┼──────┘
                         │        │         │
                         │        │         │
                    ┌────┴────────┴─────────┴──────┐
                    │    5V Power Supply (2A+)     │
                    │         GND ──── +5V         │
                    └──────────────────────────────┘
```

## Power Considerations

### LED Strip Power Requirements

Calculate power needed:
```
Power = Number of LEDs × 60mA (max per LED)
Example: 8 LEDs × 60mA = 480mA
Recommendation: Use 2A supply for headroom
```

### Power Supply Options

1. **External 5V Power Supply**
   - Recommended for >8 LEDs
   - Use quality regulated supply
   - Connect grounds together

2. **USB Power**
   - Only for very small strips (<8 LEDs)
   - ESP32 + LEDs from same USB source
   - May cause brownouts with more LEDs

3. **Separate Supplies**
   ```
   USB Power ──> ESP32-S3
   5V Supply ──> LED Strip + Level Shifter
   
   Important: Connect GND between all supplies!
   ```

## Level Shifter Options

### Why Use a Level Shifter?

ESP32 outputs 3.3V logic, but WS2812B expects 5V logic.
While it often works without shifting, a level shifter ensures reliability.

### Level Shifter Types

1. **74HCT125** (Recommended)
   - Cheap and reliable
   - No extra power needed
   - One gate per data line

2. **TXS0108E**
   - Bidirectional (not needed here)
   - 8 channels
   - Automatic direction sensing

3. **Simple Transistor Circuit**
   - 2N3904 or similar NPN transistor
   - Two resistors (4.7kΩ, 10kΩ)
   - DIY solution

### Simple 74HCT125 Wiring

```
ESP32 GPIO48 ──> 74HCT125 Input (Pin 2)
3.3V ──────────> 74HCT125 Vcc (Pin 14)
GND ───────────> 74HCT125 GND (Pin 7)
GND ───────────> 74HCT125 Enable (Pin 1, active low)
5V ────────────> (Connect to WS2812B power)
                 74HCT125 Output (Pin 3) ──> WS2812B DIN
```

## Protection Components

### Data Line Resistor
```
GPIO48 ──[ 330Ω ]──> LED Strip DIN
```
- Protects GPIO from current spikes
- Reduces signal reflections
- 330-470Ω works well

### Power Supply Capacitor
```
LED Strip 5V ──┬──> Strip +5V
               │
          [ 1000μF ]
               │
LED Strip GND ──┴──> Strip GND
```
- Place close to LED strip
- Smooths power delivery
- Prevents voltage drops
- Use 1000μF 10V or higher

## Step-by-Step Wiring

### Basic Setup (Minimum)

1. **Connect LED Strip GND**
   - LED strip GND to ESP32 GND
   - Also connect to power supply GND

2. **Connect LED Strip Power**
   - LED strip +5V to power supply +5V
   - Do NOT connect to ESP32 5V pin

3. **Connect LED Strip Data**
   - LED strip DIN to ESP32 GPIO48
   - Add 330Ω resistor in series (recommended)

4. **Power ESP32**
   - USB-C cable to ESP32
   - Or use VIN pin with regulated 5V

### Enhanced Setup (Recommended)

1. **Install Level Shifter**
   - LV side to ESP32 (3.3V)
   - HV side to LED strip (5V)

2. **Add Capacitor**
   - 1000μF across LED strip power
   - Positive to +5V, Negative to GND
   - Place close to strip

3. **Add Resistor**
   - 330Ω between level shifter output and LED DIN

4. **Verify Connections**
   - All grounds connected together
   - No shorts between power and ground
   - Data line properly isolated

## Testing

### Before Powering On

1. **Check Continuity**
   - Verify ground connections
   - Check for shorts

2. **Verify Voltages**
   - ESP32 powered separately first
   - Measure LED strip voltage (should be 5V)

3. **Data Line**
   - Check GPIO48 connection
   - Verify level shifter if used

### First Power On

1. **Power ESP32 Only**
   - Check boot messages
   - Verify WiFi connection

2. **Power LED Strip**
   - All grounds connected
   - Watch for any issues

3. **Test LEDs**
   - Access web UI
   - Set low RPM threshold
   - LEDs should light up

## Troubleshooting

### LEDs Don't Light Up
- [ ] Check power supply voltage (5V)
- [ ] Verify GND connections
- [ ] Check data line connection to GPIO48
- [ ] Try without level shifter first
- [ ] Check LED strip orientation (DIN vs DOUT)

### First LED Works, Others Don't
- [ ] Check power supply capacity
- [ ] Add capacitor to power supply
- [ ] Verify strip isn't damaged
- [ ] Check connections along strip

### Flickering or Random Colors
- [ ] Add level shifter if not present
- [ ] Add data line resistor
- [ ] Improve power supply quality
- [ ] Shorten data wire length
- [ ] Add capacitor to power

### LEDs Work But Wrong Colors
- [ ] Check GRB vs RGB order in code
- [ ] Verify LED type (WS2812B confirmed?)
- [ ] Check power supply voltage

## Safety Notes

⚠️ **Important Safety Information**

- Never connect LED strip 5V to ESP32 3.3V pin
- Always connect all grounds together
- Use appropriate wire gauge for LED current
- Don't exceed power supply ratings
- Keep connections away from moving parts
- Use proper fusing if installed in vehicle
- Double-check polarity before connecting power
- Start with low LED count for testing

## Vehicle Installation

When installing in a vehicle:

1. **Power Source**
   - Tap into 12V switched power (ignition on)
   - Use 12V to 5V buck converter (3A+)
   - Add inline fuse (5A)

2. **Mounting**
   - Secure ESP32 away from heat
   - Mount LEDs in visible location
   - Protect from moisture
   - Allow for ventilation

3. **Wiring**
   - Use automotive-grade wire
   - Properly route and secure cables
   - Protect wiring from sharp edges
   - Use cable ties and loom

4. **OBD2 Connection**
   - Keep Vgate dongle plugged in
   - Ensure good OBD2 port connection
   - Check dongle LED indicators

## Example Bill of Materials

| Component | Specification | Quantity | Notes |
|-----------|---------------|----------|-------|
| ESP32-S3 DevKit | Any variant | 1 | With USB-C |
| WS2812B LED Strip | 5V, 60 LEDs/m | 1 | 8+ LEDs |
| Power Supply | 5V 2A+ | 1 | Regulated |
| Level Shifter | 74HCT125 | 1 | Optional but recommended |
| Resistor | 330Ω | 1 | 1/4W |
| Capacitor | 1000μF 10V | 1 | Electrolytic |
| Jumper Wires | Various | 1 set | For prototyping |
| Wire | 22AWG | 1m | Solid core for breadboard |

## Additional Resources

- ESP32-S3 Pinout: Check your specific dev board
- WS2812B Datasheet: For detailed timing specs
- Adafruit NeoPixel Guide: Excellent wiring information
- ESP32 GPIO Matrix: For alternative pin selection
