use crate::error::{Error, Result};
use esp_idf_svc::nvs::{EspNvs, EspNvsPartition, NvsDefault};
use esp_idf_svc::sys::{esp_mac_type_t_ESP_MAC_WIFI_STA, esp_read_mac};
use log::{debug, info, warn, LevelFilter};
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;
use std::sync::Mutex;

const AP_SSID_PREFIX: &str = "TachTalk-";

/// Read WiFi STA MAC address from eFuse (available before WiFi driver init)
fn get_wifi_sta_mac() -> [u8; 6] {
    let mut mac = [0u8; 6];
    // SAFETY: esp_read_mac just reads from eFuse, no driver needed
    unsafe {
        esp_read_mac(mac.as_mut_ptr(), esp_mac_type_t_ESP_MAC_WIFI_STA);
    }
    mac
}

/// Generate default AP SSID from WiFi MAC address
fn default_ap_ssid() -> String {
    let mac = get_wifi_sta_mac();
    format!("{AP_SSID_PREFIX}{:02X}{:02X}", mac[4], mac[5])
}

// Re-export shift-lights types for use in the firmware
pub use tachtalk_shift_lights_lib::{LedRule, RGB8};

/// A named collection of LED rule configurations.
///
/// Profiles allow users to switch between different shift light setups
/// (e.g., "Street", "Track", "Economy") without reconfiguring each time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedProfile {
    pub name: String,
    pub rules: Vec<LedRule>,
    /// RPM value to show when previewing this profile (e.g., when cycling profiles)
    #[serde(default = "default_preview_rpm")]
    pub preview_rpm: u32,
}

const fn default_preview_rpm() -> u32 {
    3000
}

/// Configurable log level
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    #[default]
    Info,
    Debug,
}

impl LogLevel {
    #[must_use]
    pub const fn as_level_filter(self) -> LevelFilter {
        match self {
            Self::Off => LevelFilter::Off,
            Self::Error => LevelFilter::Error,
            Self::Warn => LevelFilter::Warn,
            Self::Info => LevelFilter::Info,
            Self::Debug => LevelFilter::Debug,
        }
    }
}

const NVS_NAMESPACE: &str = "tachtalk";
const NVS_CONFIG_KEY: &str = "config";

// Global NVS handle - initialized once in main
static NVS: Mutex<Option<EspNvs<NvsDefault>>> = Mutex::new(None);

