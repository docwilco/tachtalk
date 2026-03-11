//! Status LED controller and background task
//!
//! Drives three GPIO-connected status LEDs (red, yellow, green) to indicate:
//! - **Red**: WiFi STA connection state (off/blink/solid)
//! - **Yellow**: OBD2 dongle TCP state (off/solid during init/flicker when active)
//! - **Green**: Client connection state (off/solid/flicker on activity)
//!
//! The task is fully event-driven via an `mpsc` channel — producers send
//! [`StatusLedMessage`] variants on state transitions and per-request activity.
//! An OTA override displays a sequential chase pattern across all three LEDs.

use crate::obd2::DongleTcpState;
use crate::ota::OtaState;
use crate::wifi::WifiStaState;
use crate::State;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use esp_idf_hal::gpio::{AnyOutputPin, Output, PinDriver};
use log::{info, warn};

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
    /// OBD2 client count changed (downstream clients in proxy mode)
    ClientCount(u32),
    /// A request was sent to the dongle (triggers yellow flicker)
    DongleActivity,
    /// A request was received from / sent on behalf of a client (triggers green flicker)
    ClientActivity,
    /// OTA status changed
    OtaStatus(OtaState),
}

/// Channel sender type for status LED messages.
pub type StatusLedSender = Sender<StatusLedMessage>;

// ---------------------------------------------------------------------------
// StatusLedController — GPIO output driver
// ---------------------------------------------------------------------------

/// Drives up to three GPIO pins as status indicator LEDs (active-high).
pub struct StatusLedController {
    red: Option<PinDriver<'static, AnyOutputPin, Output>>,
    yellow: Option<PinDriver<'static, AnyOutputPin, Output>>,
    green: Option<PinDriver<'static, AnyOutputPin, Output>>,
    /// Activity flicker duration (how long activity indicators stay lit)
    flicker_duration: Duration,
}

impl StatusLedController {
    /// Create a new controller from the configured GPIO pin numbers.
    ///
    /// A pin value of 0 means "disabled" — that LED will not be driven.
    /// `flicker_ms` sets how long activity indicators stay lit.
    #[must_use]
    pub fn new(red_pin: u8, yellow_pin: u8, green_pin: u8, flicker_ms: u16) -> Self {
        Self {
            red: Self::init_pin(red_pin, "red"),
            yellow: Self::init_pin(yellow_pin, "yellow"),
            green: Self::init_pin(green_pin, "green"),
            flicker_duration: Duration::from_millis(u64::from(flicker_ms)),
        }
    }

