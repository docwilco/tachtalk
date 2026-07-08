//! Status LED controller and background task
//!
//! Drives a chain of three WS2812B status LEDs on a single data GPIO
//! (ordered outward from the MCU): **WIFI**, **OBD**, **CLIENT**.
//! - **WIFI**: STA state — dim white (no SSID configured), blue slow blink
//!   (searching/associating), blue fast blink (awaiting DHCP), blue solid
//!   (connected); cyan pulse overlay while an AP client is associated
//! - **OBD**: dongle state — red blink (no connection), amber blink
//!   (initializing), green solid (ready) with dim-pulse on request activity
//! - **CLIENT**: OBD2 proxy clients — off (none), solid when connected
//!   (green 1, cyan 2, blue 3+) with dim-pulse on request activity
//!
//! The task is fully event-driven via an `mpsc` channel — producers send
//! [`StatusLedMessage`] variants on state transitions and per-request
//! activity. Blink and chase phases are derived from a wallclock so the task
//! only wakes at phase boundaries. An OTA override displays a purple chase.
//!
//! A plain red boot LED (separate GPIO) blinks along with the boot animation.

use crate::error::Result;
use crate::obd2::DongleTcpState;
use crate::ota::OtaState;
use crate::wifi::WifiStaState;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use esp_idf_hal::gpio::{AnyOutputPin, Output, PinDriver};
use esp_idf_hal::peripheral::Peripheral;
use esp_idf_hal::rmt::config::TransmitConfig;
use esp_idf_hal::rmt::{RmtChannel, TxRmtDriver};
use log::{info, warn};
use smart_leds::{brightness, gamma, SmartLedsWrite, RGB8};
use ws2812_esp32_rmt_driver::Ws2812Esp32Rmt;

// ---------------------------------------------------------------------------
// LED chain layout and colors
// ---------------------------------------------------------------------------

/// Number of WS2812B status LEDs in the chain
pub const STATUS_LED_COUNT: usize = 3;

/// Chain index of the WIFI status LED (closest to the MCU)
const WIFI_LED: usize = 0;
/// Chain index of the OBD status LED
const OBD_LED: usize = 1;
/// Chain index of the CLIENT status LED (furthest from the MCU)
const CLIENT_LED: usize = 2;

/// WIFI LED: STA activity (blink/solid per state)
const WIFI_COLOR: RGB8 = RGB8::new(0, 0, 255);
/// WIFI LED: no STA SSID configured (AP-only operation)
const NOT_CONFIGURED_COLOR: RGB8 = RGB8::new(32, 32, 32);
/// WIFI LED: overlay pulse while an AP client is associated
const AP_CLIENT_COLOR: RGB8 = RGB8::new(0, 255, 255);
/// OBD LED: dongle TCP connect / ELM327 init in progress
const OBD_INIT_COLOR: RGB8 = RGB8::new(255, 160, 0);
/// OBD LED: dongle ready
const OBD_READY_COLOR: RGB8 = RGB8::new(0, 255, 0);
/// OBD LED: no dongle connection
const OBD_ERROR_COLOR: RGB8 = RGB8::new(255, 0, 0);
/// CLIENT LED: color by client count (1, 2, 3+)
const CLIENT_COLORS: [RGB8; 3] = [
    RGB8::new(0, 255, 0),
    RGB8::new(0, 255, 255),
    RGB8::new(0, 0, 255),
];
/// All LEDs: OTA update chase
const OTA_COLOR: RGB8 = RGB8::new(128, 0, 128);

const OFF: RGB8 = RGB8::new(0, 0, 0);

/// Dim a color to roughly a third of its intensity (activity pulse "low"
/// phase — keeps the LED identifiable instead of blinking to black).
fn dimmed(c: RGB8) -> RGB8 {
    RGB8::new(c.r / 3, c.g / 3, c.b / 3)
}

// ---------------------------------------------------------------------------
// Timing
// ---------------------------------------------------------------------------

/// Half-period of the slow "searching for SSID" blink (~1 Hz)
const SLOW_BLINK_HALF_MS: u64 = 500;

/// Half-period of the fast "awaiting DHCP" blink (~4 Hz)
const FAST_BLINK_HALF_MS: u64 = 125;

/// Half-period of the OBD "initializing" blink (~2 Hz)
const OBD_INIT_BLINK_HALF_MS: u64 = 250;

/// Half-period of the OBD "disconnected" blink (~1 Hz)
const OBD_ERROR_BLINK_HALF_MS: u64 = 500;

