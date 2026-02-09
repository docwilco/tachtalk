use anyhow::{anyhow, Result};
use esp_idf_svc::nvs::{EspNvs, EspNvsPartition, NvsDefault};
use esp_idf_svc::sys::{esp_mac_type_t_ESP_MAC_WIFI_STA, esp_read_mac};
use log::{debug, info, warn, LevelFilter};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
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

/// Options sent with the Start command (not persisted in NVS).
#[derive(Debug, Default, Clone, Deserialize)]
pub struct StartOptions {
    #[serde(default)]
    pub query_mode: QueryMode,
    #[serde(default)]
    pub use_multi_pid: bool,
    #[serde(default)]
    pub use_repeat: bool,
    /// Repeat command string. Empty = bare CR (ELM327 spec), `"1"` = common WiFi dongle convention.
    #[serde(default)]
    pub repeat_string: String,
    #[serde(default)]
    pub use_framing: bool,
}

/// Response data byte count for each Mode 01 PID (excludes service byte and PID byte).
/// Indexed by PID byte. `0` = unknown/unsupported PID.
/// Source: SAE J1979 / ISO 15031-5.
#[rustfmt::skip]
const MODE01_PID_DATA_LENGTHS: [u8; 256] = {
    let mut t = [0u8; 256];
    // 0x00-0x20: PIDs supported bitmasks and basic engine data
    t[0x00] = 4; // PIDs supported [01-20]
    t[0x01] = 4; // Monitor status since DTCs cleared
    t[0x02] = 2; // Freeze DTC
    t[0x03] = 2; // Fuel system status
    t[0x04] = 1; // Calculated engine load
    t[0x05] = 1; // Engine coolant temperature
    t[0x06] = 1; // Short term fuel trim — Bank 1
    t[0x07] = 1; // Long term fuel trim — Bank 1
    t[0x08] = 1; // Short term fuel trim — Bank 2
    t[0x09] = 1; // Long term fuel trim — Bank 2
    t[0x0A] = 1; // Fuel pressure
    t[0x0B] = 1; // Intake manifold absolute pressure
    t[0x0C] = 2; // Engine RPM
    t[0x0D] = 1; // Vehicle speed
    t[0x0E] = 1; // Timing advance
    t[0x0F] = 1; // Intake air temperature
    t[0x10] = 2; // MAF air flow rate
    t[0x11] = 1; // Throttle position
    t[0x12] = 1; // Commanded secondary air status
    t[0x13] = 1; // O2 sensors present (2 banks)
    t[0x14] = 2; // O2 sensor 1 — voltage & trim
    t[0x15] = 2; // O2 sensor 2
    t[0x16] = 2; // O2 sensor 3
    t[0x17] = 2; // O2 sensor 4
    t[0x18] = 2; // O2 sensor 5
    t[0x19] = 2; // O2 sensor 6
    t[0x1A] = 2; // O2 sensor 7
    t[0x1B] = 2; // O2 sensor 8
    t[0x1C] = 1; // OBD standards this vehicle conforms to
    t[0x1D] = 1; // O2 sensors present (4 banks)
    t[0x1E] = 1; // Auxiliary input status
    t[0x1F] = 2; // Run time since engine start
    // 0x20-0x40
    t[0x20] = 4; // PIDs supported [21-40]
    t[0x21] = 2; // Distance traveled with MIL on
    t[0x22] = 2; // Fuel rail pressure (relative to manifold vacuum)
    t[0x23] = 2; // Fuel rail gauge pressure (diesel/GDI)
    t[0x24] = 4; // O2 sensor 1 — equiv ratio & voltage
    t[0x25] = 4; // O2 sensor 2
    t[0x26] = 4; // O2 sensor 3
    t[0x27] = 4; // O2 sensor 4
    t[0x28] = 4; // O2 sensor 5
    t[0x29] = 4; // O2 sensor 6
    t[0x2A] = 4; // O2 sensor 7
    t[0x2B] = 4; // O2 sensor 8
    t[0x2C] = 1; // Commanded EGR
    t[0x2D] = 1; // EGR error
    t[0x2E] = 1; // Commanded evaporative purge
    t[0x2F] = 1; // Fuel tank level input
    t[0x30] = 1; // Warm-ups since codes cleared
    t[0x31] = 2; // Distance traveled since codes cleared
    t[0x32] = 2; // Evap system vapor pressure
    t[0x33] = 1; // Absolute barometric pressure
    t[0x34] = 4; // O2 sensor 1 — equiv ratio & current
    t[0x35] = 4; // O2 sensor 2
    t[0x36] = 4; // O2 sensor 3
    t[0x37] = 4; // O2 sensor 4
    t[0x38] = 4; // O2 sensor 5
    t[0x39] = 4; // O2 sensor 6
    t[0x3A] = 4; // O2 sensor 7
    t[0x3B] = 4; // O2 sensor 8
    t[0x3C] = 2; // Catalyst temperature: Bank 1, Sensor 1
    t[0x3D] = 2; // Catalyst temperature: Bank 2, Sensor 1
    t[0x3E] = 2; // Catalyst temperature: Bank 1, Sensor 2
    t[0x3F] = 2; // Catalyst temperature: Bank 2, Sensor 2
    // 0x40-0x60
    t[0x40] = 4; // PIDs supported [41-60]
    t[0x41] = 4; // Monitor status this drive cycle
    t[0x42] = 2; // Control module voltage
    t[0x43] = 2; // Absolute load value
    t[0x44] = 2; // Fuel-air commanded equivalence ratio
    t[0x45] = 1; // Relative throttle position
    t[0x46] = 1; // Ambient air temperature
    t[0x47] = 1; // Absolute throttle position B
    t[0x48] = 1; // Absolute throttle position C
    t[0x49] = 1; // Accelerator pedal position D
    t[0x4A] = 1; // Accelerator pedal position E
    t[0x4B] = 1; // Accelerator pedal position F
    t[0x4C] = 1; // Commanded throttle actuator
    t[0x4D] = 2; // Time run with MIL on
    t[0x4E] = 2; // Time since trouble codes cleared
    t[0x4F] = 4; // Max values (equiv ratio, O2 voltage, O2 current, intake pressure)
    t[0x50] = 4; // Max air flow rate from MAF sensor
    t[0x51] = 1; // Fuel type
    t[0x52] = 1; // Ethanol fuel %
    t[0x53] = 2; // Absolute evap system vapor pressure
    t[0x54] = 2; // Evap system vapor pressure
    t[0x55] = 2; // Short term secondary O2 trim — Bank 1 & 3
    t[0x56] = 2; // Long term secondary O2 trim — Bank 1 & 3
    t[0x57] = 2; // Short term secondary O2 trim — Bank 2 & 4
    t[0x58] = 2; // Long term secondary O2 trim — Bank 2 & 4
    t[0x59] = 2; // Fuel rail absolute pressure
    t[0x5A] = 1; // Relative accelerator pedal position
    t[0x5B] = 1; // Hybrid battery pack remaining life
    t[0x5C] = 1; // Engine oil temperature
    t[0x5D] = 2; // Fuel injection timing
    t[0x5E] = 2; // Engine fuel rate
    t[0x5F] = 1; // Emission requirements
    // 0x60-0x80
    t[0x60] = 4; // PIDs supported [61-80]
    t[0x61] = 1; // Driver's demand engine — percent torque
    t[0x62] = 1; // Actual engine — percent torque
    t[0x63] = 2; // Engine reference torque
    t[0x64] = 5; // Engine percent torque data
    t[0x65] = 2; // Auxiliary input / output supported
    // 0x80-0xA0
    t[0x80] = 4; // PIDs supported [81-A0]
    // 0xA0-0xC0
    t[0xA0] = 4; // PIDs supported [A1-C0]
    // 0xC0-0xE0
    t[0xC0] = 4; // PIDs supported [C1-E0]
    t
};

