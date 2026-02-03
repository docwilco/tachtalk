use anyhow::{anyhow, Result};
use esp_idf_svc::nvs::{EspNvs, EspNvsPartition, NvsDefault};
use log::{debug, info, warn, LevelFilter};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WifiConfig {
    pub ssid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

impl WifiConfig {
    pub fn is_configured(&self) -> bool {
        !self.ssid.is_empty()
    }

    pub fn new_default() -> Self {
        Self {
            ssid: "V-LINK".to_string(),
            password: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpConfig {
    pub use_dhcp: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subnet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dns: Option<String>,
}

impl Default for IpConfig {
    fn default() -> Self {
        Self {
            use_dhcp: true,
            ip: None,
            gateway: None,
            subnet: None,
            dns: None,
        }
    }
}

impl IpConfig {
    /// Default static IP when not using DHCP on the dongle network
    pub const DEFAULT_STATIC_IP: &'static str = "192.168.0.20";
    pub const DEFAULT_GATEWAY: &'static str = "192.168.0.1";
    pub const DEFAULT_SUBNET: &'static str = "255.255.255.0";

    /// Get IP address, using default static IP if static mode but no IP configured
    #[must_use]
    pub fn effective_ip(&self) -> Option<&str> {
        if self.use_dhcp {
            None
        } else {
            Some(self.ip.as_deref().unwrap_or(Self::DEFAULT_STATIC_IP))
        }
    }

    /// Get gateway, using default if static mode but not configured
    #[must_use]
    pub fn effective_gateway(&self) -> Option<&str> {
        if self.use_dhcp {
            None
        } else {
            Some(self.gateway.as_deref().unwrap_or(Self::DEFAULT_GATEWAY))
        }
    }

    /// Get subnet mask, using default if static mode but not configured
    #[must_use]
    pub fn effective_subnet(&self) -> Option<&str> {
        if self.use_dhcp {
            None
        } else {
            Some(self.subnet.as_deref().unwrap_or(Self::DEFAULT_SUBNET))
        }
    }
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
}

impl Default for Obd2Config {
    fn default() -> Self {
        Self {
            dongle_ip: "192.168.0.10".to_string(),
            dongle_port: 35000,
            listen_port: 35000,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ap_password: Option<String>,
    #[serde(default)]
    pub log_level: LogLevel,
    pub thresholds: Vec<ThresholdConfig>,
    pub total_leds: usize,
    #[serde(default = "default_led_gpio")]
    pub led_gpio: u8,
    #[serde(default = "default_obd2_timeout_ms")]
    pub obd2_timeout_ms: u64,
}

const fn default_led_gpio() -> u8 {
    48
}

/// Maximum OBD2 timeout to avoid triggering watchdog in dongle task
pub const MAX_OBD2_TIMEOUT_MS: u64 = 4500;

const fn default_obd2_timeout_ms() -> u64 {
    MAX_OBD2_TIMEOUT_MS
}

impl Default for Config {
    fn default() -> Self {
        Self {
            wifi: WifiConfig::new_default(),
            ip: IpConfig::default(),
            obd2: Obd2Config::default(),
            ap_password: None,
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
            led_gpio: 48,
            obd2_timeout_ms: MAX_OBD2_TIMEOUT_MS,
        }
    }
}

impl Config {
    /// Clamp values to valid ranges (e.g., timeout limits)
    pub fn validate(&mut self) {
        if self.obd2_timeout_ms > MAX_OBD2_TIMEOUT_MS {
            warn!("Clamping obd2_timeout_ms from {} to {}", self.obd2_timeout_ms, MAX_OBD2_TIMEOUT_MS);
            self.obd2_timeout_ms = MAX_OBD2_TIMEOUT_MS;
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
            debug!("Config parsed: wifi.ssid={:?}, log_level={:?}", config.wifi.ssid, config.log_level);
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
