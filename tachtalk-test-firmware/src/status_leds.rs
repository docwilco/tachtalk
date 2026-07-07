//! Status LED controller and background task
//!
//! Drives a chain of three WS2812B status LEDs on a single data GPIO
//! (ordered outward from the MCU): **WIFI**, **OBD**, **CLIENT** (used here
//! as the test-running indicator).
//! - **WIFI**: WiFi STA connection state (off/blink/solid)
//! - **OBD**: OBD2 dongle TCP state (off/solid during init/flicker when active)
//! - **CLIENT**: Test running state (off/solid/flicker on activity)
//!
//! The task is fully event-driven via an `mpsc` channel — producers send
//! [`StatusLedMessage`] variants on state transitions and per-request activity.
//! An OTA override displays a sequential chase pattern across all three LEDs.
//!
//! A plain red boot LED (separate GPIO) blinks along with the boot animation.

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
/// Chain index of the CLIENT status LED (furthest from the MCU, test indicator)
const TEST_LED: usize = 2;

/// Color shown on the WIFI LED when lit
const WIFI_COLOR: RGB8 = RGB8::new(255, 0, 0);
/// Color shown on the OBD LED when lit
const OBD_COLOR: RGB8 = RGB8::new(255, 200, 0);
/// Color shown on the CLIENT/test LED when lit
const TEST_COLOR: RGB8 = RGB8::new(0, 255, 0);

const OFF: RGB8 = RGB8::new(0, 0, 0);

// ---------------------------------------------------------------------------
// StatusLedMessage — channel messages from producers
// ---------------------------------------------------------------------------

/// Messages sent to the status LED task via an `mpsc` channel.
#[derive(Clone, Copy)]
pub enum StatusLedMessage {
    /// WiFi STA state changed
    WifiState(WifiStaState),
    /// Dongle TCP state changed
    DongleState(DongleTcpState),
    /// Test running state changed
    TestRunning(bool),
    /// A request was sent to the dongle (triggers OBD LED flicker)
    DongleActivity,
    /// A request was sent during the test (triggers CLIENT LED flicker)
    TestActivity,
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
}

impl StatusLedController {
    /// Create a new controller.
    ///
    /// `status_gpio` is the WS2812B chain data pin (0 = disabled). `boot_led_gpio`
    /// is the plain red LED blinked during boot (0 = disabled).
    pub fn new<C: RmtChannel>(
        status_gpio: u8,
        boot_led_gpio: u8,
        led_brightness: u8,
        rmt_channel: impl Peripheral<P = C> + 'static,
    ) -> Self {
        let driver = if status_gpio == 0 {
            info!("Status LEDs disabled (pin not configured)");
            None
        } else {
            Self::init_driver(status_gpio, rmt_channel)
        };

        Self {
            driver,
            boot_led: Self::init_boot_led(boot_led_gpio),
            leds: [OFF; STATUS_LED_COUNT],
            written: None,
            brightness: led_brightness,
        }
    }

    fn init_driver<C: RmtChannel>(
        pin: u8,
        rmt_channel: impl Peripheral<P = C> + 'static,
    ) -> Option<Ws2812Esp32Rmt<'static>> {
        // Reset first to clear any residual peripheral configuration.
        // SAFETY: We trust the configured GPIO pin number is valid for this board.
        unsafe {
            esp_idf_svc::sys::gpio_reset_pin(i32::from(pin));
        }
        let led_pin = unsafe { esp_idf_hal::gpio::AnyIOPin::new(i32::from(pin)) };
        // Extra memory blocks buffer against WiFi interrupting the RMT;
        // 3 LEDs × 24 items fit easily in 2 blocks.
        let config = TransmitConfig::new().clock_divider(1).mem_block_num(2);
        let tx_driver = match TxRmtDriver::new(rmt_channel, led_pin, &config) {
            Ok(d) => d,
            Err(e) => {
                warn!("Failed to initialize status LED RMT on GPIO {pin}: {e:?}");
                return None;
            }
        };
        match Ws2812Esp32Rmt::new_with_rmt_driver(tx_driver) {
            Ok(d) => {
                info!("Status LEDs (3x WS2812B) initialized on GPIO {pin}");
                Some(d)
            }
            Err(e) => {
                warn!("Failed to initialize status LED driver on GPIO {pin}: {e:?}");
                None
            }
        }
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

