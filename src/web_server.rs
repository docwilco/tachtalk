use anyhow::Result;
use embedded_svc::http::Method;
use embedded_svc::io::Write;
use esp_idf_svc::http::server::{Configuration, EspHttpServer};
use esp_idf_svc::wifi::{BlockingWifi, EspWifi};
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use esp_idf_svc::sys::{esp_get_free_heap_size, esp_get_minimum_free_heap_size};

use crate::config::Config;
use crate::obd2::{AtCommandLog, PidLog};
use crate::sse_server::SSE_PORT;
use crate::WifiMode;

/// IP configuration for WiFi request
#[derive(serde::Deserialize)]
struct IpConfigRequest {
    use_dhcp: bool,
    ip: Option<String>,
    gateway: Option<String>,
    subnet: Option<String>,
    dns: Option<String>,
}

/// WiFi configuration request from web UI
#[derive(serde::Deserialize)]
struct WifiRequest {
    ssid: String,
    password: Option<String>,
    ip: Option<IpConfigRequest>,
}

/// WiFi network scan result
#[derive(serde::Serialize)]
struct Network {
    ssid: String,
    rssi: i8,
}

/// Network status response
#[derive(serde::Serialize)]
struct NetworkStatus {
    ip: Option<String>,
    gateway: Option<String>,
    subnet: Option<String>,
    dns: Option<String>,
    mac: String,
    rssi: Option<i8>,
}

/// Debug info response
#[derive(serde::Serialize)]
struct DebugInfo {
    at_commands: Vec<String>,
    pids: Vec<String>,
    free_heap: u32,
    min_free_heap: u32,
}

// HTML split into two parts to inject SSE_PORT without runtime allocation
const HTML_INDEX_START: &str = r#"
<!DOCTYPE html>
<html>
<head>
    <meta charset="UTF-8">
    <title>TachTalk Configuration</title>
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <script>const SSE_PORT = "#;

