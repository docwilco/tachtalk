use anyhow::{anyhow, Result};
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
pub use tachtalk_shift_lights_lib::{RGB8, ThresholdConfig};

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

/// OBD2 network configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Obd2Config {
    /// IP address of the OBD2 dongle
    pub dongle_ip: String,
    /// Port of the OBD2 dongle
    pub dongle_port: u16,
    /// Port to listen on for OBD2 clients
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
    /// Whether to test multi-PID query support at boot
    #[serde(default)]
    pub test_multi_pid: bool,
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

impl Default for Obd2Config {
    fn default() -> Self {
        Self {
            dongle_ip: "192.168.0.10".to_string(),
            dongle_port: 35000,
            listen_port: 35000,
            slow_poll_mode: SlowPollMode::default(),
            slow_poll_interval_ms: default_slow_poll_interval_ms(),
            slow_poll_ratio: default_slow_poll_ratio(),
            promotion_wait_threshold_ms: default_promotion_wait_threshold_ms(),
            fast_demotion_ms: default_fast_demotion_ms(),
            pid_inactive_removal_ms: default_pid_inactive_removal_ms(),
            test_multi_pid: false,
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
    pub thresholds: Vec<ThresholdConfig>,
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
}

const fn default_led_gpio() -> u8 {
    48
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
            thresholds: vec![
                ThresholdConfig {
                    name: "Off".to_string(),
                    rpm: 0,
                    start_led: 0,
                    end_led: 0,
                    color: RGB8::new(0, 0, 0),
                    blink: false,
                    blink_ms: 500,
                },
                ThresholdConfig {
                    name: "Blue".to_string(),
                    rpm: 1000,
                    start_led: 0,
                    end_led: 0,
                    color: RGB8::new(0, 0, 255),
                    blink: false,
                    blink_ms: 500,
                },
                ThresholdConfig {
                    name: "Green".to_string(),
                    rpm: 1500,
                    start_led: 0,
                    end_led: 0,
                    color: RGB8::new(0, 255, 0),
                    blink: false,
                    blink_ms: 500,
                },
                ThresholdConfig {
                    name: "Yellow".to_string(),
                    rpm: 2000,
                    start_led: 0,
                    end_led: 0,
                    color: RGB8::new(255, 255, 0),
                    blink: false,
                    blink_ms: 500,
                },
                ThresholdConfig {
                    name: "Red".to_string(),
                    rpm: 2500,
                    start_led: 0,
                    end_led: 0,
                    color: RGB8::new(255, 0, 0),
                    blink: false,
                    blink_ms: 500,
                },
                ThresholdConfig {
                    name: "Off".to_string(),
                    rpm: 3000,
                    start_led: 0,
                    end_led: 0,
                    color: RGB8::new(0, 0, 0),
                    blink: false,
                    blink_ms: 500,
                },
                ThresholdConfig {
                    name: "Shift".to_string(),
                    rpm: 3000,
                    start_led: 0,
                    end_led: 0,
                    color: RGB8::new(0, 0, 255),
                    blink: true,
                    blink_ms: 500,
                },
            ],
            total_leds: 1,
            led_gpio: default_led_gpio(),
            obd2_timeout_ms: default_obd2_timeout_ms(),
            brightness: default_brightness(),
            dump_cpu_metrics: false,
            dump_socket_info: false,
            encoder_pin_a: 0, // Disabled by default
            encoder_pin_b: 0,
        }
    }
}

impl Config {
    /// Clamp values to valid ranges and fix invalid values
    pub fn validate(&mut self) {
        if self.obd2_timeout_ms > MAX_OBD2_TIMEOUT_MS {
            warn!("Clamping obd2_timeout_ms from {} to {}", self.obd2_timeout_ms, MAX_OBD2_TIMEOUT_MS);
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
        let nvs = nvs_guard.as_ref().ok_or_else(|| anyhow!("NVS not initialized"))?;
        
        // Get the blob length first
        let len = nvs.blob_len(NVS_CONFIG_KEY)?;
        if let Some(len) = len {
            debug!("Config blob size: {len} bytes");
            let mut buf = vec![0u8; len];
            nvs.get_blob(NVS_CONFIG_KEY, &mut buf)?;
            let config: Config = serde_json::from_slice(&buf)?;
            debug!("Config parsed: wifi.ssid={:?}, log_level={:?}, led_gpio={}", config.wifi.ssid, config.log_level, config.led_gpio);
            Ok(config)
        } else {
            Err(anyhow!("No config found in NVS"))
        }
    }

    pub fn save(&self) -> Result<()> {
        debug!("Saving config to NVS");
        let mut nvs_guard = NVS.lock().unwrap();
        let nvs = nvs_guard.as_mut().ok_or_else(|| anyhow!("NVS not initialized"))?;
        
        let json = serde_json::to_vec(self)?;
        debug!("Config JSON size: {} bytes", json.len());
        nvs.set_blob(NVS_CONFIG_KEY, &json)?;
        info!("Config saved to NVS");
        Ok(())
    }
}
