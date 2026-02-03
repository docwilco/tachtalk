//! Mock ELM327 OBD2 adapter for testing TachTalk proxy
//!
//! Usage: cargo run --example mock_elm327
//! Then connect TachTalk proxy to 127.0.0.1:35000

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Instant;

fn main() {
    println!("Mock ELM327 starting on 0.0.0.0:35000...");
    let listener = TcpListener::bind("0.0.0.0:35000").expect("Failed to bind");
    println!("Mock ELM327 ready - waiting for connections...");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                println!("Client connected: {:?}", stream.peer_addr());
                std::thread::spawn(|| handle_client(stream));
            }
            Err(e) => eprintln!("Connection error: {e}"),
        }
    }
}

fn handle_client(mut stream: TcpStream) {
    let mut buffer = Vec::new();
    let mut byte = [0u8; 1];
    let start_time = Instant::now();

    loop {
        match stream.read(&mut byte) {
            Ok(0) => {
                println!("Client disconnected");
                break;
            }
            Ok(_) => {
                let ch = byte[0];
                
                // Carriage return terminates command
                if ch == b'\r' {
                    let command = String::from_utf8_lossy(&buffer).trim().to_uppercase();
                    
                    if !command.is_empty() {
                        println!("RX: {}", command);
                        let response = process_command(&command, &start_time);
                        println!("TX: {}", response.escape_debug());
                        
                        if let Err(e) = stream.write_all(response.as_bytes()) {
                            eprintln!("Write error: {e}");
                            break;
                        }
                    }
                    
                    buffer.clear();
                } else if ch != b'\n' {
                    // Accumulate command (ignore linefeeds)
                    buffer.push(ch);
                }
            }
            Err(e) => {
                eprintln!("Read error: {e}");
                break;
            }
        }
    }
}

fn process_command(cmd: &str, start_time: &Instant) -> String {
    match cmd {
        // Reset
        "ATZ" => "\r\rELM327 v1.5\r\r>".to_string(),
        
        // Echo off/on
        "ATE0" => "OK\r\r>".to_string(),
        "ATE1" => "OK\r\r>".to_string(),
        
        // Linefeeds off/on
        "ATL0" => "OK\r\r>".to_string(),
        "ATL1" => "OK\r\r>".to_string(),
        
        // Spaces off/on
        "ATS0" => "OK\r\r>".to_string(),
        "ATS1" => "OK\r\r>".to_string(),
        
        // Headers off/on
        "ATH0" => "OK\r\r>".to_string(),
        "ATH1" => "OK\r\r>".to_string(),
        
        // Protocol selection
        "ATSP0" => "OK\r\r>".to_string(),
        c if c.starts_with("ATSP") => "OK\r\r>".to_string(),
        c if c.starts_with("ATST") => "OK\r\r>".to_string(),
        c if c.starts_with("ATAT") => "OK\r\r>".to_string(),
        
        // Device info
        "ATI" => "ELM327 v1.5\r\r>".to_string(),
        "AT@1" => "Mock ELM327\r\r>".to_string(),
        
        // OBD2 PIDs supported queries
        "0100" => "4100BE3FA813\r\r>".to_string(), // PIDs 01-20 supported
        "0120" => "412080000001\r\r>".to_string(), // PIDs 21-40 supported
        "0140" => "4140FED08000\r\r>".to_string(), // PIDs 41-60 supported
        "0160" => "NO DATA\r\r>".to_string(),
        
        // Mode 01 - Current data
        "010C" => {
            // RPM - simulate varying RPM (800-3000 RPM with wave pattern)
            let elapsed = start_time.elapsed().as_secs_f32();
            let rpm_variation = (elapsed * 0.5).sin() * 1100.0;
            let rpm = 1900.0 + rpm_variation;
            let rpm_raw = (rpm * 4.0) as u32;
            format!("410C{:04X}\r\r>", rpm_raw)
        }
        "010D" => "410D28\r\r>".to_string(),      // Speed: 40 km/h
        "0105" => "01054F\r\r>".to_string(),      // Coolant temp: 39°C
        "010F" => "010F38\r\r>".to_string(),      // Intake air temp: 16°C
        "0111" => "011145\r\r>".to_string(),      // Throttle: 27%
        "0104" => "010464\r\r>".to_string(),      // Engine load: 39.2%
        
        // Multi-PID request (example: RPM + Speed + Coolant)
        c if c.contains("010C") && c.len() > 4 => {
            let elapsed = start_time.elapsed().as_secs_f32();
            let rpm_variation = (elapsed * 0.5).sin() * 1100.0;
            let rpm = 1900.0 + rpm_variation;
            let rpm_raw = (rpm * 4.0) as u32;
            format!("410C{:04X}0D280F38\r\r>", rpm_raw) // RPM + Speed + IAT
        }
        
        // Mode 03 - Show stored DTCs
        "03" => "4300\r\r>".to_string(), // No DTCs
        
        // Mode 09 - Vehicle info
        "0902" => "490213455034353637383930\r\r>".to_string(), // VIN part 1
        
        // Unknown command
        _ => "?\r\r>".to_string(),
    }
}