const HTML_INDEX_END: &str = r#";</script>
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
        .debug-section {
            background-color: #2a2a3a;
            padding: 10px 15px;
            margin: 20px 0;
            border-radius: 5px;
            border-left: 4px solid #888;
        }
        .debug-section summary {
            cursor: pointer;
            font-weight: bold;
            color: #888;
        }
        .debug-section[open] summary {
            margin-bottom: 10px;
        }
        .debug-content {
            padding: 10px 0;
        }
        .debug-content h3 {
            margin: 10px 0 10px 0;
            font-size: 1em;
            color: #aaa;
        }
        .debug-row {
            display: flex;
            justify-content: space-between;
            padding: 3px 0;
            font-size: 0.9em;
        }
        .at-commands {
            font-family: monospace;
            font-size: 0.85em;
            color: #8f8;
            word-break: break-all;
        }
        .password-wrapper {
            display: inline-flex;
            align-items: center;
            gap: 5px;
        }
        .toggle-password {
            background-color: #444;
            color: #fff;
            padding: 5px 10px;
            border: 1px solid #444;
            border-radius: 3px;
            cursor: pointer;
            font-size: 0.9em;
        }
        .toggle-password:hover {
            background-color: #555;
        }
        .spinner {
            display: inline-block;
            width: 14px;
            height: 14px;
            border: 2px solid rgba(0, 0, 0, 0.3);
            border-top-color: #000;
            border-radius: 50%;
            animation: spin 0.8s linear infinite;
            margin-right: 6px;
            vertical-align: middle;
        }
        @keyframes spin {
            to { transform: rotate(360deg); }
        }
        button:disabled {
            opacity: 0.7;
            cursor: not-allowed;
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
        
        <details class="debug-section">
            <summary>🔧 Debug Info</summary>
            <div class="debug-content">
                <div class="debug-row"><span>Free Heap:</span><span id="freeHeap">---</span></div>
                <div class="debug-row"><span>Min Free Heap:</span><span id="minFreeHeap">---</span></div>
                <h3>AT Commands Received</h3>
                <div id="atCommands" class="at-commands">Loading...</div>
                <h3>OBD2 PIDs Requested</h3>
                <div id="pids" class="pids">Loading...</div>
            </div>
        </details>
        
        <div class="wifi-config">
            <h2>📶 WiFi Configuration</h2>
            <div class="form-group">
                <label>SSID:</label>
                <input type="text" id="wifiSsid" placeholder="Your WiFi network name">
            </div>
            <div class="form-group">
                <label>Password:</label>
                <span class="password-wrapper">
                    <input type="password" id="wifiPassword" placeholder="WiFi password">
                    <button type="button" class="toggle-password" onclick="togglePasswordVisibility()">Show</button>
                </span>
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
            <button id="btnSaveWifi" onclick="saveWifi()">Save & Connect</button>
            <button id="btnScanWifi" onclick="scanWifi()">Scan Networks</button>
            <div id="wifiNetworks" style="margin-top: 10px;"></div>
        </div>
        
        <div class="wifi-config">
            <h2>🔌 OBD2 Configuration</h2>
            <div class="form-group">
                <label>Dongle IP:</label>
                <input type="text" id="obd2DongleIp" placeholder="192.168.0.10">
            </div>
            <div class="form-group">
                <label>Dongle Port:</label>
                <input type="number" id="obd2DonglePort" placeholder="35000" min="1" max="65535">
            </div>
            <div class="form-group">
                <label>Proxy Listen Port:</label>
                <input type="number" id="obd2ListenPort" placeholder="35000" min="1" max="65535">
            </div>
        </div>
        
        <div class="wifi-config">
            <h2>⚙️ System Settings</h2>
            <div class="form-group">
                <label>Log Level:</label>
                <select id="logLevel">
                    <option value="off">Off</option>
                    <option value="error">Error</option>
                    <option value="warn">Warn</option>
                    <option value="info">Info</option>
                    <option value="debug">Debug</option>
                </select>
            </div>
            <div class="form-group">
                <button id="btnReboot" onclick="rebootDevice()" style="background-color: #ff5500;">🔄 Reboot Device</button>
            </div>
        </div>
        
        <h2>RPM Thresholds</h2>
        <div id="thresholds"></div>
        
        <button onclick="addThreshold()">Add Threshold</button>
        
        <div class="form-group">
            <label>Total LEDs:</label>
            <input type="number" id="totalLeds" value="8">
        </div>
        
        <div class="form-group">
            <label>LED GPIO Pin:</label>
            <input type="number" id="ledGpio" value="48" min="0" max="48">
            <small style="color: #888;">(requires restart)</small>
        </div>
        
        <button id="btnSaveConfig" onclick="saveConfig()">Save Configuration</button>
        <button id="btnReload" onclick="loadConfig()">Reload</button>
    </div>

    <script>
        function setButtonLoading(btnId, loading, loadingText) {
            const btn = document.getElementById(btnId);
            if (!btn) return;
            if (loading) {
                btn.dataset.originalText = btn.textContent;
                btn.innerHTML = '<span class="spinner"></span>' + (loadingText || btn.textContent);
                btn.disabled = true;
            } else {
                btn.innerHTML = btn.dataset.originalText || btn.textContent.replace(/<[^>]*>/g, '');
                btn.disabled = false;
            }
        }

        let config = {
            wifi: { ssid: '', password: '' },
            ip: { use_dhcp: true, ip: null, gateway: null, subnet: null, dns: null },
            obd2: { dongle_ip: '192.168.0.10', dongle_port: 35000, listen_port: 35000 },
            log_level: 'info',
            thresholds: [
                { name: 'Off', rpm: 0, start_led: 0, end_led: 0, color: { r: 0, g: 0, b: 0 }, blink: false, blink_ms: 500 },
                { name: 'Blue', rpm: 1000, start_led: 0, end_led: 0, color: { r: 0, g: 0, b: 255 }, blink: false, blink_ms: 500 },
                { name: 'Green', rpm: 1500, start_led: 0, end_led: 0, color: { r: 0, g: 255, b: 0 }, blink: false, blink_ms: 500 },
                { name: 'Yellow', rpm: 2000, start_led: 0, end_led: 0, color: { r: 255, g: 255, b: 0 }, blink: false, blink_ms: 500 },
                { name: 'Red', rpm: 2500, start_led: 0, end_led: 0, color: { r: 255, g: 0, b: 0 }, blink: false, blink_ms: 500 },
                { name: 'Off', rpm: 3000, start_led: 0, end_led: 0, color: { r: 0, g: 0, b: 0 }, blink: false, blink_ms: 500 },
                { name: 'Shift', rpm: 3000, start_led: 0, end_led: 0, color: { r: 0, g: 0, b: 255 }, blink: true, blink_ms: 500 }
            ],
            total_leds: 1,
            led_gpio: 48
        };

        function toggleStaticIp() {
            const mode = document.getElementById('ipMode').value;
            const fields = document.getElementById('staticIpFields');
            fields.className = mode === 'static' ? '' : 'hidden';
        }

        function togglePasswordVisibility() {
            const input = document.getElementById('wifiPassword');
            const button = event.target;
            if (input.type === 'password') {
                input.type = 'text';
                button.textContent = 'Hide';
            } else {
                input.type = 'password';
                button.textContent = 'Show';
            }
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
                div.innerHTML = '<h3>' + (threshold.name || 'Threshold ' + (index + 1)) + '</h3>' +
                    '<div class="form-group">' +
                        '<label>Name:</label>' +
                        '<input type="text" value="' + (threshold.name || '') + '" onchange="updateThreshold(' + index + ', \'name\', this.value)">' +
                    '</div>' +
                    '<div class="form-group">' +
                        '<label>RPM:</label>' +
                        '<input type="number" value="' + threshold.rpm + '" onchange="updateThreshold(' + index + ', \'rpm\', parseInt(this.value))">' +
                    '</div>' +
                    '<div class="form-group">' +
                        '<label>Start LED:</label>' +
                        '<input type="number" min="0" value="' + threshold.start_led + '" onchange="updateThreshold(' + index + ', \'start_led\', parseInt(this.value))">' +
                    '</div>' +
                    '<div class="form-group">' +
                        '<label>End LED:</label>' +
                        '<input type="number" min="0" value="' + threshold.end_led + '" onchange="updateThreshold(' + index + ', \'end_led\', parseInt(this.value))">' +
                    '</div>' +
                    '<div class="form-group">' +
                        '<label>Color:</label>' +
                        '<input type="color" value="' + rgbToHex(threshold.color) + '" onchange="updateThreshold(' + index + ', \'color\', hexToRgb(this.value))">' +
                    '</div>' +
                    '<div class="form-group">' +
                        '<label>Blink:</label>' +
                        '<input type="checkbox" ' + (threshold.blink ? 'checked' : '') + ' onchange="updateThreshold(' + index + ', \'blink\', this.checked)">' +
                    '</div>' +
                    '<div class="form-group">' +
                        '<label>Blink ms:</label>' +
                        '<input type="number" min="50" step="50" value="' + (threshold.blink_ms || 500) + '" onchange="updateThreshold(' + index + ', \'blink_ms\', parseInt(this.value))">' +
                    '</div>' +
                    '<button onclick="moveThreshold(' + index + ', -1)">▲ Up</button>' +
                    '<button onclick="moveThreshold(' + index + ', 1)">▼ Down</button>' +
                    '<button onclick="removeThreshold(' + index + ')">Remove</button>';
                container.appendChild(div);
            });
        }

        function updateThreshold(index, field, value) {
            config.thresholds[index][field] = value;
            if (field === 'name') renderThresholds();
        }

        function addThreshold() {
            config.thresholds.push({
                name: 'New Threshold',
                rpm: 5000,
                start_led: 0,
                end_led: 7,
                color: { r: 255, g: 0, b: 0 },
                blink: false,
                blink_ms: 500
            });
            renderThresholds();
        }

        function moveThreshold(index, direction) {
            const newIndex = index + direction;
            if (newIndex < 0 || newIndex >= config.thresholds.length) return;
            const temp = config.thresholds[index];
            config.thresholds[index] = config.thresholds[newIndex];
            config.thresholds[newIndex] = temp;
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
            config.total_leds = parseInt(document.getElementById('totalLeds').value);
            config.led_gpio = parseInt(document.getElementById('ledGpio').value);
            config.obd2 = {
                dongle_ip: document.getElementById('obd2DongleIp').value || '192.168.0.10',
                dongle_port: parseInt(document.getElementById('obd2DonglePort').value) || 35000,
                listen_port: parseInt(document.getElementById('obd2ListenPort').value) || 35000
            };
            config.log_level = document.getElementById('logLevel').value;
            
            setButtonLoading('btnSaveConfig', true, 'Saving...');
            try {
                const response = await fetch('/api/config', {
                    method: 'POST',
                    headers: {
                        'Content-Type': 'application/json',
                    },
                    body: JSON.stringify(config)
                });
                
                if (response.ok) {
                    const result = await response.json().catch(() => ({}));
                    if (result.restart) {
                        showStatus('Configuration saved! Restarting device...', false);
                    } else {
                        showStatus('Configuration saved successfully!', false);
                    }
                } else {
                    showStatus('Failed to save configuration', true);
                }
            } catch (error) {
                showStatus('Error: ' + error.message, true);
            } finally {
                setButtonLoading('btnSaveConfig', false);
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
            config.ip = ipConfig;
            
            setButtonLoading('btnSaveWifi', true, 'Connecting...');
            try {
                const response = await fetch('/api/wifi', {
                    method: 'POST',
                    headers: {
                        'Content-Type': 'application/json',
                    },
                    body: JSON.stringify({ ssid, password: password || null, ip: ipConfig })
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
            } finally {
                setButtonLoading('btnSaveWifi', false);
            }
        }

        async function scanWifi() {
            setButtonLoading('btnScanWifi', true, 'Scanning...');
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
                                    n.ssid +
                                '</button>'
                            ).join('');
                    }
                } else {
                    showStatus('Failed to scan networks', true);
                }
            } catch (error) {
                showStatus('Error: ' + error.message, true);
            } finally {
                setButtonLoading('btnScanWifi', false);
            }
        }

        async function rebootDevice() {
            if (!confirm('Are you sure you want to reboot the device?')) {
                return;
            }
            
            setButtonLoading('btnReboot', true, 'Rebooting...');
            try {
                const response = await fetch('/api/reboot', {
                    method: 'POST'
                });
                
                if (response.ok) {
                    showStatus('Device is rebooting...', false);
                    // Disable the button since device is restarting
                    document.getElementById('btnReboot').disabled = true;
                } else {
                    showStatus('Failed to reboot device', true);
                    setButtonLoading('btnReboot', false);
                }
            } catch (error) {
                // Expected to fail as device disconnects
                showStatus('Device is rebooting...', false);
            }
        }

        async function loadConfig() {
            setButtonLoading('btnReload', true, 'Loading...');
            try {
                const response = await fetch('/api/config');
                if (response.ok) {
                    config = await response.json();
                    document.getElementById('totalLeds').value = config.total_leds;
                    document.getElementById('ledGpio').value = config.led_gpio || 48;
                    document.getElementById('wifiSsid').value = config.wifi?.ssid || '';
                    document.getElementById('wifiPassword').value = config.wifi?.password || '';
                    
                    // IP config
                    const ipConfig = config.ip || { use_dhcp: true };
                    document.getElementById('ipMode').value = ipConfig.use_dhcp ? 'dhcp' : 'static';
                    document.getElementById('staticIp').value = ipConfig.ip || '';
                    document.getElementById('staticGateway').value = ipConfig.gateway || '';
                    document.getElementById('staticSubnet').value = ipConfig.subnet || '';
                    document.getElementById('staticDns').value = ipConfig.dns || '';
                    toggleStaticIp();
                    
                    // OBD2 config
                    const obd2Config = config.obd2 || { dongle_ip: '192.168.0.10', dongle_port: 35000, listen_port: 35000 };
                    document.getElementById('obd2DongleIp').value = obd2Config.dongle_ip || '192.168.0.10';
                    document.getElementById('obd2DonglePort').value = obd2Config.dongle_port || 35000;
                    document.getElementById('obd2ListenPort').value = obd2Config.listen_port || 35000;
                    
                    // Log level
                    document.getElementById('logLevel').value = config.log_level || 'info';
                    
                    renderThresholds();
                    showStatus('Configuration loaded!', false);
                } else {
                    showStatus('Failed to load configuration', true);
                }
            } catch (error) {
                showStatus('Error: ' + error.message, true);
            } finally {
                setButtonLoading('btnReload', false);
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

        let rpmEventSource = null;

        function setupRpmEventSource() {
            // Close existing connection if any
            if (rpmEventSource) {
                rpmEventSource.close();
                rpmEventSource = null;
            }

            // SSE runs on a separate port to avoid blocking the HTTP server
            const sseUrl = `http://${window.location.hostname}:${SSE_PORT}/`;
            rpmEventSource = new EventSource(sseUrl);
            rpmEventSource.onmessage = function(event) {
                const data = JSON.parse(event.data);
                document.getElementById('currentRpm').textContent = data.rpm !== null ? data.rpm : '---';
            };
            rpmEventSource.onerror = function() {
                document.getElementById('currentRpm').textContent = '---';
                // EventSource will automatically reconnect, no need to manually reconnect
            };
        }

        function formatBytes(bytes) {
            if (bytes < 1024) return bytes + ' B';
            if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + ' KB';
            return (bytes / (1024 * 1024)).toFixed(2) + ' MB';
        }

        async function loadDebugInfo() {
            try {
                const response = await fetch('/api/debug_info');
                const info = await response.json();
                
                document.getElementById('freeHeap').textContent = formatBytes(info.free_heap);
                document.getElementById('minFreeHeap').textContent = formatBytes(info.min_free_heap);
                
                const el = document.getElementById('atCommands');
                if (info.at_commands.length === 0) {
                    el.textContent = '(none yet)';
                } else {
                    el.textContent = info.at_commands.join(', ');
                }
                
                const pidEl = document.getElementById('pids');
                if (info.pids.length === 0) {
                    pidEl.textContent = '(none yet)';
                } else {
                    pidEl.textContent = info.pids.join(', ');
                }
            } catch (e) {
                document.getElementById('atCommands').textContent = '(error)';
                document.getElementById('pids').textContent = '(error)';
                document.getElementById('freeHeap').textContent = '---';
                document.getElementById('minFreeHeap').textContent = '---';
            }
        }

        // Initialize
        renderThresholds();
        loadConfig();
        loadMode();
        loadNetworkStatus();
        setupRpmEventSource();
        loadDebugInfo();
        
        // Poll network status (RPM uses SSE now)
        setInterval(loadNetworkStatus, 5000);
        // Poll debug info every second (only when debug section is open)
        setInterval(() => {
            const details = document.querySelector('.debug-section');
            if (details && details.open) loadDebugInfo();
        }, 1000);
    </script>
</body>
</html>
"#;

// Captive portal redirect page
const HTML_CAPTIVE_PORTAL: &str = r#"
<!DOCTYPE html>
<html>
<head>
    <meta charset="UTF-8">
    <title>TachTalk Setup</title>
    <meta http-equiv="refresh" content="0;url=http://192.168.71.1/">
</head>
<body>
    <p>Redirecting to <a href="http://192.168.71.1/">TachTalk Setup</a>...</p>
</body>
</html>
"#;

#[allow(clippy::too_many_lines)] // Route registration function - length is proportional to endpoints
pub fn start_server(
    config: &Arc<Mutex<Config>>,
    wifi_mode: &Arc<Mutex<WifiMode>>,
    wifi: &Arc<Mutex<BlockingWifi<EspWifi<'static>>>>,
    ap_hostname: Option<String>,
    at_command_log: AtCommandLog,
    pid_log: PidLog,
) -> Result<()> {
    info!("Web server starting...");
    
    // Enable wildcard URI matching for captive portal fallback handler
    // Enable LRU purge to handle abrupt disconnections from captive portal browsers
    // LWIP max is 16 sockets; leave room for DNS, OBD2, mDNS
    let server_config = Configuration {
        uri_match_wildcard: true,
        max_open_sockets: 10,
        session_timeout: core::time::Duration::from_secs(2),
        lru_purge_enable: true,
        ..Default::default()
    };
    let mut server = EspHttpServer::new(&server_config)?;

    // Serve the main HTML page (inject SSE port between two static parts)
    server.fn_handler("/", Method::Get, |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        let mut response = req.into_ok_response()?;
        response.write_all(HTML_INDEX_START.as_bytes())?;
        response.write_all(SSE_PORT.to_string().as_bytes())?;
        response.write_all(HTML_INDEX_END.as_bytes())?;
        Ok(())
    })?;

    // GET mode endpoint
    let mode_clone = wifi_mode.clone();
    server.fn_handler("/api/mode", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        info!("HTTP: GET /api/mode");
        let mode = mode_clone.lock().unwrap();
        let mode_str = match *mode {
            WifiMode::AccessPoint => "ap",
            WifiMode::Client => "client",
        };
        let json = format!(r#"{{"mode":"{mode_str}"}}"#);
        
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // GET config endpoint
    let config_clone = config.clone();
    server.fn_handler("/api/config", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        info!("HTTP: GET /api/config");
        let cfg = config_clone.lock().unwrap();
        let json = serde_json::to_string(&*cfg).unwrap();
        
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // POST config endpoint
    let config_clone = config.clone();
    server.fn_handler("/api/config", Method::Post, move |mut req| -> Result<(), esp_idf_svc::io::EspIOError> {
        info!("HTTP: POST /api/config");
        let mut buf = vec![0u8; 2048];
        let bytes_read = req.read(&mut buf)?;
        
        if let Ok(mut new_config) = serde_json::from_slice::<Config>(&buf[..bytes_read]) {
            // Validate/clamp values to safe ranges
            new_config.validate();
            
            debug!("Config update: {} thresholds, log_level={:?}", 
                   new_config.thresholds.len(), new_config.log_level);
            
            let needs_restart = {
                let cfg = config_clone.lock().unwrap();
                cfg.led_gpio != new_config.led_gpio
            };
            
            {
                let mut cfg = config_clone.lock().unwrap();
                *cfg = new_config;
                if let Err(e) = cfg.save() {
                    warn!("Failed to save config: {e}");
                }
            }
            
            if needs_restart {
                info!("LED GPIO changed, restarting in 2 seconds...");
                let mut response = req.into_ok_response()?;
                response.write_all(b"{\"restart\":true}")?;
                std::thread::spawn(|| {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    unsafe { esp_idf_svc::sys::esp_restart(); }
                });
            } else {
                req.into_ok_response()?;
            }
        } else {
            warn!("Invalid config JSON received");
            req.into_status_response(400)?;
        }
        
        Ok(())
    })?;

    // POST wifi endpoint - save wifi and restart
    let config_clone = config.clone();
    server.fn_handler("/api/wifi", Method::Post, move |mut req| -> Result<(), esp_idf_svc::io::EspIOError> {
        info!("HTTP: POST /api/wifi");
        let mut buf = vec![0u8; 1024];
        let bytes_read = req.read(&mut buf)?;
        
        if let Ok(wifi_req) = serde_json::from_slice::<WifiRequest>(&buf[..bytes_read]) {
            info!("WiFi config update: ssid={:?}, dhcp={:?}", 
                  wifi_req.ssid, wifi_req.ip.as_ref().map(|i| i.use_dhcp));
            let mut cfg = config_clone.lock().unwrap();
            cfg.wifi.ssid = wifi_req.ssid;
            cfg.wifi.password = wifi_req.password.filter(|p| !p.is_empty());
            
            // Update IP config if provided
            if let Some(ip_cfg) = wifi_req.ip {
                cfg.ip.use_dhcp = ip_cfg.use_dhcp;
                cfg.ip.ip = ip_cfg.ip.filter(|s| !s.is_empty());
                cfg.ip.gateway = ip_cfg.gateway.filter(|s| !s.is_empty());
                cfg.ip.subnet = ip_cfg.subnet.filter(|s| !s.is_empty());
                cfg.ip.dns = ip_cfg.dns.filter(|s| !s.is_empty());
            }
            
            if let Err(e) = cfg.save() {
                error!("Failed to save config: {e:?}");
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
        info!("HTTP: GET /api/wifi/scan");
        let mut wifi = wifi_clone.lock().unwrap();
        
        let networks: Vec<Network> = match wifi.scan() {
            Ok(aps) => {
                debug!("WiFi scan found {} networks", aps.len());
                // Deduplicate by SSID, keeping strongest signal
                let mut best_by_ssid: HashMap<String, i8> = HashMap::new();
                for ap in &aps {
                    let ssid = ap.ssid.to_string();
                    if ssid.is_empty() {
                        continue;
                    }
                    best_by_ssid
                        .entry(ssid)
                        .and_modify(|rssi| *rssi = (*rssi).max(ap.signal_strength))
                        .or_insert(ap.signal_strength);
                }
                // Convert to vec and sort by signal strength (strongest first)
                let mut networks: Vec<Network> = best_by_ssid
                    .into_iter()
                    .map(|(ssid, rssi)| Network { ssid, rssi })
                    .collect();
                networks.sort_by(|a, b| b.rssi.cmp(&a.rssi));
                networks
            }
            Err(e) => {
                error!("WiFi scan failed: {e:?}");
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
        info!("HTTP: GET /api/network");
        let wifi = wifi_clone.lock().unwrap();
        
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
            dns: ip_info.as_ref().and_then(|i| i.dns.map(|d| format!("{d}"))),
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
        info!("HTTP: GET /api/rpm");
        // This is just a fallback, real updates come via SSE on port 8081
        let json = r#"{"rpm":null}"#;
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // GET debug info endpoint (AT commands, PIDs, memory stats, etc.)
    server.fn_handler("/api/debug_info", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        debug!("HTTP: GET /api/debug_info");
        
        let at_commands: Vec<String> = at_command_log
            .lock()
            .map(|log| {
                let mut cmds: Vec<String> = log.iter().cloned().collect();
                cmds.sort();
                cmds
            })
            .unwrap_or_default();
        
        let pids: Vec<String> = pid_log
            .lock()
            .map(|log| {
                let mut pids: Vec<String> = log.iter().cloned().collect();
                pids.sort();
                pids
            })
            .unwrap_or_default();
        
        // SAFETY: These are simple C functions that return u32 values
        let free_heap = unsafe { esp_get_free_heap_size() };
        let min_free_heap = unsafe { esp_get_minimum_free_heap_size() };
        
        let info = DebugInfo {
            at_commands,
            pids,
            free_heap,
            min_free_heap,
        };
        
        let json = serde_json::to_string(&info).unwrap_or_else(|_| r#"{"at_commands":[],"free_heap":0,"min_free_heap":0}"#.to_string());
        
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // POST reboot endpoint
    server.fn_handler("/api/reboot", Method::Post, |req| -> Result<(), esp_idf_svc::io::EspIOError> {
        info!("HTTP: POST /api/reboot - Device reboot requested");
        
        req.into_ok_response()?;
        
        // Schedule restart after response is sent
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(1));
            info!("Rebooting device now...");
            unsafe {
                esp_idf_svc::sys::esp_restart();
            }
        });
        
        Ok(())
    })?;

    // Captive portal fallback handler - redirect requests with wrong Host header
    // Must be registered last as it's a wildcard that matches everything
    if let Some(hostname) = ap_hostname {
        let valid_hosts: Vec<String> = vec![
            hostname.clone(),
            format!("{hostname}.local"),
            "192.168.71.1".to_string(),
        ];
        
        info!("Captive portal enabled for hostname: {hostname}");
        
        server.fn_handler("/*", Method::Get, move |req| -> Result<(), esp_idf_svc::io::EspIOError> {
            // Check Host header
            let host = req.header("Host").unwrap_or("");
            let host_lower = host.to_lowercase();
            // Strip port if present
            let host_without_port = host_lower.split(':').next().unwrap_or("");
            
            let is_valid_host = valid_hosts.iter().any(|h| h == host_without_port);
            
            if is_valid_host {
                // Valid host but unknown path - return 404
                info!("HTTP: GET {} -> 404 (host: {})", req.uri(), host);
                req.into_status_response(404)?;
            } else {
                // Wrong host - redirect to captive portal
                info!("HTTP: GET {} -> 302 captive (host: {})", req.uri(), host);
                let mut response = req.into_response(302, Some("Found"), &[
                    ("Location", "http://192.168.71.1/"),
                    ("Cache-Control", "no-cache"),
                    ("Connection", "close"),
                ])?;
                response.write_all(HTML_CAPTIVE_PORTAL.as_bytes())?;
            }
            Ok(())
        })?;
    }

    info!("Web server started on http://0.0.0.0:80");
    
    // Keep server alive
    std::mem::forget(server);
    
    Ok(())
}