/// AP-client overlay pulse: cycle length and on-time at the start of each cycle
const AP_PULSE_PERIOD_MS: u64 = 2000;
const AP_PULSE_ON_MS: u64 = 200;

/// OTA chase step duration (each LED lights for this long)
const OTA_CHASE_MS: u64 = 200;

/// Whether a square-wave blink with the given half-period is in its on phase.
fn blink_on(now_ms: u64, half_period_ms: u64) -> bool {
    (now_ms / half_period_ms) % 2 == 0
}

/// Milliseconds until the next multiple of `period_ms`.
fn until_next_boundary(now_ms: u64, period_ms: u64) -> u64 {
    period_ms - (now_ms % period_ms)
}

// ---------------------------------------------------------------------------
// StatusLedMessage — channel messages from producers
// ---------------------------------------------------------------------------

/// Messages sent to the status LED task via an `mpsc` channel.
#[derive(Clone, Copy)]
pub enum StatusLedMessage {
    /// WiFi STA state changed
    WifiState(WifiStaState),
    /// Number of stations associated with our AP changed
    ApClientCount(u32),
    /// Dongle TCP state changed
    DongleState(DongleTcpState),
    /// OBD2 client count changed (downstream clients in proxy mode)
    ClientCount(u32),
    /// A request was sent to the dongle (triggers OBD LED flicker)
    DongleActivity,
    /// A request was received from / sent on behalf of a client (triggers CLIENT LED flicker)
    ClientActivity,
    /// OTA status changed
    OtaStatus(OtaState),
}

/// Channel sender type for status LED messages.
pub type StatusLedSender = Sender<StatusLedMessage>;

// ---------------------------------------------------------------------------
// StatusLedController — WS2812B chain driver
// ---------------------------------------------------------------------------

/// Drives the three-LED WS2812B status chain plus the plain red boot LED.
pub struct StatusLedController {
    driver: Option<Ws2812Esp32Rmt<'static>>,
    boot_led: Option<PinDriver<'static, AnyOutputPin, Output>>,
    /// Desired LED colors (pre-brightness)
    leds: [RGB8; STATUS_LED_COUNT],
    /// Last chain state actually written, to skip redundant RMT transmissions
    written: Option<[RGB8; STATUS_LED_COUNT]>,
    brightness: u8,
    /// Activity flicker duration (how long activity indicators stay lit)
    flicker_duration: Duration,
}

impl StatusLedController {
    /// Create a new controller.
    ///
    /// `status_gpio` is the WS2812B chain data pin (0 = disabled). `boot_led_gpio`
    /// is the plain red LED blinked during boot (0 = disabled). `flicker_ms` sets
    /// how long activity indicators stay lit.
    pub fn new<C: RmtChannel>(
        status_gpio: u8,
        boot_led_gpio: u8,
        led_brightness: u8,
        flicker_ms: u16,
        rmt_channel: impl Peripheral<P = C> + 'static,
    ) -> Self {
        let driver = if status_gpio == 0 {
            info!("Status LEDs disabled (pin not configured)");
            None
        } else {
            match Self::init_driver(status_gpio, rmt_channel) {
                Ok(d) => {
                    info!("Status LEDs (3x WS2812B) initialized on GPIO {status_gpio}");
                    Some(d)
                }
                Err(e) => {
                    warn!("Failed to initialize status LEDs on GPIO {status_gpio}: {e:?}");
                    None
                }
            }
        };

        Self {
            driver,
            boot_led: Self::init_boot_led(boot_led_gpio),
            leds: [OFF; STATUS_LED_COUNT],
            written: None,
            brightness: led_brightness,
            flicker_duration: Duration::from_millis(u64::from(flicker_ms)),
        }
    }