    fn init_pin(pin: u8, name: &str) -> Option<PinDriver<'static, AnyOutputPin, Output>> {
        if pin == 0 {
            return None;
        }
        // SAFETY: We trust the user-configured GPIO pin number is valid for this board.
        // The pin is reset first to clear any residual peripheral configuration.
        unsafe {
            esp_idf_svc::sys::gpio_reset_pin(i32::from(pin));
        }
        let output_pin = unsafe { AnyOutputPin::new(i32::from(pin)) };
        match PinDriver::output(output_pin) {
            Ok(driver) => {
                info!("Status LED '{name}' initialized on GPIO {pin}");
                Some(driver)
            }
            Err(e) => {
                warn!("Failed to initialize status LED '{name}' on GPIO {pin}: {e}");
                None
            }
        }
    }

    /// Set the red LED on or off.
    fn set_red(&mut self, on: bool) {
        if let Some(ref mut pin) = self.red {
            let _ = if on { pin.set_high() } else { pin.set_low() };
        }
    }

    /// Set the yellow LED on or off.
    fn set_yellow(&mut self, on: bool) {
        if let Some(ref mut pin) = self.yellow {
            let _ = if on { pin.set_high() } else { pin.set_low() };
        }
    }

    /// Set the green LED on or off.
    fn set_green(&mut self, on: bool) {
        if let Some(ref mut pin) = self.green {
            let _ = if on { pin.set_high() } else { pin.set_low() };
        }
    }

    /// All LEDs off.
    fn all_off(&mut self) {
        self.set_red(false);
        self.set_yellow(false);
        self.set_green(false);
    }

    /// Boot animation: blink each status LED in sequence, one per cycle.
    ///
    /// Designed to be called in sync with the RGB LED boot animation (3 purple
    /// blinks at 250 ms on / 250 ms off). Each status LED lights during one of
    /// the three on-phases:
    /// - Cycle 1: red
    /// - Cycle 2: yellow
    /// - Cycle 3: green
    pub fn boot_animation(&mut self, blink_duration: Duration) {
        for i in 0..3 {
            // On phase — light the LED for this cycle
            match i {
                0 => self.set_red(true),
                1 => self.set_yellow(true),
                _ => self.set_green(true),
            }
            std::thread::sleep(blink_duration);

            // Off phase
            self.all_off();
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
struct LedTaskState {
    wifi: WifiStaState,
    dongle: DongleTcpState,
    client_count: u32,
    ota: OtaState,

    wifi_blink_on: bool,
    wifi_blink_deadline: Instant,

    yellow_flicker_on: bool,
    yellow_activity_deadline: Option<Instant>,
    green_flicker_on: bool,
    green_activity_deadline: Option<Instant>,

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
            client_count: 0,
            ota: OtaState::Idle,
            wifi_blink_on: false,
            wifi_blink_deadline: now,
            yellow_flicker_on: true,
            yellow_activity_deadline: None,
            green_flicker_on: true,
            green_activity_deadline: None,
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
                    self.yellow_activity_deadline = None;
                }
            }
            StatusLedMessage::ClientCount(n) => {
                self.client_count = n;
                if n == 0 {
                    self.green_activity_deadline = None;
                }
            }
            StatusLedMessage::DongleActivity => {
                if self.dongle == DongleTcpState::Ready {
                    self.yellow_flicker_on = !self.yellow_flicker_on;
                    self.yellow_activity_deadline = Some(Instant::now() + self.activity_timeout);
                }
            }
            StatusLedMessage::ClientActivity => {
                if self.client_count > 0 {
                    self.green_flicker_on = !self.green_flicker_on;
                    self.green_activity_deadline = Some(Instant::now() + self.activity_timeout);
                }
            }
            StatusLedMessage::OtaStatus(s) => {
                let was_active = self.ota == OtaState::Updating;
                self.ota = s;
                if s == OtaState::Updating && !was_active {
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

        if self.wifi == WifiStaState::Connecting {
            consider(self.wifi_blink_deadline);
        }
        if let Some(t) = self.yellow_activity_deadline {
            consider(t);
        }
        if let Some(t) = self.green_activity_deadline {
            consider(t);
        }
        if self.ota == OtaState::Updating {
            consider(self.ota_chase_deadline);
        }

        earliest
    }

    /// Advance blink / flicker / chase timers based on the current time.
    fn update_timers(&mut self, now: Instant) {
        if self.wifi == WifiStaState::Connecting && now >= self.wifi_blink_deadline {
            self.wifi_blink_on = !self.wifi_blink_on;
            self.wifi_blink_deadline = now + Duration::from_millis(WIFI_BLINK_MS);
        }
        if let Some(t) = self.yellow_activity_deadline {
            if now >= t {
                self.yellow_activity_deadline = None;
                self.yellow_flicker_on = true;
            }
        }
        if let Some(t) = self.green_activity_deadline {
            if now >= t {
                self.green_activity_deadline = None;
                self.green_flicker_on = true;
            }
        }
        if self.ota == OtaState::Updating && now >= self.ota_chase_deadline {
            self.ota_chase_index = (self.ota_chase_index + 1) % 3;
            self.ota_chase_deadline = now + Duration::from_millis(OTA_CHASE_MS);
        }
    }

    /// Set LED outputs based on current shadow state.
    fn drive_leds(&self, controller: &mut StatusLedController) {
        if self.ota == OtaState::Updating {
            // OTA override: sequential chase
            controller.set_red(self.ota_chase_index == 0);
            controller.set_yellow(self.ota_chase_index == 1);
            controller.set_green(self.ota_chase_index == 2);
        } else {
            // Red: WiFi STA state
            let red_on = match self.wifi {
                WifiStaState::Disconnected => false,
                WifiStaState::Connecting => self.wifi_blink_on,
                WifiStaState::Connected => true,
            };
            controller.set_red(red_on);

            // Yellow: dongle TCP state
            let yellow_on = match self.dongle {
                DongleTcpState::Disconnected => false,
                DongleTcpState::Initializing => true,
                DongleTcpState::Ready => {
                    if self.yellow_activity_deadline.is_some() {
                        self.yellow_flicker_on
                    } else {
                        true
                    }
                }
            };
            controller.set_yellow(yellow_on);

            // Green: client state
            let green_on = self.client_count > 0
                && if self.green_activity_deadline.is_some() {
                    self.green_flicker_on
                } else {
                    true
                };
            controller.set_green(green_on);
        }
    }
}

/// Entry point for the status LED background task.
///
/// Reads configuration from state to create the LED controller, runs boot
/// animation, then enters the event-driven control loop.
///
/// # Panics
///
/// Panics if the config mutex is poisoned.
pub fn run_status_led_task(state: &Arc<State>, rx: &Receiver<StatusLedMessage>) {
    let mut controller = {
        let cfg = state.config.lock().unwrap();
        StatusLedController::new(
            cfg.status_led_red_pin,
            cfg.status_led_yellow_pin,
            cfg.status_led_green_pin,
            cfg.status_led_flicker_ms,
        )
    };

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
            state.update_timers(Instant::now());
            state.drive_leds(self);
        }
    }
}
