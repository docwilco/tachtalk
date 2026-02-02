use anyhow::{anyhow, Result};
use esp_idf_svc::nvs::{EspNvs, EspNvsPartition, NvsDefault};
use serde::{Deserialize, Serialize};
use smart_leds::RGB8;
use std::sync::Mutex;

const NVS_NAMESPACE: &str = "tachtalk";
const NVS_CONFIG_KEY: &str = "config";

// Global NVS handle - initialized once in main
static NVS: Mutex<Option<EspNvs<NvsDefault>>> = Mutex::new(None);

pub fn init_nvs(nvs_partition: EspNvsPartition<NvsDefault>) -> Result<()> {
    let nvs = EspNvs::new(nvs_partition, NVS_NAMESPACE, true)?;
    *NVS.lock().unwrap() = Some(nvs);
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThresholdConfig {
    pub rpm: u32,
    pub color: RGB8Color,
    pub num_leds: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RGB8Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl From<RGB8Color> for RGB8 {
    fn from(c: RGB8Color) -> Self {
        RGB8 { r: c.r, g: c.g, b: c.b }
    }
}

impl From<RGB8> for RGB8Color {
    fn from(c: RGB8) -> Self {
        RGB8Color { r: c.r, g: c.g, b: c.b }
    }
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub wifi: WifiConfig,
    #[serde(default)]
    pub ip_config: IpConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ap_password: Option<String>,
    pub thresholds: Vec<ThresholdConfig>,
    pub blink_rpm: u32,
    pub total_leds: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            wifi: WifiConfig::new_default(),
            ip_config: IpConfig::default(),
            ap_password: None,
            thresholds: vec![
                ThresholdConfig {
                    rpm: 3000,
                    color: RGB8Color { r: 0, g: 255, b: 0 },
                    num_leds: 2,
                },
                ThresholdConfig {
                    rpm: 4000,
                    color: RGB8Color { r: 255, g: 255, b: 0 },
                    num_leds: 4,
                },
                ThresholdConfig {
                    rpm: 5000,
                    color: RGB8Color { r: 255, g: 0, b: 0 },
                    num_leds: 6,
                },
            ],
            blink_rpm: 6000,
            total_leds: 8,
        }
    }
}

impl Config {
    pub fn load_or_default() -> Self {
        Self::load().unwrap_or_default()
    }

    pub fn load() -> Result<Self> {
        let nvs_guard = NVS.lock().unwrap();
        let nvs = nvs_guard.as_ref().ok_or_else(|| anyhow!("NVS not initialized"))?;
        
        // Get the blob length first
        let len = nvs.blob_len(NVS_CONFIG_KEY)?;
        if let Some(len) = len {
            let mut buf = vec![0u8; len];
            nvs.get_blob(NVS_CONFIG_KEY, &mut buf)?;
            let config: Config = serde_json::from_slice(&buf)?;
            Ok(config)
        } else {
            Err(anyhow!("No config found in NVS"))
        }
    }

    pub fn save(&self) -> Result<()> {
        let mut nvs_guard = NVS.lock().unwrap();
        let nvs = nvs_guard.as_mut().ok_or_else(|| anyhow!("NVS not initialized"))?;
        
        let json = serde_json::to_vec(self)?;
        nvs.set_blob(NVS_CONFIG_KEY, &json)?;
        Ok(())
    }
}