    fn init_driver<C: RmtChannel>(
        pin: u8,
        rmt_channel: impl Peripheral<P = C> + 'static,
    ) -> Result<Ws2812Esp32Rmt<'static>> {
        // Reset first to clear any residual peripheral configuration.
        // SAFETY: We trust the configured GPIO pin number is valid for this board.
        unsafe {
            esp_idf_svc::sys::gpio_reset_pin(i32::from(pin));
        }
        let led_pin = unsafe { esp_idf_hal::gpio::AnyIOPin::new(i32::from(pin)) };
        // Extra memory blocks buffer against WiFi interrupting the RMT
        // (see led_display.rs); 3 LEDs × 24 items fit easily in 2 blocks.
        let config = TransmitConfig::new().clock_divider(1).mem_block_num(2);
        let tx_driver = TxRmtDriver::new(rmt_channel, led_pin, &config)?;
        Ok(Ws2812Esp32Rmt::new_with_rmt_driver(tx_driver)?)
    }

    fn init_boot_led(pin: u8) -> Option<PinDriver<'static, AnyOutputPin, Output>> {
        if pin == 0 {
            return None;
        }
        // SAFETY: We trust the configured GPIO pin number is valid for this board.
        unsafe {
            esp_idf_svc::sys::gpio_reset_pin(i32::from(pin));
        }
        let output_pin = unsafe { AnyOutputPin::new(i32::from(pin)) };
        match PinDriver::output(output_pin) {
            Ok(driver) => {
                info!("Boot LED initialized on GPIO {pin}");
                Some(driver)
            }
            Err(e) => {
                warn!("Failed to initialize boot LED on GPIO {pin}: {e}");
                None
            }
        }
    }

    /// Set one LED in the chain buffer (flushed by [`Self::flush`]).
    fn set_led(&mut self, index: usize, color: RGB8) {
        self.leds[index] = color;
    }

    /// All LEDs off.
    fn all_off(&mut self) {
        self.leds = [OFF; STATUS_LED_COUNT];
        self.flush();
    }

    /// Write the LED buffer to the chain if it changed since the last write.
    fn flush(&mut self) {
        if self.written == Some(self.leds) {
            return;
        }
        if let Some(ref mut driver) = self.driver {
            let iter = brightness(gamma(self.leds.iter().copied()), self.brightness);
            if let Err(e) = driver.write(iter) {
                warn!("Status LED write failed: {e:?}");
                return;
            }
            self.written = Some(self.leds);
        }
    }

    /// Set the plain red boot LED on or off.
    fn set_boot_led(&mut self, on: bool) {
        if let Some(ref mut pin) = self.boot_led {
            let _ = if on { pin.set_high() } else { pin.set_low() };
        }
    }

    /// Boot animation: blink each status LED in sequence, one per cycle,
    /// with the plain red boot LED blinking along.
    ///
    /// Designed to be called in sync with the main RGB LED boot animation
    /// (3 purple blinks at 250 ms on / 250 ms off). Each status LED lights
    /// during one of the three on-phases in its identity color:
    /// - Cycle 1: WIFI (blue)
    /// - Cycle 2: OBD (amber)
    /// - Cycle 3: CLIENT (green)
    pub fn boot_animation(&mut self, blink_duration: Duration) {
        const BOOT_COLORS: [RGB8; STATUS_LED_COUNT] = [WIFI_COLOR, OBD_INIT_COLOR, OBD_READY_COLOR];
        for (i, color) in BOOT_COLORS.iter().enumerate() {
            // On phase — light the LED for this cycle plus the boot LED
            self.set_led(i, *color);
            self.flush();
            self.set_boot_led(true);
            std::thread::sleep(blink_duration);

            // Off phase
            self.all_off();
            self.set_boot_led(false);
            std::thread::sleep(blink_duration);
        }
    }
}

// ---------------------------------------------------------------------------
// LedTaskState — mutable shadow state for the event loop
// ---------------------------------------------------------------------------

/// Mutable shadow state used by the status LED event loop.
///
/// Bundles all tracked state and timer bookkeeping so the public
/// [`run_status_led_task`] stays short and duplication-free. Periodic
/// patterns (blinks, pulses, chases) are phase-locked to `start` so the
/// task can compute exact wake deadlines without per-pattern bookkeeping.
struct LedTaskState {
    /// Task start; blink/pulse phases are derived from time since here
    start: Instant,

    wifi: WifiStaState,
    ap_clients: u32,
    dongle: DongleTcpState,
    client_count: u32,
    ota: OtaState,

    obd_flicker_on: bool,
    obd_activity_deadline: Option<Instant>,
    client_flicker_on: bool,
    client_activity_deadline: Option<Instant>,

    activity_timeout: Duration,
}

impl LedTaskState {
    fn new(activity_timeout: Duration) -> Self {
        Self {
            start: Instant::now(),
            wifi: WifiStaState::NotConfigured,
            ap_clients: 0,
            dongle: DongleTcpState::Disconnected,
            client_count: 0,
            ota: OtaState::Idle,
            obd_flicker_on: true,
            obd_activity_deadline: None,
            client_flicker_on: true,
            client_activity_deadline: None,
            activity_timeout,
        }
    }