pub fn init_nvs(nvs_partition: EspNvsPartition<NvsDefault>) -> Result<()> {
    debug!("Initializing NVS namespace: {NVS_NAMESPACE}");
    let nvs = EspNvs::new(nvs_partition, NVS_NAMESPACE, true)?;
    *NVS.lock().unwrap() = Some(nvs);
    info!("NVS initialized");
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WifiConfig {
    pub ssid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

impl Default for WifiConfig {
    fn default() -> Self {
        Self {
            ssid: "V-LINK".to_string(),
            password: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpConfig {
    pub use_dhcp: bool,
    /// Static IP address (used when `use_dhcp` is false)
    #[serde(default = "default_static_ip")]
    pub ip: String,
    /// Subnet prefix length (e.g., 24 for /24). Defaults to 24.
    #[serde(default = "default_prefix_len")]
    pub prefix_len: u8,
}

fn default_static_ip() -> String {
    "192.168.0.20".to_string()
}

const fn default_prefix_len() -> u8 {
    24
}

impl Default for IpConfig {
    fn default() -> Self {
        Self {
            use_dhcp: true,
            ip: default_static_ip(),
            prefix_len: default_prefix_len(),
        }
    }
}

/// Slow poll mode: interval-based or ratio-based
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlowPollMode {
    /// Poll slow PIDs at fixed interval (ms)
    Interval,
    /// Poll 1 slow PID per N fast requests
    #[default]
    Ratio,
}

/// Complete PID → data-length lookup table.
///
/// A `[Option<u8>; 256]` array indexed by PID byte, initialized from the
/// SAE J1979 static table. Runtime-learned lengths for vendor-specific PIDs
/// can be written directly into a mutable copy.
pub type PidDataLengths = [Option<u8>; 256];

/// Response data byte count for each Mode 01 PID (excludes the 0x41 service byte and PID byte).
/// Indexed by PID byte. `None` = unknown/unsupported PID.
/// Source: SAE J1979 / ISO 15031-5.
///
/// Use this to initialize a mutable `PidDataLengths` that can be updated
/// at runtime as unknown PID lengths are learned.
pub const MODE01_PID_DATA_LENGTHS: PidDataLengths = {
    let mut t = [None; 256];
    // 0x00-0x20: PIDs supported bitmasks and basic engine data
    t[0x00] = Some(4); // PIDs supported [01-20]
    t[0x01] = Some(4); // Monitor status since DTCs cleared
    t[0x02] = Some(2); // Freeze DTC
    t[0x03] = Some(2); // Fuel system status
    t[0x04] = Some(1); // Calculated engine load
    t[0x05] = Some(1); // Engine coolant temperature
    t[0x06] = Some(1); // Short term fuel trim — Bank 1
    t[0x07] = Some(1); // Long term fuel trim — Bank 1
    t[0x08] = Some(1); // Short term fuel trim — Bank 2
    t[0x09] = Some(1); // Long term fuel trim — Bank 2
    t[0x0A] = Some(1); // Fuel pressure
    t[0x0B] = Some(1); // Intake manifold absolute pressure
    t[0x0C] = Some(2); // Engine RPM
    t[0x0D] = Some(1); // Vehicle speed
    t[0x0E] = Some(1); // Timing advance
    t[0x0F] = Some(1); // Intake air temperature
    t[0x10] = Some(2); // MAF air flow rate
    t[0x11] = Some(1); // Throttle position
    t[0x12] = Some(1); // Commanded secondary air status
    t[0x13] = Some(1); // O2 sensors present (2 banks)
    t[0x14] = Some(2); // O2 sensor 1 — voltage & trim
    t[0x15] = Some(2); // O2 sensor 2
    t[0x16] = Some(2); // O2 sensor 3
    t[0x17] = Some(2); // O2 sensor 4
    t[0x18] = Some(2); // O2 sensor 5
    t[0x19] = Some(2); // O2 sensor 6
    t[0x1A] = Some(2); // O2 sensor 7
    t[0x1B] = Some(2); // O2 sensor 8
    t[0x1C] = Some(1); // OBD standards this vehicle conforms to
    t[0x1D] = Some(1); // O2 sensors present (4 banks)
    t[0x1E] = Some(1); // Auxiliary input status
    t[0x1F] = Some(2); // Run time since engine start
                       // 0x20-0x40
    t[0x20] = Some(4); // PIDs supported [21-40]
    t[0x21] = Some(2); // Distance traveled with MIL on
    t[0x22] = Some(2); // Fuel rail pressure (relative to manifold vacuum)
    t[0x23] = Some(2); // Fuel rail gauge pressure (diesel/GDI)
    t[0x24] = Some(4); // O2 sensor 1 — equiv ratio & voltage
    t[0x25] = Some(4); // O2 sensor 2
    t[0x26] = Some(4); // O2 sensor 3
    t[0x27] = Some(4); // O2 sensor 4
    t[0x28] = Some(4); // O2 sensor 5
    t[0x29] = Some(4); // O2 sensor 6
    t[0x2A] = Some(4); // O2 sensor 7
    t[0x2B] = Some(4); // O2 sensor 8
    t[0x2C] = Some(1); // Commanded EGR
    t[0x2D] = Some(1); // EGR error
    t[0x2E] = Some(1); // Commanded evaporative purge
    t[0x2F] = Some(1); // Fuel tank level input
    t[0x30] = Some(1); // Warm-ups since codes cleared
    t[0x31] = Some(2); // Distance traveled since codes cleared
    t[0x32] = Some(2); // Evap system vapor pressure
    t[0x33] = Some(1); // Absolute barometric pressure
    t[0x34] = Some(4); // O2 sensor 1 — equiv ratio & current
    t[0x35] = Some(4); // O2 sensor 2
    t[0x36] = Some(4); // O2 sensor 3
    t[0x37] = Some(4); // O2 sensor 4
    t[0x38] = Some(4); // O2 sensor 5
    t[0x39] = Some(4); // O2 sensor 6
    t[0x3A] = Some(4); // O2 sensor 7
    t[0x3B] = Some(4); // O2 sensor 8
    t[0x3C] = Some(2); // Catalyst temperature: Bank 1, Sensor 1
    t[0x3D] = Some(2); // Catalyst temperature: Bank 2, Sensor 1
    t[0x3E] = Some(2); // Catalyst temperature: Bank 1, Sensor 2
    t[0x3F] = Some(2); // Catalyst temperature: Bank 2, Sensor 2
                       // 0x40-0x60
    t[0x40] = Some(4); // PIDs supported [41-60]
    t[0x41] = Some(4); // Monitor status this drive cycle
    t[0x42] = Some(2); // Control module voltage
    t[0x43] = Some(2); // Absolute load value
    t[0x44] = Some(2); // Fuel-air commanded equivalence ratio
    t[0x45] = Some(1); // Relative throttle position
    t[0x46] = Some(1); // Ambient air temperature
    t[0x47] = Some(1); // Absolute throttle position B
    t[0x48] = Some(1); // Absolute throttle position C
    t[0x49] = Some(1); // Accelerator pedal position D
    t[0x4A] = Some(1); // Accelerator pedal position E
    t[0x4B] = Some(1); // Accelerator pedal position F
    t[0x4C] = Some(1); // Commanded throttle actuator
    t[0x4D] = Some(2); // Time run with MIL on
    t[0x4E] = Some(2); // Time since trouble codes cleared
    t[0x4F] = Some(4); // Max values (equiv ratio, O2 voltage, O2 current, intake pressure)
    t[0x50] = Some(4); // Max air flow rate from MAF sensor
    t[0x51] = Some(1); // Fuel type
    t[0x52] = Some(1); // Ethanol fuel %
    t[0x53] = Some(2); // Absolute evap system vapor pressure
    t[0x54] = Some(2); // Evap system vapor pressure
    t[0x55] = Some(2); // Short term secondary O2 trim — Bank 1 & 3
    t[0x56] = Some(2); // Long term secondary O2 trim — Bank 1 & 3
    t[0x57] = Some(2); // Short term secondary O2 trim — Bank 2 & 4
    t[0x58] = Some(2); // Long term secondary O2 trim — Bank 2 & 4
    t[0x59] = Some(2); // Fuel rail absolute pressure
    t[0x5A] = Some(1); // Relative accelerator pedal position
    t[0x5B] = Some(1); // Hybrid battery pack remaining life
    t[0x5C] = Some(1); // Engine oil temperature
    t[0x5D] = Some(2); // Fuel injection timing
    t[0x5E] = Some(2); // Engine fuel rate
    t[0x5F] = Some(1); // Emission requirements
                       // 0x60-0x80
    t[0x60] = Some(4); // PIDs supported [61-80]
    t[0x61] = Some(1); // Driver's demand engine — percent torque
    t[0x62] = Some(1); // Actual engine — percent torque
    t[0x63] = Some(2); // Engine reference torque
    t[0x64] = Some(5); // Engine percent torque data
    t[0x65] = Some(2); // Auxiliary input / output supported
                       // 0x80-0xA0
    t[0x80] = Some(4); // PIDs supported [81-A0]
                       // 0xA0-0xC0
    t[0xA0] = Some(4); // PIDs supported [A1-C0]
                       // 0xC0-0xE0
    t[0xC0] = Some(4); // PIDs supported [C1-E0]
    t
};

/// OBD2 network configuration
// Allow: many simple on/off options for different query strategies and
// features, not a state machine
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Obd2Config {
    /// IP address of the OBD2 dongle
    #[serde(default = "default_dongle_ip")]
    pub dongle_ip: String,
    /// Port of the OBD2 dongle
    #[serde(default = "default_dongle_port")]
    pub dongle_port: u16,
    /// Port to listen on for OBD2 clients
    #[serde(default = "default_listen_port")]
    pub listen_port: u16,
    /// Slow poll mode: interval or ratio
    #[serde(default)]
    pub slow_poll_mode: SlowPollMode,
    /// Interval between slow PID polls (ms) - used in interval mode
    #[serde(default = "default_slow_poll_interval_ms")]
    pub slow_poll_interval_ms: u64,
    /// Ratio of fast requests per slow request - used in ratio mode
    #[serde(default = "default_slow_poll_ratio")]
    pub slow_poll_ratio: u32,
    /// Wait time threshold (ms) for promoting slow PID to fast - used in ratio mode
    #[serde(default = "default_promotion_wait_threshold_ms")]
    pub promotion_wait_threshold_ms: u64,
    /// Time without waiters before demoting fast PID to slow (ms)
    #[serde(default = "default_fast_demotion_ms")]
    pub fast_demotion_ms: u64,
    /// Time without consumption before removing PID from polling (ms)
    #[serde(default = "default_pid_inactive_removal_ms")]
    pub pid_inactive_removal_ms: u64,
    // --- Advanced layered stack options ---
    /// Combine multiple PIDs into single commands (e.g., `010C0D0F`).
    /// Auto-detects ECU support on the first multi-PID attempt.
    #[serde(default)]
    pub use_multi_pid: bool,
    /// Maximum PIDs per multi-PID command (1-6, OBD2 spec limit is 6)
    #[serde(default = "default_max_pids_per_query")]
    pub max_pids_per_query: u8,
    /// Use repeat command for identical consecutive queries
    #[serde(default)]
    pub use_repeat: bool,
    /// Repeat command string (empty = bare CR per ELM327 spec)
    #[serde(default)]
    pub repeat_string: String,
    /// Enable CAN framing (ATH1) for header parsing
    #[serde(default)]
    pub use_framing: bool,
    /// Enable pipelined queries (keep 1 request in-flight).
    /// NOTE: Not yet implemented — config field reserved for future use.
    #[serde(default)]
    pub use_pipelining: bool,
    // --- Capture options ---
    /// Enable traffic capture to PSRAM buffer
    #[serde(default)]
    pub capture_enabled: bool,
    /// Capture buffer size in bytes (16KB - 6MB)
    #[serde(default = "default_capture_buffer_size")]
    pub capture_buffer_size: u32,
}

// Serde wants functions for default values
fn default_dongle_ip() -> String {
    "192.168.0.10".to_string()
}

const fn default_dongle_port() -> u16 {
    35000
}

const fn default_listen_port() -> u16 {
    35000
}

const fn default_slow_poll_interval_ms() -> u64 {
    450
}

const fn default_slow_poll_ratio() -> u32 {
    6
}

const fn default_promotion_wait_threshold_ms() -> u64 {
    40
}

const fn default_fast_demotion_ms() -> u64 {
    2000
}

const fn default_pid_inactive_removal_ms() -> u64 {
    4000
}

const fn default_max_pids_per_query() -> u8 {
    6
}

const fn default_capture_buffer_size() -> u32 {
    4 * 1024 * 1024
}

impl Default for Obd2Config {
    fn default() -> Self {
        Self {
            dongle_ip: default_dongle_ip(),
            dongle_port: default_dongle_port(),
            listen_port: default_listen_port(),
            slow_poll_mode: SlowPollMode::default(),
            slow_poll_interval_ms: default_slow_poll_interval_ms(),
            slow_poll_ratio: default_slow_poll_ratio(),
            promotion_wait_threshold_ms: default_promotion_wait_threshold_ms(),
            fast_demotion_ms: default_fast_demotion_ms(),
            pid_inactive_removal_ms: default_pid_inactive_removal_ms(),
            use_multi_pid: false,
            max_pids_per_query: default_max_pids_per_query(),
            use_repeat: false,
            repeat_string: String::new(),
            use_framing: false,
            use_pipelining: false,
            capture_enabled: false,
            capture_buffer_size: default_capture_buffer_size(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub wifi: WifiConfig,
    #[serde(default)]
    pub ip: IpConfig,
    #[serde(default)]
    pub obd2: Obd2Config,
    /// AP SSID (defaults to "TachTalk-XXXX" where XXXX is derived from MAC)
    #[serde(default = "default_ap_ssid")]
    pub ap_ssid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ap_password: Option<String>,
    /// AP IP address for the access point interface
    #[serde(default = "default_ap_ip")]
    pub ap_ip: Ipv4Addr,
    /// AP subnet prefix length (e.g., 24 for /24)
    #[serde(default = "default_ap_prefix_len")]
    pub ap_prefix_len: u8,
    #[serde(default)]
    pub log_level: LogLevel,
    /// LED profiles (named collections of LED rules)
    #[serde(default = "default_profiles")]
    pub profiles: Vec<LedProfile>,
    /// Index of the currently active profile
    #[serde(default)]
    pub active_profile: usize,
    pub total_leds: usize,
    #[serde(default = "default_led_gpio")]
    pub led_gpio: u8,
    #[serde(default = "default_obd2_timeout_ms")]
    pub obd2_timeout_ms: u64,
    /// LED brightness (0-255)
    #[serde(default = "default_brightness")]
    pub brightness: u8,
    /// Dump CPU metrics to serial console every 5 seconds
    #[serde(default)]
    pub dump_cpu_metrics: bool,
    /// Dump socket info to serial console every 5 seconds
    #[serde(default)]
    pub dump_socket_info: bool,
    /// Rotary encoder pin A (CLK) - set to 0 to disable encoder
    #[serde(default)]
    pub encoder_pin_a: u8,
    /// Rotary encoder pin B (DT) - set to 0 to disable encoder
    #[serde(default)]
    pub encoder_pin_b: u8,
    /// Profile switch button pin - set to 0 to disable button
    #[serde(default)]
    pub button_pin: u8,
    /// Status LED red (WiFi) GPIO pin - set to 0 to disable
    #[serde(default = "default_status_led_red_pin")]
    pub status_led_red_pin: u8,
    /// Status LED yellow (dongle) GPIO pin - set to 0 to disable
    #[serde(default = "default_status_led_yellow_pin")]
    pub status_led_yellow_pin: u8,
    /// Status LED green (clients) GPIO pin - set to 0 to disable
    #[serde(default = "default_status_led_green_pin")]
    pub status_led_green_pin: u8,
    /// Status LED activity timeout in ms (LED returns to solid-on after this long without activity)
    #[serde(default = "default_status_led_flicker_ms")]
    pub status_led_flicker_ms: u16,
    /// Turn off RGB LEDs after this many ms without an RPM update (0 = disabled)
    #[serde(default = "default_rpm_stale_timeout_ms")]
    pub rpm_stale_timeout_ms: u16,
}

const fn default_led_gpio() -> u8 {
    48
}

const fn default_status_led_red_pin() -> u8 {
    9
}

const fn default_status_led_yellow_pin() -> u8 {
    10
}

const fn default_status_led_green_pin() -> u8 {
    11
}

const fn default_status_led_flicker_ms() -> u16 {
    200
}

const fn default_rpm_stale_timeout_ms() -> u16 {
    1000
}

const fn default_brightness() -> u8 {
    255
}

fn default_ap_ip() -> Ipv4Addr {
    Ipv4Addr::new(10, 15, 25, 1)
}

const fn default_ap_prefix_len() -> u8 {
    24
}

fn default_profile() -> LedProfile {
    LedProfile {
        name: "Default".to_string(),
        rules: vec![
            LedRule {
                name: "Blue".to_string(),
                rpm_lower: 1000,
                rpm_upper: None,
                start_led: 0,
                end_led: 2,
                colors: smallvec::smallvec![RGB8::new(0, 0, 255)],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Green".to_string(),
                rpm_lower: 1500,
                rpm_upper: None,
                start_led: 3,
                end_led: 5,
                colors: smallvec::smallvec![RGB8::new(0, 255, 0)],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Yellow".to_string(),
                rpm_lower: 2000,
                rpm_upper: None,
                start_led: 6,
                end_led: 8,
                colors: smallvec::smallvec![RGB8::new(255, 255, 0)],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Red".to_string(),
                rpm_lower: 2500,
                rpm_upper: None,
                start_led: 9,
                end_led: 11,
                colors: smallvec::smallvec![RGB8::new(255, 0, 0)],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Off".to_string(),
                rpm_lower: 3000,
                rpm_upper: None,
                start_led: 0,
                end_led: 11,
                colors: smallvec::smallvec![RGB8::new(0, 0, 0)],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Shift".to_string(),
                rpm_lower: 3000,
                rpm_upper: None,
                start_led: 0,
                end_led: 11,
                colors: smallvec::smallvec![RGB8::new(0, 0, 255)],
                blink: true,
                blink_ms: 500,
            },
        ],
        preview_rpm: 2900,
    }
}

fn rainbow_profile() -> LedProfile {
    LedProfile {
        name: "Rainbow".to_string(),
        rules: vec![
            LedRule {
                name: "Rainbow".to_string(),
                rpm_lower: 1000,
                rpm_upper: Some(3000),
                start_led: 0,
                end_led: 12,
                colors: smallvec::smallvec![
                    RGB8::new(0, 0, 255),
                    RGB8::new(0, 255, 255),
                    RGB8::new(0, 255, 0),
                    RGB8::new(255, 255, 0),
                    RGB8::new(255, 0, 0),
                ],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Shift".to_string(),
                rpm_lower: 3000,
                rpm_upper: None,
                start_led: 0,
                end_led: 12,
                colors: smallvec::smallvec![RGB8::new(0, 0, 255)],
                blink: true,
                blink_ms: 100,
            },
        ],
        preview_rpm: 2900,
    }
}

fn martijn_profile() -> LedProfile {
    LedProfile {
        name: "Martijn".to_string(),
        rules: vec![
            LedRule {
                name: "Left".to_string(),
                rpm_lower: 1000,
                rpm_upper: Some(3000),
                start_led: 0,
                end_led: 5,
                colors: smallvec::smallvec![RGB8::new(0, 0, 255), RGB8::new(255, 0, 0),],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Right".to_string(),
                rpm_lower: 1000,
                rpm_upper: Some(3000),
                start_led: 11,
                end_led: 6,
                colors: smallvec::smallvec![RGB8::new(0, 0, 255), RGB8::new(255, 0, 0),],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Shift".to_string(),
                rpm_lower: 3000,
                rpm_upper: None,
                start_led: 0,
                end_led: 11,
                colors: smallvec::smallvec![RGB8::new(0, 0, 0)],
                blink: true,
                blink_ms: 100,
            },
        ],
        preview_rpm: 2900,
    }
}

fn default_profiles() -> Vec<LedProfile> {
    vec![default_profile(), rainbow_profile(), martijn_profile()]
}

/// Maximum OBD2 timeout to avoid triggering watchdog in dongle task
pub const MAX_OBD2_TIMEOUT_MS: u64 = 4500;

const fn default_obd2_timeout_ms() -> u64 {
    MAX_OBD2_TIMEOUT_MS
}

impl Default for Config {
    fn default() -> Self {
        Self {
            wifi: WifiConfig::default(),
            ip: IpConfig::default(),
            obd2: Obd2Config::default(),
            ap_ssid: default_ap_ssid(),
            ap_password: None,
            ap_ip: default_ap_ip(),
            ap_prefix_len: default_ap_prefix_len(),
            log_level: LogLevel::default(),
            profiles: default_profiles(),
            active_profile: 0,
            total_leds: 1,
            led_gpio: default_led_gpio(),
            obd2_timeout_ms: default_obd2_timeout_ms(),
            brightness: default_brightness(),
            dump_cpu_metrics: false,
            dump_socket_info: false,
            encoder_pin_a: 0, // Disabled by default
            encoder_pin_b: 0,
            button_pin: 0, // Disabled by default
            status_led_red_pin: default_status_led_red_pin(),
            status_led_yellow_pin: default_status_led_yellow_pin(),
            status_led_green_pin: default_status_led_green_pin(),
            status_led_flicker_ms: default_status_led_flicker_ms(),
            rpm_stale_timeout_ms: default_rpm_stale_timeout_ms(),
        }
    }
}

impl Config {
    /// Get the LED rules from the active profile
    #[must_use]
    pub fn active_rules(&self) -> &[LedRule] {
        self.profiles
            .get(self.active_profile)
            .map_or(&[], |p| p.rules.as_slice())
    }

    /// Clamp values to valid ranges and fix invalid values
    pub fn validate(&mut self) {
        if self.obd2_timeout_ms > MAX_OBD2_TIMEOUT_MS {
            warn!(
                "Clamping obd2_timeout_ms from {} to {}",
                self.obd2_timeout_ms, MAX_OBD2_TIMEOUT_MS
            );
            self.obd2_timeout_ms = MAX_OBD2_TIMEOUT_MS;
        }
        if self.wifi.ssid.is_empty() {
            warn!("WiFi SSID is empty, resetting to default");
            self.wifi = WifiConfig::default();
        }
        if self.ap_ssid.is_empty() {
            warn!("AP SSID is empty, resetting to default");
            self.ap_ssid = default_ap_ssid();
        }
        if self.ip.ip.is_empty() {
            warn!("Static IP is empty, resetting to default");
            self.ip.ip = default_static_ip();
        }
        // Clamp capture buffer size (16KB - 6MB). Upper bound preserves
        // ~2MB of the 8MB PSRAM for TLS, WiFi, and heap allocations.
        if self.obd2.capture_buffer_size < 16 * 1024 {
            self.obd2.capture_buffer_size = 16 * 1024;
        } else if self.obd2.capture_buffer_size > 6 * 1024 * 1024 {
            self.obd2.capture_buffer_size = 6 * 1024 * 1024;
        }
    }

    pub fn load_or_default() -> Self {
        match Self::load() {
            Ok(mut config) => {
                info!("Loaded config from NVS");
                config.validate();
                config
            }
            Err(e) => {
                warn!("Failed to load config from NVS: {e}, using defaults");
                Self::default()
            }
        }
    }

    pub fn load() -> Result<Self> {
        debug!("Loading config from NVS");
        let nvs_guard = NVS.lock().unwrap();
        let nvs = nvs_guard.as_ref().ok_or(Error::NvsNotInitialized)?;

        // Get the blob length first
        let len = nvs.blob_len(NVS_CONFIG_KEY)?;
        if let Some(len) = len {
            debug!("Config blob size: {len} bytes");
            let mut buf = vec![0u8; len];
            nvs.get_blob(NVS_CONFIG_KEY, &mut buf)?;
            let config: Config = serde_json::from_slice(&buf)?;
            debug!(
                "Config parsed: wifi.ssid={:?}, log_level={:?}, led_gpio={}",
                config.wifi.ssid, config.log_level, config.led_gpio
            );
            Ok(config)
        } else {
            Err(Error::NvsConfigNotFound)
        }
    }

    pub fn save(&self) -> Result<()> {
        debug!("Saving config to NVS");
        let mut nvs_guard = NVS.lock().unwrap();
        let nvs = nvs_guard.as_mut().ok_or(Error::NvsNotInitialized)?;

        let json = serde_json::to_vec(self)?;
        debug!("Config JSON size: {} bytes", json.len());
        nvs.set_blob(NVS_CONFIG_KEY, &json)?;
        info!("Config saved to NVS");
        Ok(())
    }
}
