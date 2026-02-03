//! Mock ELM327 OBD2 adapter for testing TachTalk proxy
//!
//! Usage: cargo run -p tachtalk-mock-elm327-server
//! Then connect TachTalk proxy to 127.0.0.1:35000

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Instant;
use tachtalk_elm327_lib::ClientState;

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
    let mut state = ClientState::new();

    loop {
        match stream.read(&mut byte) {
            Ok(0) => {
                println!("Client disconnected");
                break;
            }
            Ok(_) => {
                let ch = byte[0];

                // Echo character if enabled
                if state.echo_enabled && stream.write_all(&byte).is_err() {
                    break;
                }

                // Carriage return terminates command
                if ch == b'\r' {
                    let command = String::from_utf8_lossy(&buffer).trim().to_uppercase();

                    if !command.is_empty() {
                        println!("RX: {}", command);
                        let response = process_command(&command, &start_time, &mut state);
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

fn get_rpm_value(start_time: &Instant) -> u32 {
    const MIN_RPM: f32 = 800.0;
    const MAX_RPM: f32 = 3500.0;
    const RAMP_TIME: f32 = 4.0;
    const HOLD_TIME: f32 = 3.0;
    const CYCLE_TIME: f32 = 2.0 * (RAMP_TIME + HOLD_TIME);

    let elapsed = start_time.elapsed().as_secs_f32();
    let phase = elapsed % CYCLE_TIME;

    let rpm = if phase < RAMP_TIME {
        MIN_RPM + (MAX_RPM - MIN_RPM) * (phase / RAMP_TIME)
    } else if phase < RAMP_TIME + HOLD_TIME {
        MAX_RPM
    } else if phase < 2.0 * RAMP_TIME + HOLD_TIME {
        let ramp_phase = phase - RAMP_TIME - HOLD_TIME;
        MAX_RPM - (MAX_RPM - MIN_RPM) * (ramp_phase / RAMP_TIME)
    } else {
        MIN_RPM
    };

    (rpm * 4.0) as u32
}

fn get_pid_response(pid: &str, start_time: &Instant) -> Option<String> {
    match pid {
        "00" => Some("BE3FA813".to_string()), // PIDs supported 01-20
        "04" => Some("64".to_string()),       // Engine load: 39.2%
        "05" => Some("4F".to_string()),       // Coolant temp: 39°C
        "0C" => Some(format!("{:04X}", get_rpm_value(start_time))), // RPM
        "0D" => Some("28".to_string()),       // Speed: 40 km/h
        "0F" => Some("38".to_string()),       // Intake air temp: 16°C
        "11" => Some("45".to_string()),       // Throttle: 27%
        "20" => Some("80000001".to_string()), // PIDs supported 21-40
        "40" => Some("FED08000".to_string()), // PIDs supported 41-60
        _ => None,
    }
}

fn process_command(cmd: &str, start_time: &Instant, state: &mut ClientState) -> String {
    let le = state.line_ending();

    // Handle AT commands using the shared library
    if cmd.starts_with("AT") {
        // Override AT@1 for mock server identification
        if cmd == "AT@1" {
            return format!("{le}Mock ELM327{le}>");
        }
        return state.handle_at_command(cmd);
    }

    // Handle OBD2 commands
    match cmd {
        // Mode 03 - Show stored DTCs
        "03" => format!("4300{le}{le}>"),

        // Mode 09 - Vehicle info
        "0902" => format!("490213455034353637383930{le}{le}>"),

        // Mode 01 - Current data (single or multi-PID)
        c if c.starts_with("01") && c.len() >= 4 => {
            let pid_data = &c[2..]; // Everything after "01"

            // Parse PIDs (pairs of hex digits)
            let mut pids = Vec::new();
            let mut chars = pid_data.chars().peekable();

            while chars.peek().is_some() {
                let mut pid = String::new();

                // Get next two characters
                if let Some(c1) = chars.next() {
                    if let Some(c2) = chars.next() {
                        pid.push(c1);
                        pid.push(c2);
                        pids.push(pid.to_uppercase());
                    }
                }
            }

            // Build response
            let mut response = String::new();
            for pid in pids {
                if let Some(data) = get_pid_response(&pid, start_time) {
                    response.push_str(&format!("{pid}{data}"));
                } else {
                    // Unknown PID
                    return format!("NO DATA{le}{le}>");
                }
            }

            if response.is_empty() {
                format!("?{le}{le}>")
            } else {
                format!("41{response}{le}{le}>")
            }
        }

        // Unknown command
        _ => format!("?{le}{le}>"),
    }
}