    /// Milliseconds since task start (blink phase reference).
    fn now_ms(&self, now: Instant) -> u64 {
        // u64::MAX ms = 584 million years, safe to truncate
        #[allow(clippy::cast_possible_truncation)]
        let ms = now.duration_since(self.start).as_millis() as u64;
        ms
    }

    /// Process a single status LED message, updating shadow state.
    fn process_message(&mut self, msg: StatusLedMessage) {
        match msg {
            StatusLedMessage::WifiState(s) => self.wifi = s,
            StatusLedMessage::ApClientCount(n) => self.ap_clients = n,
            StatusLedMessage::DongleState(s) => {
                self.dongle = s;
                if s == DongleTcpState::Disconnected {
                    self.obd_activity_deadline = None;
                }
            }
            StatusLedMessage::ClientCount(n) => {
                self.client_count = n;
                if n == 0 {
                    self.client_activity_deadline = None;
                }
            }
            StatusLedMessage::DongleActivity => {
                if self.dongle == DongleTcpState::Ready {
                    self.obd_flicker_on = !self.obd_flicker_on;
                    self.obd_activity_deadline = Some(Instant::now() + self.activity_timeout);
                }
            }
            StatusLedMessage::ClientActivity => {
                if self.client_count > 0 {
                    self.client_flicker_on = !self.client_flicker_on;
                    self.client_activity_deadline = Some(Instant::now() + self.activity_timeout);
                }
            }
            StatusLedMessage::OtaStatus(s) => self.ota = s,
        }
    }

    /// Compute the earliest deadline we need to wake for.
    fn next_deadline(&self, now: Instant) -> Option<Instant> {
        let now_ms = self.now_ms(now);
        let mut earliest: Option<Instant> = None;
        let mut consider = |t: Instant| {
            earliest = Some(match earliest {
                Some(e) => e.min(t),
                None => t,
            });
        };

        if self.ota == OtaState::Updating {
            // OTA chase overrides everything; no other timer matters
            return Some(now + Duration::from_millis(until_next_boundary(now_ms, OTA_CHASE_MS)));
        }

        match self.wifi {
            WifiStaState::Searching => {
                consider(
                    now + Duration::from_millis(until_next_boundary(now_ms, SLOW_BLINK_HALF_MS)),
                );
            }
            WifiStaState::AwaitingIp => {
                consider(
                    now + Duration::from_millis(until_next_boundary(now_ms, FAST_BLINK_HALF_MS)),
                );
            }
            WifiStaState::NotConfigured | WifiStaState::Connected => {}
        }
        if self.ap_clients > 0 {
            let pos = now_ms % AP_PULSE_PERIOD_MS;
            let until = if pos < AP_PULSE_ON_MS {
                AP_PULSE_ON_MS - pos
            } else {
                AP_PULSE_PERIOD_MS - pos
            };
            consider(now + Duration::from_millis(until));
        }
        match self.dongle {
            DongleTcpState::Disconnected => {
                consider(
                    now + Duration::from_millis(until_next_boundary(
                        now_ms,
                        OBD_ERROR_BLINK_HALF_MS,
                    )),
                );
            }
            DongleTcpState::Initializing => {
                consider(
                    now + Duration::from_millis(until_next_boundary(
                        now_ms,
                        OBD_INIT_BLINK_HALF_MS,
                    )),
                );
            }
            DongleTcpState::Ready => {}
        }
        if let Some(t) = self.obd_activity_deadline {
            consider(t);
        }
        if let Some(t) = self.client_activity_deadline {
            consider(t);
        }

        earliest
    }

    /// Expire activity-pulse timers based on the current time.
    fn update_timers(&mut self, now: Instant) {
        if let Some(t) = self.obd_activity_deadline {
            if now >= t {
                self.obd_activity_deadline = None;
                self.obd_flicker_on = true;
            }
        }
        if let Some(t) = self.client_activity_deadline {
            if now >= t {
                self.client_activity_deadline = None;
                self.client_flicker_on = true;
            }
        }
    }

    /// Render the WIFI LED color for the current state.
    fn wifi_color(&self, now_ms: u64) -> RGB8 {
        // AP-client overlay: brief cyan pulse at the start of each cycle
        if self.ap_clients > 0 && now_ms % AP_PULSE_PERIOD_MS < AP_PULSE_ON_MS {
            return AP_CLIENT_COLOR;
        }
        match self.wifi {
            WifiStaState::NotConfigured => NOT_CONFIGURED_COLOR,
            WifiStaState::Searching => {
                if blink_on(now_ms, SLOW_BLINK_HALF_MS) {
                    WIFI_COLOR
                } else {
                    OFF
                }
            }
            WifiStaState::AwaitingIp => {
                if blink_on(now_ms, FAST_BLINK_HALF_MS) {
                    WIFI_COLOR
                } else {
                    OFF
                }
            }
            WifiStaState::Connected => WIFI_COLOR,
        }
    }

