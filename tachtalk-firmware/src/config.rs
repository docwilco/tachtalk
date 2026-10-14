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
pub use tachtalk_shift_lights_lib::{LedRule, PidPriority, ProfileType, RGB8};

/// A named collection of LED rule configurations bound to an OBD2 PID.
///
/// Profiles allow users to switch between different shift light setups
/// (e.g., "Street", "Track", "Economy") or define overlays (e.g.,
/// coolant temperature warning) and triggered profiles (e.g., pit lane
/// speed limiter).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedProfile {
    pub name: String,
    pub rules: Vec<LedRule>,
    /// Decoded PID value to show when previewing this profile
    #[serde(default = "default_preview_value")]
    pub preview_value: u32,
    /// OBD2 Mode 01 PID this profile is bound to (default: 0x0C = RPM)
    #[serde(default = "default_pid")]
    pub pid: u8,
    /// Polling priority for this profile's PID
    #[serde(default)]
    pub pid_priority: PidPriority,
    /// How this profile is activated
    #[serde(default)]
    pub profile_type: ProfileType,
    /// GPIO pin for Triggered profiles (0 = disabled)
    #[serde(default)]
    pub button_pin: u8,
    /// Auto-disable Triggered profiles when decoded value exceeds this threshold
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_disable_above: Option<u32>,
}

const fn default_preview_value() -> u32 {
    3000
}

const fn default_pid() -> u8 {
    0x0C
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
const NVS_ADMIN_PW_KEY: &str = "admin_pw";

// Global NVS handle - initialized once in main
static NVS: Mutex<Option<EspNvs<NvsDefault>>> = Mutex::new(None);

/// Run a closure that writes to NVS on a short-lived thread with an
/// internal-RAM stack.  SPI flash writes disable the cache, so they cannot
/// run on a thread whose stack is in PSRAM (same SPI bus).
fn nvs_write_on_internal_stack<F>(f: F) -> Result<()>
where
    F: FnOnce() -> Result<()> + Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    crate::thread_util::spawn_named(
        c"nvs_wr",
        4096,
        crate::thread_util::StackMemory::Internal,
        move || {
            let result = f();
            let _ = tx.send(result);
        },
    );
    rx.recv().map_err(|_| Error::NvsWriteThreadPanicked)?
}

pub fn init_nvs(nvs_partition: EspNvsPartition<NvsDefault>) -> Result<()> {
    debug!("Initializing NVS namespace: {NVS_NAMESPACE}");
    let nvs = EspNvs::new(nvs_partition, NVS_NAMESPACE, true)?;
    *NVS.lock().unwrap() = Some(nvs);
    info!("NVS initialized");
    Ok(())
}

/// Load the admin password hash from its own NVS key (separate from config).
pub fn load_admin_password_hash() -> Option<String> {
    let nvs_guard = NVS.lock().unwrap();
    let nvs = nvs_guard.as_ref()?;
    let len = nvs.str_len(NVS_ADMIN_PW_KEY).ok()??;
    let mut buf = vec![0u8; len];
    nvs.get_str(NVS_ADMIN_PW_KEY, &mut buf)
        .ok()?
        .map(String::from)
}

/// Save (or clear) the admin password hash in its own NVS key.
pub fn save_admin_password_hash(hash: Option<&str>) -> Result<()> {
    let hash = hash.map(String::from);
    nvs_write_on_internal_stack(move || {
        let mut nvs_guard = NVS.lock().unwrap();
        let nvs = nvs_guard.as_mut().ok_or(Error::NvsNotInitialized)?;
        match hash.as_deref() {
            Some(h) => nvs.set_str(NVS_ADMIN_PW_KEY, h)?,
            None => {
                nvs.remove(NVS_ADMIN_PW_KEY)?;
            }
        }
        Ok(())
    })
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
    /// Repeat identical consecutive queries with a bare CR (the ELM327
    /// datasheet's repeat); support is detected on first use
    #[serde(default)]
    pub use_repeat: bool,
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
    #[serde(default = "default_total_leds")]
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
    #[serde(default = "default_encoder_pin_a")]
    pub encoder_pin_a: u8,
    /// Rotary encoder pin B (DT) - set to 0 to disable encoder
    #[serde(default = "default_encoder_pin_b")]
    pub encoder_pin_b: u8,
    /// Profile switch button pin - set to 0 to disable button
    #[serde(default = "default_button_pin")]
    pub button_pin: u8,
    /// Encoder 2x acceleration threshold in ms. If consecutive encoder
    /// events arrive within this window the brightness step is doubled.
    /// Set to 0 to disable 2x acceleration.
    #[serde(default = "default_encoder_accel_2x_ms")]
    pub encoder_accel_2x_ms: u16,
    /// Encoder 4x acceleration threshold in ms. If consecutive encoder
    /// events arrive within this (shorter) window the brightness step is
    /// quadrupled instead. Set to 0 to disable 4x acceleration.
    #[serde(default = "default_encoder_accel_4x_ms")]
    pub encoder_accel_4x_ms: u16,
    /// Status LED chain (3x WS2812B: WIFI, OBD, CLIENT) data GPIO - set to 0 to disable
    #[serde(default = "default_status_led_gpio")]
    pub status_led_gpio: u8,
    /// Status LED brightness (0-255)
    #[serde(default = "default_status_led_brightness")]
    pub status_led_brightness: u8,
    /// Generic red boot LED GPIO - blinks during boot animation, 0 to disable
    #[serde(default = "default_boot_led_gpio")]
    pub boot_led_gpio: u8,
    /// Status LED activity timeout in ms (LED returns to solid-on after this long without activity)
    #[serde(default = "default_status_led_flicker_ms")]
    pub status_led_flicker_ms: u16,
    /// Turn off RGB LEDs after this many ms without an RPM update (0 = disabled)
    #[serde(default = "default_rpm_stale_timeout_ms")]
    pub rpm_stale_timeout_ms: u16,
    /// Duration (ms) to show `preview_value` after brightness/profile change
    #[serde(default = "default_preview_duration_ms")]
    pub preview_duration_ms: u16,
}

