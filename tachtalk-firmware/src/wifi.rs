//! WiFi initialization and connection management

use atomic_enum::atomic_enum;
use esp_idf_hal::delay::FreeRtos;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::ipv4::{
    self, ClientConfiguration as IpClientConfiguration, ClientSettings as IpClientSettings,
    Configuration as IpConfiguration, Ipv4Addr, Mask, Subnet,
};
use esp_idf_svc::netif::{EspNetif, NetifConfiguration, NetifStack};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{
    AccessPointConfiguration, AuthMethod, ClientConfiguration, Configuration, EspWifi, WifiDriver,
};
use log::{debug, error, info, warn};
use std::sync::Arc;

use crate::config::Config;
use crate::error::Result;
use crate::status_leds::StatusLedMessage;
use crate::watchdog::WatchdogHandle;
use crate::State;

// ---------------------------------------------------------------------------
// WiFi STA state (used across tasks)
// ---------------------------------------------------------------------------

/// WiFi STA connection state, stored atomically on [`State`].
#[atomic_enum]
#[derive(Default, PartialEq, Eq)]
pub enum WifiStaState {
    /// Not associated at L2
    #[default]
    Disconnected = 0,
    /// Connecting (scanning, associating, or awaiting IP)
    Connecting = 1,
    /// Fully connected with a valid IP address
    Connected = 2,
}

impl Default for AtomicWifiStaState {
    fn default() -> Self {
        Self::new(WifiStaState::default())
    }
}

fn create_sta_netif(config: &Config) -> Result<EspNetif> {
    if config.ip.use_dhcp {
        info!("STA netif: DHCP mode");
        Ok(EspNetif::new(NetifStack::Sta)?)
    } else {
        // Parse static IP configuration
        let ip: Ipv4Addr = config
            .ip
            .ip
            .parse()
            .map_err(|_| crate::error::Error::InvalidStaticIp(config.ip.ip.clone()))?;
        let mask = config.ip.prefix_len;

        info!("STA netif: Static IP {ip}/{mask} (no gateway)");

        let mut sta_config = NetifConfiguration::wifi_default_client();
        sta_config.ip_configuration = Some(IpConfiguration::Client(IpClientConfiguration::Fixed(
            IpClientSettings {
                ip,
                subnet: Subnet {
                    gateway: Ipv4Addr::UNSPECIFIED,
                    mask: Mask(mask),
                },
                dns: None,
                secondary_dns: None,
            },
        )));
        Ok(EspNetif::new_with_conf(&sta_config)?)
    }
}

fn create_ap_netif(ap_ip: Ipv4Addr, ap_prefix_len: u8) -> Result<EspNetif> {
    // Custom router config that uses our IP as DNS
    // (default uses 8.8.8.8 which bypasses our captive portal DNS)
    let ap_router_config = ipv4::RouterConfiguration {
        subnet: ipv4::Subnet {
            gateway: ap_ip,
            mask: ipv4::Mask(ap_prefix_len),
        },
        dhcp_enabled: true,
        dns: Some(ap_ip),           // Point to our DNS server
        secondary_dns: Some(ap_ip), // Also use our DNS
    };

    let mut ap_netif_config = NetifConfiguration::wifi_default_router();
    ap_netif_config.ip_configuration = Some(ipv4::Configuration::Router(ap_router_config));
    Ok(EspNetif::new_with_conf(&ap_netif_config)?)
}

fn start_wifi(
    config: &Config,
    mut wifi: EspWifi<'static>,
    ap_ssid: &str,
    ap_password: Option<&str>,
    ap_auth_method: AuthMethod,
) -> Result<EspWifi<'static>> {
    // Get STA credentials from config
    let sta_ssid = config.wifi.ssid.clone();
    let sta_password = config.wifi.password.clone().unwrap_or_default();
    let sta_auth_method = if sta_password.is_empty() {
        AuthMethod::None
    } else {
        AuthMethod::WPA2Personal
    };

    // Determine AP password for config
    let ap_pw = ap_password.unwrap_or("");

    // Start WiFi in Mixed mode (AP + STA) so web UI is accessible while scanning
    info!("Starting WiFi in Mixed mode: AP '{ap_ssid}' + STA '{sta_ssid}'");
    wifi.set_configuration(&Configuration::Mixed(
        ClientConfiguration {
            ssid: sta_ssid.as_str().try_into().unwrap_or_default(),
            password: sta_password.as_str().try_into().unwrap_or_default(),
            auth_method: sta_auth_method,
            ..Default::default()
        },
        AccessPointConfiguration {
            ssid: ap_ssid.try_into().unwrap(),
            password: ap_pw.try_into().unwrap_or_default(),
            auth_method: ap_auth_method,
            channel: 0,
            ..Default::default()
        },
    ))?;
    wifi.start()?;

    Ok(wifi)
}