    /// Render the OBD LED color for the current state.
    fn obd_color(&self, now_ms: u64) -> RGB8 {
        match self.dongle {
            DongleTcpState::Disconnected => {
                if blink_on(now_ms, OBD_ERROR_BLINK_HALF_MS) {
                    OBD_ERROR_COLOR
                } else {
                    OFF
                }
            }
            DongleTcpState::Initializing => {
                if blink_on(now_ms, OBD_INIT_BLINK_HALF_MS) {
                    OBD_INIT_COLOR
                } else {
                    OFF
                }
            }
            DongleTcpState::Ready => {
                if self.obd_activity_deadline.is_some() && !self.obd_flicker_on {
                    dimmed(OBD_READY_COLOR)
                } else {
                    OBD_READY_COLOR
                }
            }
        }
    }

    /// Render the CLIENT LED color for the current state.
    fn client_color(&self) -> RGB8 {
        if self.client_count == 0 {
            return OFF;
        }
        let idx = usize::try_from(self.client_count - 1)
            .unwrap_or(usize::MAX)
            .min(CLIENT_COLORS.len() - 1);
        let color = CLIENT_COLORS[idx];
        if self.client_activity_deadline.is_some() && !self.client_flicker_on {
            dimmed(color)
        } else {
            color
        }
    }

    /// Set LED outputs based on current shadow state.
    fn drive_leds(&self, now: Instant, controller: &mut StatusLedController) {
        let now_ms = self.now_ms(now);
        if self.ota == OtaState::Updating {
            // OTA override: purple chase across the chain
            #[allow(clippy::cast_possible_truncation)]
            let chase = ((now_ms / OTA_CHASE_MS) % STATUS_LED_COUNT as u64) as usize;
            for i in 0..STATUS_LED_COUNT {
                controller.set_led(i, if i == chase { OTA_COLOR } else { OFF });
            }
        } else {
            controller.set_led(WIFI_LED, self.wifi_color(now_ms));
            controller.set_led(OBD_LED, self.obd_color(now_ms));
            controller.set_led(CLIENT_LED, self.client_color());
        }
        controller.flush();
    }
}

/// Entry point for the status LED background task.
///
/// Runs the boot animation, then enters the event-driven control loop.
/// The controller is created in `main` (it needs an RMT channel peripheral).
pub fn run_status_led_task(mut controller: StatusLedController, rx: &Receiver<StatusLedMessage>) {
    controller.boot_animation(Duration::from_millis(250));
    controller.run_task(rx);
}

impl StatusLedController {
    /// Run the status LED control loop.
    ///
    /// This blocks on the channel receiver using `recv_timeout`, waking only
    /// when a message arrives or a blink/flicker deadline expires.
    pub fn run_task(&mut self, rx: &Receiver<StatusLedMessage>) {
        let mut state = LedTaskState::new(self.flicker_duration);

        info!(
            "Status LED task started (activity timeout: {}ms)",
            self.flicker_duration.as_millis()
        );

        loop {
            let deadline = state.next_deadline(Instant::now());

            // Block on channel with optional timeout
            let msg = match deadline {
                Some(dl) => {
                    let now = Instant::now();
                    if now >= dl {
                        None
                    } else {
                        match rx.recv_timeout(dl - now) {
                            Ok(m) => Some(m),
                            Err(RecvTimeoutError::Timeout) => None,
                            Err(RecvTimeoutError::Disconnected) => {
                                warn!("Status LED channel closed, task exiting");
                                self.all_off();
                                return;
                            }
                        }
                    }
                }
                None => {
                    if let Ok(m) = rx.recv() {
                        Some(m)
                    } else {
                        warn!("Status LED channel closed, task exiting");
                        self.all_off();
                        return;
                    }
                }
            };

            // Process initial message (if any)
            if let Some(msg) = msg {
                state.process_message(msg);
            }

            // Drain any additional pending messages to batch updates
            while let Ok(msg) = rx.try_recv() {
                state.process_message(msg);
            }

            // Advance timers and drive outputs
            let now = Instant::now();
            state.update_timers(now);
            state.drive_leds(now, self);
        }
    }
}