const fn default_led_gpio() -> u8 {
    3
}

const fn default_total_leds() -> usize {
    12
}

const fn default_encoder_pin_a() -> u8 {
    43
}

const fn default_encoder_pin_b() -> u8 {
    2
}

const fn default_button_pin() -> u8 {
    44
}

const fn default_status_led_gpio() -> u8 {
    7
}

const fn default_status_led_brightness() -> u8 {
    64
}

const fn default_boot_led_gpio() -> u8 {
    46
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

const fn default_preview_duration_ms() -> u16 {
    500
}

const fn default_encoder_accel_2x_ms() -> u16 {
    80
}

const fn default_encoder_accel_4x_ms() -> u16 {
    30
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
                value_lower: 4000,
                value_upper: None,
                start_led: 0,
                end_led: 2,
                colors: smallvec::smallvec![RGB8::new(0, 0, 255)],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Green".to_string(),
                value_lower: 6000,
                value_upper: None,
                start_led: 3,
                end_led: 5,
                colors: smallvec::smallvec![RGB8::new(0, 255, 0)],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Yellow".to_string(),
                value_lower: 8000,
                value_upper: None,
                start_led: 6,
                end_led: 8,
                colors: smallvec::smallvec![RGB8::new(255, 255, 0)],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Red".to_string(),
                value_lower: 10000,
                value_upper: None,
                start_led: 9,
                end_led: 11,
                colors: smallvec::smallvec![RGB8::new(255, 0, 0)],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Off".to_string(),
                value_lower: 12000,
                value_upper: None,
                start_led: 0,
                end_led: 11,
                colors: smallvec::smallvec![RGB8::new(0, 0, 0)],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Shift".to_string(),
                value_lower: 12000,
                value_upper: None,
                start_led: 0,
                end_led: 11,
                colors: smallvec::smallvec![RGB8::new(0, 0, 255)],
                blink: true,
                blink_ms: 500,
            },
        ],
        preview_value: 11600,
        pid: 0x0C,
        pid_priority: PidPriority::Fast,
        profile_type: ProfileType::Normal,
        button_pin: 0,
        auto_disable_above: None,
    }
}

fn rainbow_profile() -> LedProfile {
    LedProfile {
        name: "Rainbow".to_string(),
        rules: vec![
            LedRule {
                name: "Rainbow".to_string(),
                value_lower: 4000,
                value_upper: Some(12000),
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
                value_lower: 12000,
                value_upper: None,
                start_led: 0,
                end_led: 12,
                colors: smallvec::smallvec![RGB8::new(0, 0, 255)],
                blink: true,
                blink_ms: 100,
            },
        ],
        preview_value: 11600,
        pid: 0x0C,
        pid_priority: PidPriority::Fast,
        profile_type: ProfileType::Normal,
        button_pin: 0,
        auto_disable_above: None,
    }
}

fn martijn_profile() -> LedProfile {
    LedProfile {
        name: "Martijn".to_string(),
        rules: vec![
            LedRule {
                name: "Left".to_string(),
                value_lower: 4000,
                value_upper: Some(12000),
                start_led: 0,
                end_led: 5,
                colors: smallvec::smallvec![RGB8::new(0, 0, 255), RGB8::new(255, 0, 0),],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Right".to_string(),
                value_lower: 4000,
                value_upper: Some(12000),
                start_led: 11,
                end_led: 6,
                colors: smallvec::smallvec![RGB8::new(0, 0, 255), RGB8::new(255, 0, 0),],
                blink: false,
                blink_ms: 500,
            },
            LedRule {
                name: "Shift".to_string(),
                value_lower: 12000,
                value_upper: None,
                start_led: 0,
                end_led: 11,
                colors: smallvec::smallvec![RGB8::new(0, 0, 0)],
                blink: true,
                blink_ms: 100,
            },
        ],
        preview_value: 11600,
        pid: 0x0C,
        pid_priority: PidPriority::Fast,
        profile_type: ProfileType::Normal,
        button_pin: 0,
        auto_disable_above: None,
    }
}

