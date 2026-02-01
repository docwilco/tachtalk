use anyhow::Result;
use embedded_svc::http::Method;
use embedded_svc::io::Write;
use esp_idf_svc::http::server::{Configuration, EspHttpServer};
use log::*;
use std::sync::{Arc, Mutex};

use crate::config::Config;
use crate::leds::LedController;

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
        input[type="number"], input[type="color"] {
            padding: 5px;
            border: 1px solid #444;
            background-color: #333;
            color: #fff;
            border-radius: 3px;
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
    </style>
</head>
<body>
    <div class="container">
        <h1>🏁 TachTalk Configuration</h1>
        
        <div id="status"></div>
        
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
            thresholds: [
                { rpm: 3000, color: { r: 0, g: 255, b: 0 }, num_leds: 2 },
                { rpm: 4000, color: { r: 255, g: 255, b: 0 }, num_leds: 4 },
                { rpm: 5000, color: { r: 255, g: 0, b: 0 }, num_leds: 6 }
            ],
            blink_rpm: 6000,
            total_leds: 8
        };

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
                div.innerHTML = `
                    <h3>Threshold ${index + 1}</h3>
                    <div class="form-group">
                        <label>RPM:</label>
                        <input type="number" value="${threshold.rpm}" onchange="updateThreshold(${index}, 'rpm', parseInt(this.value))">
                    </div>
                    <div class="form-group">
                        <label>Color:</label>
                        <input type="color" value="${rgbToHex(threshold.color)}" onchange="updateThreshold(${index}, 'color', hexToRgb(this.value))">
                    </div>
                    <div class="form-group">
                        <label>Number of LEDs:</label>
                        <input type="number" value="${threshold.num_leds}" onchange="updateThreshold(${index}, 'num_leds', parseInt(this.value))">
                    </div>
                    <button onclick="removeThreshold(${index})">Remove</button>
                `;
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

        async function loadConfig() {
            try {
                const response = await fetch('/api/config');
                if (response.ok) {
                    config = await response.json();
                    document.getElementById('blinkRpm').value = config.blink_rpm;
                    document.getElementById('totalLeds').value = config.total_leds;
                    renderThresholds();
                    showStatus('Configuration loaded!', false);
                } else {
                    showStatus('Failed to load configuration', true);
                }
            } catch (error) {
                showStatus('Error: ' + error.message, true);
            }
        }

        // Initialize
        renderThresholds();
        loadConfig();
    </script>
</body>
</html>
"#;

pub fn start_server(
    config: Arc<Mutex<Config>>,
    led_controller: Arc<Mutex<LedController>>,
) -> Result<()> {
    let server_config = Configuration::default();
    let mut server = EspHttpServer::new(&server_config)?;

    // Serve the main HTML page
    server.fn_handler("/", Method::Get, |req| {
        let mut response = req.into_ok_response()?;
        response.write_all(HTML_INDEX.as_bytes())?;
        Ok(())
    })?;

    // GET config endpoint
    let config_clone = config.clone();
    server.fn_handler("/api/config", Method::Get, move |req| {
        let cfg = config_clone.lock().unwrap();
        let json = serde_json::to_string(&*cfg).unwrap();
        
        let mut response = req.into_ok_response()?;
        response.write_all(json.as_bytes())?;
        Ok(())
    })?;

    // POST config endpoint
    let config_clone = config.clone();
    server.fn_handler("/api/config", Method::Post, move |mut req| {
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

    info!("Web server started on http://0.0.0.0:80");
    
    // Keep server alive
    std::mem::forget(server);
    
    Ok(())
}