    /// Set the WIFI LED on or off.
    fn set_wifi(&mut self, on: bool) {
        self.leds[WIFI_LED] = if on { WIFI_COLOR } else { OFF };
    }

    /// Set the OBD LED on or off.
    fn set_obd(&mut self, on: bool) {
        self.leds[OBD_LED] = if on { OBD_COLOR } else { OFF };
    }

    /// Set the CLIENT/test LED on or off.
    fn set_test(&mut self, on: bool) {
        self.leds[TEST_LED] = if on { TEST_COLOR } else { OFF };
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
    /// Each status LED lights during one of the three on-phases:
    /// - Cycle 1: WIFI
    /// - Cycle 2: OBD
    /// - Cycle 3: CLIENT/test
    pub fn boot_animation(&mut self, blink_duration: Duration) {
        for i in 0..STATUS_LED_COUNT {
            // On phase — light the LED for this cycle plus the boot LED
            match i {
                WIFI_LED => self.set_wifi(true),
                OBD_LED => self.set_obd(true),
                _ => self.set_test(true),
            }
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
// Status LED task
// ---------------------------------------------------------------------------

/// Blink half-period for WiFi "connecting" state (~2 Hz = 250 ms half-period)
const WIFI_BLINK_MS: u64 = 250;

/// OTA chase step duration (each LED lights for this long)
const OTA_CHASE_MS: u64 = 200;

// ---------------------------------------------------------------------------
// LedTaskState — mutable shadow state for the event loop
// ---------------------------------------------------------------------------

/// Mutable shadow state used by the status LED event loop.
///
/// Bundles all tracked state and timer bookkeeping so the public
/// [`status_led_task`] stays short and duplication-free.
#[allow(clippy::struct_excessive_bools)]
struct LedTaskState {
    wifi: WifiStaState,
    dongle: DongleTcpState,
    test_running: bool,
    ota: OtaState,

    wifi_blink_on: bool,
    wifi_blink_deadline: Instant,

    obd_flicker_on: bool,
    obd_activity_deadline: Option<Instant>,
    test_flicker_on: bool,
    test_activity_deadline: Option<Instant>,

    ota_chase_index: u8,
    ota_chase_deadline: Instant,

    activity_timeout: Duration,
}

impl LedTaskState {
    fn new(activity_timeout: Duration) -> Self {
        let now = Instant::now();
        Self {
            wifi: WifiStaState::Disconnected,
            dongle: DongleTcpState::Disconnected,
            test_running: false,
            ota: OtaState::Idle,
            wifi_blink_on: false,
            wifi_blink_deadline: now,
            obd_flicker_on: true,
            obd_activity_deadline: None,
            test_flicker_on: true,
            test_activity_deadline: None,
            ota_chase_index: 0,
            ota_chase_deadline: now,
            activity_timeout,
        }
    }

    /// Process a single status LED message, updating shadow state.
    fn process_message(&mut self, msg: StatusLedMessage) {
        match msg {
            StatusLedMessage::WifiState(s) => {
                if s != self.wifi {
                    self.wifi = s;
                    self.wifi_blink_on = true;
                    self.wifi_blink_deadline =
                        Instant::now() + Duration::from_millis(WIFI_BLINK_MS);
                }
            }
            StatusLedMessage::DongleState(s) => {
                self.dongle = s;
                if s == DongleTcpState::Disconnected {
                    self.obd_activity_deadline = None;
                }
            }
            StatusLedMessage::TestRunning(running) => {
                self.test_running = running;
                if !running {
                    self.test_activity_deadline = None;
                }
            }
            StatusLedMessage::DongleActivity => {
                if self.dongle == DongleTcpState::Ready {
                    self.obd_flicker_on = !self.obd_flicker_on;
                    self.obd_activity_deadline = Some(Instant::now() + self.activity_timeout);
                }
            }
            StatusLedMessage::TestActivity => {
                if self.test_running {
                    self.test_flicker_on = !self.test_flicker_on;
                    self.test_activity_deadline = Some(Instant::now() + self.activity_timeout);
                }
            }
            StatusLedMessage::OtaStatus(s) => {
                let was_active = matches!(self.ota, OtaState::Downloading | OtaState::Flashing);
                self.ota = s;
                if matches!(s, OtaState::Downloading | OtaState::Flashing) && !was_active {
                    self.ota_chase_index = 0;
                    self.ota_chase_deadline = Instant::now() + Duration::from_millis(OTA_CHASE_MS);
                }
            }
        }
    }

    /// Compute the earliest deadline we need to wake for.
    fn next_deadline(&self) -> Option<Instant> {
        let mut earliest: Option<Instant> = None;
        let mut consider = |t: Instant| {
            earliest = Some(match earliest {
                Some(e) => e.min(t),
                None => t,
            });
        };

        if self.wifi == WifiStaState::AwaitingIp {
            consider(self.wifi_blink_deadline);
        }
        if let Some(t) = self.obd_activity_deadline {
            consider(t);
        }
        if let Some(t) = self.test_activity_deadline {
            consider(t);
        }
        if matches!(self.ota, OtaState::Downloading | OtaState::Flashing) {
            consider(self.ota_chase_deadline);
        }

        earliest
    }

    /// Advance blink / flicker / chase timers based on the current time.
    fn update_timers(&mut self, now: Instant) {
        if self.wifi == WifiStaState::AwaitingIp && now >= self.wifi_blink_deadline {
            self.wifi_blink_on = !self.wifi_blink_on;
            self.wifi_blink_deadline = now + Duration::from_millis(WIFI_BLINK_MS);
        }
        if let Some(t) = self.obd_activity_deadline {
            if now >= t {
                self.obd_activity_deadline = None;
                self.obd_flicker_on = true;
            }
        }
        if let Some(t) = self.test_activity_deadline {
            if now >= t {
                self.test_activity_deadline = None;
                self.test_flicker_on = true;
            }
        }
        if matches!(self.ota, OtaState::Downloading | OtaState::Flashing)
            && now >= self.ota_chase_deadline
        {
            self.ota_chase_index = (self.ota_chase_index + 1) % 3;
            self.ota_chase_deadline = now + Duration::from_millis(OTA_CHASE_MS);
        }
    }

    /// Set LED outputs based on current shadow state.
    fn drive_leds(&self, controller: &mut StatusLedController) {
        if matches!(self.ota, OtaState::Downloading | OtaState::Flashing) {
            // OTA override: sequential chase
            controller.set_wifi(self.ota_chase_index == 0);
            controller.set_obd(self.ota_chase_index == 1);
            controller.set_test(self.ota_chase_index == 2);
        } else {
            // WIFI: WiFi STA state
            let wifi_on = match self.wifi {
                WifiStaState::Disconnected => false,
                WifiStaState::AwaitingIp => self.wifi_blink_on,
                WifiStaState::Connected => true,
            };
            controller.set_wifi(wifi_on);

            // OBD: dongle TCP state
            let obd_on = match self.dongle {
                DongleTcpState::Disconnected => false,
                DongleTcpState::Initializing => true,
                DongleTcpState::Ready => {
                    if self.obd_activity_deadline.is_some() {
                        self.obd_flicker_on
                    } else {
                        true
                    }
                }
            };
            controller.set_obd(obd_on);

            // CLIENT: test running state
            let test_on = self.test_running
                && if self.test_activity_deadline.is_some() {
                    self.test_flicker_on
                } else {
                    true
                };
            controller.set_test(test_on);
        }
        controller.flush();
    }
}

/// Run the status LED background task.
///
/// This task blocks on the channel receiver using `recv_timeout`, waking only
/// when a message arrives or a blink/flicker deadline expires.
pub fn status_led_task(
    mut controller: StatusLedController,
    rx: &Receiver<StatusLedMessage>,
    activity_timeout: Duration,
) {
    let mut state = LedTaskState::new(activity_timeout);

    info!(
        "Status LED task started (activity timeout: {}ms)",
        activity_timeout.as_millis()
    );

    loop {
        let deadline = state.next_deadline();

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
                            controller.all_off();
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
                    controller.all_off();
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
        state.update_timers(Instant::now());
        state.drive_leds(&mut controller);
    }
}