/// Look up the response data byte count for a Mode 01 PID.
/// Returns 0 for unknown/unsupported PIDs.
#[must_use]
pub const fn pid_data_length(pid: u8) -> u8 {
    MODE01_PID_DATA_LENGTHS[pid as usize]
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
            capture_buffer_size: default_capture_buffer_size(),
            capture_overflow: CaptureOverflow::default(),
            obd2_timeout_ms: default_obd2_timeout_ms(),
        }
    }
}

impl TestConfig {
    /// Parse fast PIDs from comma-separated string (e.g. `"010C,0149"` → `[0x0C, 0x49]`).
    ///
    /// Assumes all PIDs are Mode 01. Strips the leading `01` service byte and parses
    /// the remaining two hex digits as a `u8`. Silently skips entries that are too
    /// short or contain invalid hex.
    pub fn get_fast_pids(&self) -> SmallVec<[u8; 8]> {
        Self::parse_pid_list(&self.fast_pids)
    }

    /// Parse slow PIDs from comma-separated string (e.g. `"0105"` → `[0x05]`).
    ///
    /// See [`Self::get_fast_pids`] for format details.
    pub fn get_slow_pids(&self) -> SmallVec<[u8; 8]> {
        Self::parse_pid_list(&self.slow_pids)
    }

    /// Parse a comma-separated PID string into a list of PID bytes.
    fn parse_pid_list(s: &str) -> SmallVec<[u8; 8]> {
        s.split(',')
            .filter_map(|entry| {
                let trimmed = entry.trim();
                // Expect at least 4 chars: "01XX"
                if trimmed.len() < 4 {
                    return None;
                }
                u8::from_str_radix(&trimmed[2..4], 16).ok()
            })
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
