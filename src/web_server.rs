use anyhow::Result;
use embedded_svc::http::Method;
use embedded_svc::io::Write;
use esp_idf_svc::http::server::{Configuration, EspHttpServer};
use esp_idf_svc::wifi::{BlockingWifi, EspWifi};
use log::*;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::os::unix::io::RawFd;

use crate::config::Config;
use crate::leds::LedController;
use crate::WifiMode;

/// Message types for SSE manager
pub enum SseMessage {
    /// New SSE client connected (socket fd, done signal sender)
    NewConnection(RawFd, Sender<()>),
    /// RPM update to broadcast
    RpmUpdate(u32),
}

/// Sender for SSE messages (new connections and RPM updates)
pub type SseSender = Sender<SseMessage>;

/// Create SSE channel and start the manager task
pub fn start_sse_manager() -> SseSender {
    let (tx, rx) = mpsc::channel::<SseMessage>();
    
    std::thread::spawn(move || {
        sse_manager_task(rx);
    });
    
    tx
}

/// SSE manager task - handles all SSE connections in a single thread
fn sse_manager_task(rx: Receiver<SseMessage>) {
    // Active connections: (socket fd, done signal sender)
    let mut connections: Vec<(RawFd, Sender<()>)> = Vec::new();
    let mut current_rpm: Option<u32> = None;
    
    loop {
        // Non-blocking receive to handle multiple message types
        match rx.try_recv() {
            Ok(SseMessage::NewConnection(fd, done_tx)) => {
                info!("SSE: New connection registered (fd={})", fd);
                // Send current RPM to new client immediately
                if let Some(rpm) = current_rpm {
                    let msg = format!("data: {{\"rpm\":{}}}\n\n", rpm);
                    if write_to_fd(fd, msg.as_bytes()).is_err() {
                        // Connection already dead, signal done
                        let _ = done_tx.send(());
                        continue;
                    }
                }
                connections.push((fd, done_tx));
            }
            Ok(SseMessage::RpmUpdate(rpm)) => {
                current_rpm = Some(rpm);
                let msg = format!("data: {{\"rpm\":{}}}\n\n", rpm);
                
                // Broadcast to all connections, remove dead ones
                connections.retain(|(fd, done_tx)| {
                    if write_to_fd(*fd, msg.as_bytes()).is_ok() {
                        true
                    } else {
                        info!("SSE: Connection closed (fd={})", fd);
                        let _ = done_tx.send(());
                        false
                    }
                });
            }
            Err(TryRecvError::Empty) => {
                // No messages, sleep briefly
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(TryRecvError::Disconnected) => {
                info!("SSE: Manager channel disconnected, exiting");
                break;
            }
        }
    }
}

/// Write data to a raw file descriptor
fn write_to_fd(fd: RawFd, data: &[u8]) -> std::io::Result<()> {
    let written = unsafe {
        esp_idf_svc::sys::write(fd, data.as_ptr() as *const _, data.len())
    };
    if written < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

const HTML_INDEX: &str = r#"
<!DOCTYPE html>
<html>
<head>
    <title>TachTalk Configuration</title>
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <style>
        body {
            font-family: Arial, sans-serif;
            margin: 20px;
            background-color: #1a1a1a;
            color: #ffffff;
        }
        .container {
            max-width: 800px;
            margin: 0 auto;
        }
        h1 {
            color: #00ff00;
        }
        .threshold {
            background-color: #2a2a2a;
            padding: 15px;
            margin: 10px 0;
            border-radius: 5px;
            border-left: 4px solid #00ff00;
        }
        .form-group {
            margin: 10px 0;
        }
        label {
            display: inline-block;
            width: 120px;
            font-weight: bold;
        }
        input[type="number"], input[type="color"], input[type="text"], input[type="password"] {
            padding: 5px;
            border: 1px solid #444;
            background-color: #333;
            color: #fff;
            border-radius: 3px;
        }
        input[type="text"], input[type="password"] {
            width: 200px;
        }
        button {
            background-color: #00ff00;
            color: #000;
            padding: 10px 20px;
            border: none;
            border-radius: 5px;
            cursor: pointer;
            font-weight: bold;
            margin: 10px 5px;
        }
        button:hover {
            background-color: #00dd00;
        }
        .blink-config {
            background-color: #3a2a2a;
            padding: 15px;
            margin: 20px 0;
            border-radius: 5px;
            border-left: 4px solid #ff0000;
        }
        .wifi-config {
            background-color: #2a2a3a;
            padding: 15px;
            margin: 20px 0;
            border-radius: 5px;
            border-left: 4px solid #0088ff;
        }
        .ap-mode-banner {
            background-color: #ff8800;
            color: #000;
            padding: 15px;
            margin: 20px 0;
            border-radius: 5px;
            text-align: center;
            font-weight: bold;
        }
        #status {
            padding: 10px;
            margin: 10px 0;
            border-radius: 5px;
            display: none;
        }
        .success {
            background-color: #2d5016;
            border: 1px solid #00ff00;
        }
        .error {
            background-color: #5a1a1a;
            border: 1px solid #ff0000;
        }
        .mode-indicator {
            padding: 5px 10px;
            border-radius: 3px;
            font-size: 0.9em;
            margin-left: 10px;
        }
        .mode-ap {
            background-color: #ff8800;
            color: #000;
        }
        .mode-client {
            background-color: #00ff00;
            color: #000;
        }
        .network-info {
            background-color: #2a3a2a;
            padding: 15px;
            margin: 20px 0;
            border-radius: 5px;
            border-left: 4px solid #00ff00;
        }
        .network-info .info-row {
            display: flex;
            justify-content: space-between;
            padding: 5px 0;
            border-bottom: 1px solid #444;
        }
        .network-info .info-row:last-child {
            border-bottom: none;
        }
        .rpm-display {
            font-size: 3em;
            text-align: center;
            color: #00ff00;
            padding: 20px;
            background-color: #2a2a2a;
            border-radius: 10px;
            margin: 20px 0;
        }
        select {
            padding: 5px;
            border: 1px solid #444;
            background-color: #333;
            color: #fff;
            border-radius: 3px;
        }
        .hidden {
            display: none;
        }
    </style>
</head>
<body>
    <div class="container">
        <h1>🏁 TachTalk Configuration <span id="modeIndicator" class="mode-indicator"></span></h1>
        
        <div id="apBanner" class="ap-mode-banner" style="display: none;">
            ⚠️ Running in Setup Mode - Configure WiFi below to connect to your network
        </div>
        
        <div id="status"></div>
        
        <div class="rpm-display">
            <div>RPM: <span id="currentRpm">---</span></div>
        </div>
        
        <div class="network-info">
            <h2>📊 Network Status</h2>
            <div class="info-row"><span>IP Address:</span><span id="netIp">---</span></div>
            <div class="info-row"><span>Gateway:</span><span id="netGateway">---</span></div>
            <div class="info-row"><span>Subnet Mask:</span><span id="netSubnet">---</span></div>
            <div class="info-row"><span>DNS:</span><span id="netDns">---</span></div>
            <div class="info-row"><span>MAC Address:</span><span id="netMac">---</span></div>
            <div class="info-row"><span>RSSI:</span><span id="netRssi">---</span></div>
        </div>
        
        <div class="wifi-config">
            <h2>📶 WiFi Configuration</h2>
            <div class="form-group">
                <label>SSID:</label>
                <input type="text" id="wifiSsid" placeholder="Your WiFi network name">
            </div>
            <div class="form-group">
                <label>Password:</label>
                <input type="password" id="wifiPassword" placeholder="WiFi password">
            </div>
            <div class="form-group">
                <label>IP Mode:</label>
                <select id="ipMode" onchange="toggleStaticIp()">
                    <option value="dhcp">DHCP (Automatic)</option>
                    <option value="static">Static IP</option>
                </select>
            </div>
            <div id="staticIpFields" class="hidden">
                <div class="form-group">
                    <label>IP Address:</label>
                    <input type="text" id="staticIp" placeholder="192.168.1.100">
                </div>
                <div class="form-group">
                    <label>Gateway:</label>
                    <input type="text" id="staticGateway" placeholder="192.168.1.1">
                </div>
                <div class="form-group">
                    <label>Subnet Mask:</label>
                    <input type="text" id="staticSubnet" placeholder="255.255.255.0">
                </div>
                <div class="form-group">
                    <label>DNS:</label>
                    <input type="text" id="staticDns" placeholder="8.8.8.8">
                </div>
            </div>
            <button onclick="saveWifi()">Save & Connect</button>
            <button onclick="scanWifi()">Scan Networks</button>
            <div id="wifiNetworks" style="margin-top: 10px;"></div>
        </div>
        
        <h2>RPM Thresholds</h2>
        <div id="thresholds"></div>
        
        <button onclick="addThreshold()">Add Threshold</button>
        
        <div class="blink-config">
            <h2>Blink Configuration</h2>
            <div class="form-group">
                <label>Blink RPM:</label>
                <input type="number" id="blinkRpm" value="6000">
            </div>
        </div>
        
        <div class="form-group">
            <label>Total LEDs:</label>
            <input type="number" id="totalLeds" value="8">
        </div>
        
        <button onclick="saveConfig()">Save Configuration</button>
        <button onclick="loadConfig()">Reload</button>
    </div>

    <script>
        let config = {
            wifi: { ssid: '', password: '' },
            ip_config: { use_dhcp: true, ip: null, gateway: null, subnet: null, dns: null },
            thresholds: [
                { rpm: 3000, color: { r: 0, g: 255, b: 0 }, num_leds: 2 },
                { rpm: 4000, color: { r: 255, g: 255, b: 0 }, num_leds: 4 },
                { rpm: 5000, color: { r: 255, g: 0, b: 0 }, num_leds: 6 }
            ],
            blink_rpm: 6000,
            total_leds: 8
        };

        function toggleStaticIp() {
            const mode = document.getElementById('ipMode').value;
            const fields = document.getElementById('staticIpFields');
            fields.className = mode === 'static' ? '' : 'hidden';
        }

        function rgbToHex(color) {
            return '#' + [color.r, color.g, color.b].map(x => {
                const hex = x.toString(16);
                return hex.length === 1 ? '0' + hex : hex;
            }).join('');
        }

        function hexToRgb(hex) {
            const result = /^#?([a-f\d]{2})([a-f\d]{2})([a-f\d]{2})$/i.exec(hex);
            return result ? {
                r: parseInt(result[1], 16),
                g: parseInt(result[2], 16),
                b: parseInt(result[3], 16)
            } : { r: 0, g: 0, b: 0 };
        }

        function renderThresholds() {
            const container = document.getElementById('thresholds');
            container.innerHTML = '';
            
            config.thresholds.forEach((threshold, index) => {
                const div = document.createElement('div');
                div.className = 'threshold';
                div.innerHTML = '<h3>Threshold ' + (index + 1) + '</h3>' +
                    '<div class="form-group">' +
                        '<label>RPM:</label>' +
                        '<input type="number" value="' + threshold.rpm + '" onchange="updateThreshold(' + index + ', \'rpm\', parseInt(this.value))">' +
                    '</div>' +
                    '<div class="form-group">' +
                        '<label>Color:</label>' +
                        '<input type="color" value="' + rgbToHex(threshold.color) + '" onchange="updateThreshold(' + index + ', \'color\', hexToRgb(this.value))">' +
                    '</div>' +
                    '<div class="form-group">' +
                        '<label>Number of LEDs:</label>' +
                        '<input type="number" value="' + threshold.num_leds + '" onchange="updateThreshold(' + index + ', \'num_leds\', parseInt(this.value))">' +
                    '</div>' +
                    '<button onclick="removeThreshold(' + index + ')">Remove</button>';
                container.appendChild(div);
            });
        }

        function updateThreshold(index, field, value) {
            config.thresholds[index][field] = value;
        }

        function addThreshold() {
            config.thresholds.push({
                rpm: 5000,
                color: { r: 255, g: 0, b: 0 },
                num_leds: 2
            });
            renderThresholds();
        }

        function removeThreshold(index) {
            config.thresholds.splice(index, 1);
            renderThresholds();
        }

        function showStatus(message, isError) {
            const status = document.getElementById('status');
            status.textContent = message;
            status.className = isError ? 'error' : 'success';
            status.style.display = 'block';
            setTimeout(() => {
                status.style.display = 'none';
            }, 3000);
        }

        async function saveConfig() {
            config.blink_rpm = parseInt(document.getElementById('blinkRpm').value);
            config.total_leds = parseInt(document.getElementById('totalLeds').value);
            
            try {
                const response = await fetch('/api/config', {
                    method: 'POST',
                    headers: {
                        'Content-Type': 'application/json',
                    },
                    body: JSON.stringify(config)
                });
                
                if (response.ok) {
                    showStatus('Configuration saved successfully!', false);
                } else {
                    showStatus('Failed to save configuration', true);
                }
            } catch (error) {
                showStatus('Error: ' + error.message, true);
            }
        }

        async function saveWifi() {
            const ssid = document.getElementById('wifiSsid').value;
            const password = document.getElementById('wifiPassword').value;
            const useDhcp = document.getElementById('ipMode').value === 'dhcp';
            
            if (!ssid) {
                showStatus('Please enter a WiFi SSID', true);
                return;
            }
            
            const ipConfig = {
                use_dhcp: useDhcp,
                ip: useDhcp ? null : document.getElementById('staticIp').value || null,
                gateway: useDhcp ? null : document.getElementById('staticGateway').value || null,
                subnet: useDhcp ? null : document.getElementById('staticSubnet').value || null,
                dns: useDhcp ? null : document.getElementById('staticDns').value || null
            };
            
            config.wifi = { ssid, password: password || null };
            config.ip_config = ipConfig;
            
            try {
                const response = await fetch('/api/wifi', {
                    method: 'POST',
                    headers: {
                        'Content-Type': 'application/json',
                    },
                    body: JSON.stringify({ ssid, password: password || null, ip_config: ipConfig })
                });
                
                if (response.ok) {
                    showStatus('WiFi saved! Device will restart and connect to ' + ssid, false);
                    setTimeout(() => {
                        showStatus('Restarting device...', false);
                    }, 2000);
                } else {
                    showStatus('Failed to save WiFi configuration', true);
                }
            } catch (error) {
                showStatus('Error: ' + error.message, true);
            }
        }

        async function scanWifi() {
            showStatus('Scanning for networks...', false);
            try {
                const response = await fetch('/api/wifi/scan');
                if (response.ok) {
                    const networks = await response.json();
                    const container = document.getElementById('wifiNetworks');
                    if (networks.length === 0) {
                        container.innerHTML = '<p>No networks found</p>';
                    } else {
                        container.innerHTML = '<p>Available networks (click to select):</p>' +
                            networks.map(n => 
                                '<button onclick="document.getElementById(\'wifiSsid\').value=\'' + n.ssid + '\'" style="margin: 2px; padding: 5px 10px;">' +
                                    n.ssid + ' (' + n.rssi + ' dBm)' +
                                '</button>'
                            ).join('');
                    }
                } else {
                    showStatus('Failed to scan networks', true);
                }
            } catch (error) {
                showStatus('Error: ' + error.message, true);
            }
        }

        async function loadConfig() {
            try {
                const response = await fetch('/api/config');
                if (response.ok) {
                    config = await response.json();
                    document.getElementById('blinkRpm').value = config.blink_rpm;
                    document.getElementById('totalLeds').value = config.total_leds;
                    document.getElementById('wifiSsid').value = config.wifi?.ssid || '';
                    document.getElementById('wifiPassword').value = config.wifi?.password || '';
                    
                    // IP config
                    const ipConfig = config.ip_config || { use_dhcp: true };
                    document.getElementById('ipMode').value = ipConfig.use_dhcp ? 'dhcp' : 'static';
                    document.getElementById('staticIp').value = ipConfig.ip || '';
                    document.getElementById('staticGateway').value = ipConfig.gateway || '';
                    document.getElementById('staticSubnet').value = ipConfig.subnet || '';
                    document.getElementById('staticDns').value = ipConfig.dns || '';
                    toggleStaticIp();
                    
                    renderThresholds();
                    showStatus('Configuration loaded!', false);
                } else {
                    showStatus('Failed to load configuration', true);
                }
            } catch (error) {
                showStatus('Error: ' + error.message, true);
            }
        }

        async function loadMode() {
            try {
                const response = await fetch('/api/mode');
                if (response.ok) {
                    const data = await response.json();
                    const indicator = document.getElementById('modeIndicator');
                    const banner = document.getElementById('apBanner');
                    if (data.mode === 'ap') {
                        indicator.textContent = 'Setup Mode';
                        indicator.className = 'mode-indicator mode-ap';
                        banner.style.display = 'block';
                    } else {
                        indicator.textContent = 'Connected';
                        indicator.className = 'mode-indicator mode-client';
                        banner.style.display = 'none';
                    }
                }
            } catch (error) {
                console.error('Failed to load mode:', error);
            }
        }

        async function loadNetworkStatus() {
            try {
                const response = await fetch('/api/network');
                if (response.ok) {
                    const data = await response.json();
                    document.getElementById('netIp').textContent = data.ip || '---';
                    document.getElementById('netGateway').textContent = data.gateway || '---';
                    document.getElementById('netSubnet').textContent = data.subnet || '---';
                    document.getElementById('netDns').textContent = data.dns || '---';
                    document.getElementById('netMac').textContent = data.mac || '---';
                    document.getElementById('netRssi').textContent = data.rssi ? data.rssi + ' dBm' : '---';
                }
            } catch (error) {
                console.error('Failed to load network status:', error);
            }
        }

        async function loadRpm() {
            // RPM is now handled via SSE, this is just a fallback
        }

        function setupRpmEventSource() {
            const evtSource = new EventSource('/api/rpm/stream');
            evtSource.onmessage = function(event) {
                const data = JSON.parse(event.data);
                document.getElementById('currentRpm').textContent = data.rpm !== null ? data.rpm : '---';
            };
            evtSource.onerror = function() {
                document.getElementById('currentRpm').textContent = '---';
                // Reconnect after 2 seconds
                setTimeout(setupRpmEventSource, 2000);
            };
        }

        // Initialize
        renderThresholds();
        loadConfig();
        loadMode();
        loadNetworkStatus();
        setupRpmEventSource();
        
        // Poll network status (RPM uses SSE now)
        setInterval(loadNetworkStatus, 5000);
    </script>
</body>
</html>
"#;

// Captive portal redirect page
const HTML_CAPTIVE_PORTAL: &str = r#"
<!DOCTYPE html>
<html>
<head>
    <title>TachTalk Setup</title>
    <meta http-equiv="refresh" content="0;url=http://192.168.4.1/">
</head>
<body>
    <p>Redirecting to <a href="http://192.168.4.1/">TachTalk Setup</a>...</p>
</body>
</html>
"#;

pub fn start_server(
    config: Arc<Mutex<Config>>,
    _led_controller: Arc<Mutex<LedController>>,
    wifi_mode: Arc<Mutex<WifiMode>>,
    wifi: Arc<Mutex<BlockingWifi<EspWifi<'static>>>>,
    sse_tx: SseSender,
) -> Result<()> {
    let server_config = Configuration::default();
    let mut server = EspHttpServer::new(&server_config)?;

    // Serve the main HTML page
    server.fn_handler("/", Method::Get, |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        let mut response = req.into_ok_response()?;
        response.write_all(HTML_INDEX.as_bytes())?;
        Ok(())
    })?;

    // Captive portal handlers - redirect common captive portal detection URLs
    let captive_urls = [
        "/generate_204",           // Android
        "/gen_204",                // Android
        "/hotspot-detect.html",    // Apple
        "/library/test/success.html", // Apple
        "/ncsi.txt",               // Windows
        "/connecttest.txt",        // Windows
        "/redirect",               // Various
        "/canonical.html",         // Firefox
        "/success.txt",            // Various
    ];

    for url in captive_urls {
        server.fn_handler(url, Method::Get, |req| -> Result<(), esp_idf_svc::io::EspIOError> {
            let mut response = req.into_response(302, Some("Found"), &[
                ("Location", "http://192.168.4.1/"),
                ("Cache-Control", "no-cache"),
            ])?;
            response.write_all(HTML_CAPTIVE_PORTAL.as_bytes())?;
            Ok(())
        })?;
    }

    // GET mode endpoint
    let mode_clone = wifi_mode.clone();
    server.fn_handler("/api/mode", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        let mode = mode_clone.lock().unwrap();
        let mode_str = match *mode {
            WifiMode::AccessPoint => "ap",
            WifiMode::Client => "client",
        };
        let json = format!(r#"{{"mode":"{}"}}"#, mode_str);
        
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // GET config endpoint
    let config_clone = config.clone();
    server.fn_handler("/api/config", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        let cfg = config_clone.lock().unwrap();
        let json = serde_json::to_string(&*cfg).unwrap();
        
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // POST config endpoint
    let config_clone = config.clone();
    server.fn_handler("/api/config", Method::Post, move |mut req| -> Result<(), esp_idf_svc::io::EspIOError> {
        let mut buf = vec![0u8; 2048];
        let bytes_read = req.read(&mut buf)?;
        
        if let Ok(new_config) = serde_json::from_slice::<Config>(&buf[..bytes_read]) {
            let mut cfg = config_clone.lock().unwrap();
            *cfg = new_config;
            let _ = cfg.save();
            
            req.into_ok_response()?;
        } else {
            req.into_status_response(400)?;
        }
        
        Ok(())
    })?;

    // POST wifi endpoint - save wifi and restart
    let config_clone = config.clone();
    server.fn_handler("/api/wifi", Method::Post, move |mut req| -> Result<(), esp_idf_svc::io::EspIOError> {
        let mut buf = vec![0u8; 1024];
        let bytes_read = req.read(&mut buf)?;
        
        #[derive(serde::Deserialize)]
        struct IpConfigRequest {
            use_dhcp: bool,
            ip: Option<String>,
            gateway: Option<String>,
            subnet: Option<String>,
            dns: Option<String>,
        }
        
        #[derive(serde::Deserialize)]
        struct WifiRequest {
            ssid: String,
            password: Option<String>,
            ip_config: Option<IpConfigRequest>,
        }
        
        if let Ok(wifi_req) = serde_json::from_slice::<WifiRequest>(&buf[..bytes_read]) {
            let mut cfg = config_clone.lock().unwrap();
            cfg.wifi.ssid = wifi_req.ssid;
            cfg.wifi.password = wifi_req.password.filter(|p| !p.is_empty());
            
            // Update IP config if provided
            if let Some(ip_cfg) = wifi_req.ip_config {
                cfg.ip_config.use_dhcp = ip_cfg.use_dhcp;
                cfg.ip_config.ip = ip_cfg.ip.filter(|s| !s.is_empty());
                cfg.ip_config.gateway = ip_cfg.gateway.filter(|s| !s.is_empty());
                cfg.ip_config.subnet = ip_cfg.subnet.filter(|s| !s.is_empty());
                cfg.ip_config.dns = ip_cfg.dns.filter(|s| !s.is_empty());
            }
            
            if let Err(e) = cfg.save() {
                error!("Failed to save config: {:?}", e);
                req.into_status_response(500)?;
                return Ok(());
            }
            
            req.into_ok_response()?;
            
            // Schedule restart after response is sent
            info!("WiFi configured, restarting in 2 seconds...");
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_secs(2));
                unsafe {
                    esp_idf_svc::sys::esp_restart();
                }
            });
        } else {
            req.into_status_response(400)?;
        }
        
        Ok(())
    })?;

    // GET wifi scan endpoint
    let wifi_clone = wifi.clone();
    server.fn_handler("/api/wifi/scan", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        let mut wifi = wifi_clone.lock().unwrap();
        
        #[derive(serde::Serialize)]
        struct Network {
            ssid: String,
            rssi: i8,
        }
        
        let networks: Vec<Network> = match wifi.scan() {
            Ok(aps) => aps
                .into_iter()
                .map(|ap| Network {
                    ssid: ap.ssid.to_string(),
                    rssi: ap.signal_strength,
                })
                .collect(),
            Err(e) => {
                error!("WiFi scan failed: {:?}", e);
                Vec::new()
            }
        };
        
        let json = serde_json::to_string(&networks).unwrap_or_else(|_| "[]".to_string());
        
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // GET network status endpoint
    let wifi_clone = wifi.clone();
    server.fn_handler("/api/network", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        let wifi = wifi_clone.lock().unwrap();
        
        #[derive(serde::Serialize)]
        struct NetworkStatus {
            ip: Option<String>,
            gateway: Option<String>,
            subnet: Option<String>,
            dns: Option<String>,
            mac: String,
            rssi: Option<i8>,
        }
        
        let sta_netif = wifi.wifi().sta_netif();
        let ip_info = sta_netif.get_ip_info().ok();
        
        let mac_bytes = wifi.wifi().get_mac(esp_idf_svc::wifi::WifiDeviceId::Sta).unwrap_or([0u8; 6]);
        let mac = format!("{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            mac_bytes[0], mac_bytes[1], mac_bytes[2],
            mac_bytes[3], mac_bytes[4], mac_bytes[5]);
        
        let status = NetworkStatus {
            ip: ip_info.as_ref().map(|i| format!("{}", i.ip)),
            gateway: ip_info.as_ref().map(|i| format!("{}", i.subnet.gateway)),
            subnet: ip_info.as_ref().map(|i| format!("{}", i.subnet.mask)),
            dns: ip_info.as_ref().and_then(|i| i.dns.map(|d| format!("{}", d))),
            mac,
            rssi: None, // TODO: Get RSSI from wifi driver
        };
        
        let json = serde_json::to_string(&status).unwrap_or_else(|_| "{}".to_string());
        
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // GET RPM endpoint (fallback for non-SSE clients)
    server.fn_handler("/api/rpm", Method::Get, |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        // This is just a fallback, real updates come via SSE
        let json = r#"{"rpm":null}"#;
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // SSE endpoint for RPM streaming
    let sse_tx_clone = sse_tx.clone();
    server.fn_handler("/api/rpm/stream", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        // Send SSE headers
        let mut response = req.into_response(200, Some("OK"), &[
            ("Content-Type", "text/event-stream"),
            ("Cache-Control", "no-cache"),
            ("Connection", "keep-alive"),
            ("Access-Control-Allow-Origin", "*"),
        ])?;
        
        // Get the raw connection and its file descriptor
        let conn = response.connection().raw_connection()?;
        
        // Send initial message
        conn.write_all(b"data: {\"rpm\":null}\n\n")?;
        
        // Get the socket fd from the raw connection using RawHandle trait
        use esp_idf_svc::handle::RawHandle;
        let fd = unsafe { esp_idf_svc::sys::httpd_req_to_sockfd(conn.handle()) };
        
        // Create channel to be notified when connection is closed
        let (done_tx, done_rx) = mpsc::channel::<()>();
        
        // Register this connection with the SSE manager
        if sse_tx_clone.send(SseMessage::NewConnection(fd, done_tx)).is_err() {
            return Ok(());
        }
        
        // Block until the SSE manager signals that the connection is closed
        // This keeps the HTTP handler alive so the connection stays open
        let _ = done_rx.recv();
        
        Ok(())
    })?;

    info!("Web server started on http://0.0.0.0:80");
    
    // Keep server alive
    std::mem::forget(server);
    
    Ok(())
}