/// Initialize WiFi driver and network interfaces
pub fn init_wifi(
    config: &Config,
    modem: esp_idf_hal::modem::Modem,
    sys_loop: EspSystemEventLoop,
    nvs: EspDefaultNvsPartition,
) -> Result<(EspWifi<'static>, String)> {
    info!("Initializing WiFi...");

    let wifi_driver = WifiDriver::new(modem, sys_loop, Some(nvs))?;
    let sta_netif = create_sta_netif(config)?;
    let ap_netif = create_ap_netif(config.ap_ip, config.ap_prefix_len)?;
    let wifi = EspWifi::wrap_all(wifi_driver, sta_netif, ap_netif)?;

    // AP SSID from config (default is MAC-derived, computed in Config::default())
    let ap_ssid = config.ap_ssid.clone();

    // Get AP password from config
    let ap_password = config.ap_password.clone();
    let ap_auth_method = match &ap_password {
        Some(pw) if !pw.is_empty() => AuthMethod::WPA2Personal,
        _ => AuthMethod::None,
    };

    // Start WiFi in Mixed mode
    let wifi = start_wifi(
        config,
        wifi,
        &ap_ssid,
        ap_password.as_deref(),
        ap_auth_method,
    )?;

    let ap_ip_info = wifi.ap_netif().get_ip_info()?;
    info!(
        "AP started - connect to '{ap_ssid}' and navigate to http://{}",
        ap_ip_info.ip
    );

    Ok((wifi, ap_ssid))
}

enum StaConnectionState {
    /// Not connected at L2 (WiFi association)
    Disconnected,
    /// Connecting (L2 associated but waiting for IP)
    Connecting,
    /// Fully connected with a valid IP address
    Connected(Ipv4Addr),
}

impl From<&StaConnectionState> for WifiStaState {
    fn from(cs: &StaConnectionState) -> Self {
        match cs {
            StaConnectionState::Disconnected => Self::Disconnected,
            StaConnectionState::Connecting => Self::Connecting,
            StaConnectionState::Connected(_) => Self::Connected,
        }
    }
}

/// Background task to manage WiFi STA connection.
///
/// Always runs in Mixed mode (AP + STA) - AP is never disabled.
pub fn wifi_connection_manager(state: &Arc<State>) {
    /// Update the `wifi_sta_state` atomic and send a status LED message on change.
    fn update_wifi_state(state: &State, new: WifiStaState, prev: &mut WifiStaState) {
        if new != *prev {
            state
                .wifi_sta_state
                .store(new, std::sync::atomic::Ordering::Relaxed);
            let _ = state.status_led_tx.send(StatusLedMessage::WifiState(new));
            *prev = new;
        }
    }

    let watchdog = WatchdogHandle::register(c"wifi_manager");

    // Read STA SSID from config (cached at task start - changes require reboot)
    let sta_ssid = {
        let cfg_guard = state.config.lock().unwrap();
        cfg_guard.wifi.ssid.clone()
    };

    let mut prev_led_state = WifiStaState::Disconnected;
    let mut was_connected = false;

    // Send Connecting immediately so the LED starts blinking right away.
    // Without this, the LED would remain off until a state change occurs.
    if !sta_ssid.is_empty() {
        let _ = state
            .status_led_tx
            .send(StatusLedMessage::WifiState(WifiStaState::Connecting));
        prev_led_state = WifiStaState::Connecting;
    }

    loop {
        watchdog.feed();

        let connection_state = {
            let wifi_guard = state.wifi.lock().unwrap();
            let l2_connected = match wifi_guard.is_connected() {
                Ok(connected) => connected,
                Err(e) => {
                    error!("Failed to check WiFi connection status: {e}");
                    false
                }
            };
            if l2_connected {
                match wifi_guard.sta_netif().get_ip_info() {
                    Ok(info) if !info.ip.is_unspecified() => StaConnectionState::Connected(info.ip),
                    Ok(_) => StaConnectionState::Connecting,
                    Err(e) => {
                        error!("Failed to get STA IP info: {e}");
                        StaConnectionState::Connecting
                    }
                }
            } else {
                StaConnectionState::Disconnected
            }
        };

        let led_state = WifiStaState::from(&connection_state);
        update_wifi_state(state, led_state, &mut prev_led_state);

        match connection_state {
            StaConnectionState::Connected(ip) => {
                // Fully connected with IP - just monitor
                if !was_connected {
                    info!("WiFi STA connected to '{sta_ssid}' with IP: {ip}");
                    was_connected = true;
                }
                FreeRtos::delay_ms(1000);
            }
            StaConnectionState::Connecting => {
                // L2 connected but waiting for IP - don't call connect(), just wait
                FreeRtos::delay_ms(1000);
            }
            StaConnectionState::Disconnected => {
                // Not connected at L2 - try to connect
                if was_connected {
                    warn!("WiFi STA disconnected from '{sta_ssid}'");
                    was_connected = false;
                }

                debug!("Attempting to connect to '{sta_ssid}'...");

                // Initiate connection (non-blocking)
                {
                    let mut wifi_guard = state.wifi.lock().unwrap();
                    if let Err(e) = wifi_guard.connect() {
                        debug!("STA connection initiation failed: {e:?}");
                    }
                }

                // Wait for L2 connection or timeout (15s)
                for _ in 0..15 {
                    watchdog.feed();
                    FreeRtos::delay_ms(1000);

                    let wifi_guard = state.wifi.lock().unwrap();
                    match wifi_guard.is_connected() {
                        Ok(true) => {
                            // Re-evaluate state for the LED on L2 connect
                            let new_state = match wifi_guard.sta_netif().get_ip_info() {
                                Ok(info) if !info.ip.is_unspecified() => WifiStaState::Connected,
                                _ => WifiStaState::Connecting,
                            };
                            update_wifi_state(state, new_state, &mut prev_led_state);
                            break;
                        }
                        Ok(false) => {}
                        Err(e) => {
                            error!("Failed to check WiFi connection status: {e}");
                        }
                    }
                }
            }
        }
    }
}
