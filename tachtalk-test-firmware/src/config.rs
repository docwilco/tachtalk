use anyhow::{anyhow, Result};
use esp_idf_svc::nvs::{EspNvs, EspNvsPartition, NvsDefault};
use esp_idf_svc::sys::{esp_mac_type_t_ESP_MAC_WIFI_STA, esp_read_mac};
use log::{debug, info, warn, LevelFilter};
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;
use std::sync::Mutex;

const AP_SSID_PREFIX: &str = "TachTest-";

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

/// Query mode for OBD2 testing
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QueryMode {
    /// Send PID as-is (e.g., `010C\r`) - baseline
    #[default]
    NoCount,
    /// Append ` 1` to all requests (e.g., `010C 1\r`)
    AlwaysOne,
    /// First request without count to detect ECU count, then use that
    AdaptiveCount,
    /// Send multiple requests before waiting for responses
    Pipelined,
    /// Pure TCP proxy with traffic recording to PSRAM
    RawCapture,
}

/// Method for counting responses in `AdaptiveCount` mode
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResponseCountMethod {
    /// Count occurrences of response header (e.g., `41`)
    #[default]
    CountResponseHeaders,
    /// Count non-empty lines before `>`
    CountLines,
}

/// Overflow behavior for capture buffer
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CaptureOverflow {
    /// Stop capturing when buffer is full
    #[default]
    Stop,
    /// Wrap around and overwrite oldest data
    Wrap,
}

const NVS_NAMESPACE: &str = "tachtest";
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
    /// Subnet prefix length (e.g., 24 for /24)
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

/// OBD2 test configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestConfig {
    /// IP address of the OBD2 dongle
    #[serde(default = "default_dongle_ip")]
    pub dongle_ip: String,
    /// Port of the OBD2 dongle
    #[serde(default = "default_dongle_port")]
    pub dongle_port: u16,
    /// Port to listen on for proxy clients (mode 5)
    #[serde(default = "default_listen_port")]
    pub listen_port: u16,
    /// Fast PIDs to poll (comma-separated, e.g., "010C,0149")
    #[serde(default = "default_fast_pids")]
    pub fast_pids: String,
    /// Slow PIDs to poll (comma-separated, e.g., "0105")
    #[serde(default = "default_slow_pids")]
    pub slow_pids: String,
    /// Pipeline bytes on wire (mode 4)
    #[serde(default = "default_pipeline_bytes")]
    pub pipeline_bytes: u16,
    /// Response count method (mode 3)
    #[serde(default)]
    pub response_count_method: ResponseCountMethod,
    /// Capture buffer size in bytes (mode 5)
    #[serde(default = "default_capture_buffer_size")]
    pub capture_buffer_size: u32,
    /// Capture overflow behavior (mode 5)
    #[serde(default)]
    pub capture_overflow: CaptureOverflow,
    /// OBD2 command timeout in ms
    #[serde(default = "default_obd2_timeout_ms")]
    pub obd2_timeout_ms: u64,
}

fn default_dongle_ip() -> String {
    "192.168.0.10".to_string()
}

const fn default_dongle_port() -> u16 {
    35000
}

const fn default_listen_port() -> u16 {
    35000
}

fn default_fast_pids() -> String {
    "010C,0149".to_string()
}

fn default_slow_pids() -> String {
    "0105".to_string()
}

const fn default_pipeline_bytes() -> u16 {
    64
}

const fn default_capture_buffer_size() -> u32 {
    4 * 1024 * 1024 // 4 MB
}

/// Maximum OBD2 timeout to avoid triggering watchdog
pub const MAX_OBD2_TIMEOUT_MS: u64 = 4500;

const fn default_obd2_timeout_ms() -> u64 {
    MAX_OBD2_TIMEOUT_MS
}

