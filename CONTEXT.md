# TachTalk

ESP32-S3 firmware and companion tools that turn OBD2 engine data from a WiFi
ELM327 dongle into shift-light LED displays, while proxying the dongle to
other client apps.

## Language

### Wire protocol

**Wire protocol**:
The ELM327 serial dialect spoken over TCP: AT commands, hex OBD requests,
CR-framed lines, the `>` prompt. The subject of `tachtalk-elm327-lib`.
_Avoid_: ELM protocol, dongle protocol

**Server side**:
The role that answers something speaking to us as if we were an ELM327:
framing, echo, the AT dialect, response formatting, the prompt.
_Avoid_: proxy side, emulation

**Client side**:
The role that speaks to an ELM327, real or emulated (a dongle, the TachTalk
proxy, the mock server): command encoding, prompt-terminated receive,
multi-ECU response parsing, PID data-length learning.
_Avoid_: dongle side

**Session**:
One role of the wire protocol (`ServerSession`, `ClientSession`) over a
stream it owns: it reads and writes the bytes and keeps the protocol state;
the program decides what to answer and when. What a session keeps about its
peer is named for the peer: `ServerSession` holds `ClientState`,
`ClientSession` holds `ServerState`.
_Avoid_: session machine, handler, driver, codec, DongleClient, action

**Adapter**:
A program's loop around a session: the stream's settings, timeouts,
reconnects, and business rules (what to answer, when to poll).
_Avoid_: wrapper, driver

**Repeat**:
The ELM327 datasheet's repeat mechanism: a bare carriage return re-runs the
last command. The bare `1` found in pre-2026-10 history was an invention and
is retired.

### Vehicle data

**PID**:
An OBD2 parameter id (e.g. 0x0C engine RPM). Fast PIDs are polled every
cycle, slow PIDs on a ratio.

**Dongle**:
The WiFi ELM327 adapter plugged into the car's OBD2 port, reached over TCP.
_Avoid_: interface, scanner

### LED display

**Rev strip**:
The main WS2812B strip showing shift-light patterns driven by profiles.
_Avoid_: main LEDs, RPM LEDs

**Status chain**:
The three WS2812B status LEDs (WIFI, OBD, CLIENT) on their own data pin.

**Profile**:
A named set of LED rules bound to a PID; Normal, Overlay, or Triggered by
type. Overlays stack on the active Normal profile.

**Baking**:
Precomputing a profile's rules against the strip geometry
(`bake_led_rules`); rendering requires baked rules and re-baking follows any
config change.