fn coolant_warning_profile() -> LedProfile {
    LedProfile {
        name: "Coolant Warning".to_string(),
        rules: vec![LedRule {
            name: "Overheat".to_string(),
            value_lower: 150,
            value_upper: None,
            start_led: 0,
            end_led: 11,
            colors: smallvec::smallvec![RGB8::new(255, 0, 0)],
            blink: true,
            blink_ms: 200,
        }],
        preview_value: 160,
        pid: 0x05,
        pid_priority: PidPriority::Slow,
        profile_type: ProfileType::Overlay,
        button_pin: 0,
        auto_disable_above: None,
    }
}

fn default_profiles() -> Vec<LedProfile> {
    vec![
        default_profile(),
        rainbow_profile(),
        martijn_profile(),
        coolant_warning_profile(),
    ]
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
            total_leds: default_total_leds(),
            led_gpio: default_led_gpio(),
            obd2_timeout_ms: default_obd2_timeout_ms(),
            brightness: default_brightness(),
            dump_cpu_metrics: true,
            dump_socket_info: false,
            encoder_pin_a: default_encoder_pin_a(),
            encoder_pin_b: default_encoder_pin_b(),
            button_pin: default_button_pin(),
            encoder_accel_2x_ms: default_encoder_accel_2x_ms(),
            encoder_accel_4x_ms: default_encoder_accel_4x_ms(),
            status_led_gpio: default_status_led_gpio(),
            status_led_brightness: default_status_led_brightness(),
            boot_led_gpio: default_boot_led_gpio(),
            status_led_flicker_ms: default_status_led_flicker_ms(),
            rpm_stale_timeout_ms: default_rpm_stale_timeout_ms(),
            preview_duration_ms: default_preview_duration_ms(),
        }
    }
}

impl Config {
    /// Get the active Normal profile (if any)
    #[must_use]
    pub fn active_normal_profile(&self) -> Option<&LedProfile> {
        self.profiles
            .get(self.active_profile)
            .filter(|p| p.profile_type == ProfileType::Normal)
    }

    /// Get the LED rules from the active Normal profile
    #[must_use]
    pub fn active_rules(&self) -> &[LedRule] {
        self.active_normal_profile()
            .map_or(&[], |p| p.rules.as_slice())
    }

    /// Iterate over all Overlay profiles
    pub fn overlay_profiles(&self) -> impl Iterator<Item = (usize, &LedProfile)> {
        self.profiles
            .iter()
            .enumerate()
            .filter(|(_, p)| p.profile_type == ProfileType::Overlay)
    }

    /// Iterate over all Triggered profiles
    pub fn triggered_profiles(&self) -> impl Iterator<Item = (usize, &LedProfile)> {
        self.profiles
            .iter()
            .enumerate()
            .filter(|(_, p)| p.profile_type == ProfileType::Triggered)
    }

    /// Iterate over Triggered profiles that are currently enabled at runtime.
    pub fn enabled_triggered_profiles<'a>(
        &'a self,
        triggered_enabled: &'a [bool],
    ) -> impl Iterator<Item = (usize, &'a LedProfile)> {
        self.triggered_profiles()
            .filter(move |(i, _)| triggered_enabled.get(*i).copied().unwrap_or(false))
    }

    /// Advance `active_profile` to the next Normal profile (wrapping).
    /// Returns the new index, or the current index if no Normal profiles exist.
    pub fn cycle_to_next_normal_profile(&mut self) -> usize {
        let count = self.profiles.len();
        if count == 0 {
            return self.active_profile;
        }
        for offset in 1..=count {
            let idx = (self.active_profile + offset) % count;
            if self.profiles[idx].profile_type == ProfileType::Normal {
                self.active_profile = idx;
                return idx;
            }
        }
        self.active_profile
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
        self.obd2.capture_buffer_size = self
            .obd2
            .capture_buffer_size
            .clamp(16 * 1024, 6 * 1024 * 1024);
        // Validate profile fields
        for profile in &mut self.profiles {
            // Non-Triggered profiles shouldn't have button_pin or auto_disable
            if profile.profile_type != ProfileType::Triggered {
                profile.button_pin = 0;
                profile.auto_disable_above = None;
            }
        }
        // Ensure active_profile points to a valid index
        if self.active_profile >= self.profiles.len() {
            self.active_profile = 0;
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
        debug!("Saving config to NVS (led_gpio={})", self.led_gpio);
        // NVS writes go to SPI flash, which disables the cache.  If the
        // current thread's stack lives in PSRAM (same SPI bus), ESP-IDF
        // aborts.  Bounce the write to a short-lived thread with an
        // internal-RAM stack.
        let json = serde_json::to_vec(self)?;
        nvs_write_on_internal_stack(move || {
            let mut nvs_guard = NVS.lock().unwrap();
            let nvs = nvs_guard.as_mut().ok_or(Error::NvsNotInitialized)?;
            debug!("Config JSON size: {} bytes", json.len());
            nvs.set_blob(NVS_CONFIG_KEY, &json)?;
            info!("Config saved to NVS");
            Ok(())
        })
    }
}