impl Default for TestConfig {
    fn default() -> Self {
        Self {
            dongle_ip: default_dongle_ip(),
            dongle_port: default_dongle_port(),
            listen_port: default_listen_port(),
            fast_pids: default_fast_pids(),
            slow_pids: default_slow_pids(),
            pipeline_bytes: default_pipeline_bytes(),
            response_count_method: ResponseCountMethod::default(),
            capture_buffer_size: default_capture_buffer_size(),
            capture_overflow: CaptureOverflow::default(),
            obd2_timeout_ms: default_obd2_timeout_ms(),
        }
    }
}

impl TestConfig {
    /// Parse fast PIDs from comma-separated string
    pub fn get_fast_pids(&self) -> Vec<String> {
        self.fast_pids
            .split(',')
            .map(|s| s.trim().to_uppercase())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Parse slow PIDs from comma-separated string
    pub fn get_slow_pids(&self) -> Vec<String> {
        self.slow_pids
            .split(',')
            .map(|s| s.trim().to_uppercase())
            .filter(|s| !s.is_empty())
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub wifi: WifiConfig,
    #[serde(default)]
    pub ip: IpConfig,
    #[serde(default)]
    pub test: TestConfig,
    /// AP SSID (defaults to "TachTest-XXXX" where XXXX is derived from MAC)
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
    /// Dump CPU metrics to serial console every 5 seconds
    #[serde(default)]
    pub dump_cpu_metrics: bool,
    /// Dump socket info to serial console every 5 seconds
    #[serde(default)]
    pub dump_socket_info: bool,
}

fn default_ap_ip() -> Ipv4Addr {
    Ipv4Addr::new(10, 15, 25, 1)
}

const fn default_ap_prefix_len() -> u8 {
    24
}

impl Default for Config {
    fn default() -> Self {
        Self {
            wifi: WifiConfig::default(),
            ip: IpConfig::default(),
            test: TestConfig::default(),
            ap_ssid: default_ap_ssid(),
            ap_password: None,
            ap_ip: default_ap_ip(),
            ap_prefix_len: default_ap_prefix_len(),
            log_level: LogLevel::default(),
            dump_cpu_metrics: false,
            dump_socket_info: false,
        }
    }
}

impl Config {
    /// Clamp values to valid ranges and fix invalid values
    pub fn validate(&mut self) {
        if self.test.obd2_timeout_ms > MAX_OBD2_TIMEOUT_MS {
            warn!(
                "Clamping obd2_timeout_ms from {} to {}",
                self.test.obd2_timeout_ms, MAX_OBD2_TIMEOUT_MS
            );
            self.test.obd2_timeout_ms = MAX_OBD2_TIMEOUT_MS;
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
        // Clamp capture buffer size to reasonable bounds (1MB - 6MB)
        if self.test.capture_buffer_size < 1024 * 1024 {
            self.test.capture_buffer_size = 1024 * 1024;
        } else if self.test.capture_buffer_size > 6 * 1024 * 1024 {
            self.test.capture_buffer_size = 6 * 1024 * 1024;
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
        let nvs = nvs_guard
            .as_ref()
            .ok_or_else(|| anyhow!("NVS not initialized"))?;

        // Get the blob length first
        let len = nvs.blob_len(NVS_CONFIG_KEY)?;
        if let Some(len) = len {
            debug!("Config blob size: {len} bytes");
            let mut buf = vec![0u8; len];
            nvs.get_blob(NVS_CONFIG_KEY, &mut buf)?;
            let config: Config = serde_json::from_slice(&buf)?;
            debug!(
                "Config parsed: wifi.ssid={:?}, log_level={:?}",
                config.wifi.ssid, config.log_level
            );
            Ok(config)
        } else {
            Err(anyhow!("No config found in NVS"))
        }
    }

    pub fn save(&self) -> Result<()> {
        debug!("Saving config to NVS");
        let mut nvs_guard = NVS.lock().unwrap();
        let nvs = nvs_guard
            .as_mut()
            .ok_or_else(|| anyhow!("NVS not initialized"))?;

        let json = serde_json::to_vec(self)?;
        debug!("Config JSON size: {} bytes", json.len());
        nvs.set_blob(NVS_CONFIG_KEY, &json)?;
        info!("Config saved to NVS");
        Ok(())
    }
}
