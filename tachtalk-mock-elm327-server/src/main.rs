//! Mock ELM327 OBD2 adapter for testing TachTalk proxy

use std::io::{self, ErrorKind};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use clap::Parser;
use tachtalk_elm327_lib::{Command, Identity, ReplyLine, ServerSession};

/// Mock ELM327 OBD2 adapter for testing TachTalk proxy.
///
/// Connect TachTalk proxy to 127.0.0.1:35000
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Suppress request/response logging
    #[arg(short, long)]
    quiet: bool,

    /// Maximum requests per second (throttle responses to this rate)
    #[arg(short = 'm', long, value_name = "RPS")]
    max_rps: Option<f64>,
}

/// The mock's identity strings: the stock ATI banner, a mock-specific
/// device description so clients can tell it from real hardware.
fn identity() -> Identity {
    Identity {
        ati: "ELM327 v1.5".to_string(),
        device_description: "Mock ELM327".to_string(),
    }
}

fn main() {
    let args = Args::parse();

    let min_interval = args.max_rps.map(|rps| {
        assert!(rps > 0.0, "--max-rps must be positive");
        Duration::from_secs_f64(1.0 / rps)
    });
    let start_time = Instant::now();

    println!("Mock ELM327 starting on 0.0.0.0:35000...");
    let listener = TcpListener::bind("0.0.0.0:35000").expect("Failed to bind");

    if let Some(interval) = min_interval {
        println!(
            "Rate limit: {:.1} req/s (min interval: {:.1}ms)",
            1.0 / interval.as_secs_f64(),
            interval.as_secs_f64() * 1000.0
        );
    } else {
        println!("Rate limit: unlimited");
    }

    println!("Mock ELM327 ready - waiting for connections...");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                println!("Client connected: {:?}", stream.peer_addr());
                let quiet = args.quiet;
                std::thread::spawn(move || handle_client(stream, quiet, start_time, min_interval));
            }
            Err(e) => eprintln!("Connection error: {e}"),
        }
    }
}

/// Sleep out the remainder of the rate-limit interval since the last
/// response, if a limit is set.
fn rate_limit_wait(min_interval: Option<Duration>, last_response_time: Option<Instant>) {
    if let (Some(interval), Some(last_time)) = (min_interval, last_response_time) {
        if let Some(remaining) = interval.checked_sub(last_time.elapsed()) {
            std::thread::sleep(remaining);
        }
    }
}

/// How the mock answers an OBD command.
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    /// One line from the engine ECU.
    Reply(ReplyLine),
    /// A rejection line such as `?` or `NO DATA`.
    Reject(&'static str),
}

fn handle_client(
    stream: TcpStream,
    quiet: bool,
    start_time: Instant,
    min_interval: Option<Duration>,
) {
    let mut session = ServerSession::new(stream, identity());
    let mut last_response_time: Option<Instant> = None;

    loop {
        let result = match session.next_command() {
            Ok(Command::At(at)) => {
                if !quiet {
                    println!("RX: {}", at.command().to_uppercase());
                }
                rate_limit_wait(min_interval, last_response_time);
                last_response_time = Some(Instant::now());
                at.answer()
            }
            Ok(Command::Obd(obd)) => {
                if !quiet {
                    println!("RX: {}", obd.command().to_uppercase());
                }
                rate_limit_wait(min_interval, last_response_time);
                let answer = respond_obd(obd.command(), &start_time);
                last_response_time = Some(Instant::now());
                if !quiet {
                    println!("TX: {answer:?}");
                }
                answer_with(obd, answer)
            }
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => {
                println!("Client disconnected");
                break;
            }
            Err(e) => {
                eprintln!("Read error: {e}");
                break;
            }
        };
        if let Err(e) = result {
            eprintln!("Write error: {e}");
            break;
        }
    }
}

fn answer_with<S: io::Read + io::Write>(
    obd: tachtalk_elm327_lib::ObdCommand<'_, S>,
    answer: Answer,
) -> io::Result<()> {
    match answer {
        Answer::Reply(line) => obd.reply(&[line]),
        Answer::Reject(text) => obd.reject(text),
    }
}

fn get_rpm_value(start_time: &Instant) -> u32 {
    const MIN_RPM: f64 = 800.0;
    const MAX_RPM: f64 = 3500.0;
    const BLIP_RPM: f64 = 2500.0;
    const RAMP_TIME: f64 = 4.0;
    const HOLD_TIME: f64 = 3.0;
    const BLIP_TIME: f64 = 0.4; // Time for each half of a blip (up or down)
                                // Cycle: ramp up (4s) + hold max (3s) + ramp down (4s) + blip1 (0.8s) + blip2 (0.8s) + hold min (1.4s)
    const CYCLE_TIME: f64 = 2.0 * RAMP_TIME + HOLD_TIME + 4.0 * BLIP_TIME + 1.4;

    let elapsed = start_time.elapsed().as_secs_f64();
    let phase = elapsed % CYCLE_TIME;

    let ramp_down_end = 2.0 * RAMP_TIME + HOLD_TIME; // 11s
    let blip1_up_end = ramp_down_end + BLIP_TIME; // 11.4s
    let blip1_down_end = blip1_up_end + BLIP_TIME; // 11.8s
    let blip2_up_end = blip1_down_end + BLIP_TIME; // 12.2s
    let blip2_down_end = blip2_up_end + BLIP_TIME; // 12.6s

    let rpm = if phase < RAMP_TIME {
        // Slow ramp up
        MIN_RPM + (MAX_RPM - MIN_RPM) * (phase / RAMP_TIME)
    } else if phase < RAMP_TIME + HOLD_TIME {
        // Hold at max
        MAX_RPM
    } else if phase < ramp_down_end {
        // Slow ramp down
        let ramp_phase = phase - RAMP_TIME - HOLD_TIME;
        MAX_RPM - (MAX_RPM - MIN_RPM) * (ramp_phase / RAMP_TIME)
    } else if phase < blip1_up_end {
        // Blip 1 up
        let blip_phase = phase - ramp_down_end;
        MIN_RPM + (BLIP_RPM - MIN_RPM) * (blip_phase / BLIP_TIME)
    } else if phase < blip1_down_end {
        // Blip 1 down
        let blip_phase = phase - blip1_up_end;
        BLIP_RPM - (BLIP_RPM - MIN_RPM) * (blip_phase / BLIP_TIME)
    } else if phase < blip2_up_end {
        // Blip 2 up
        let blip_phase = phase - blip1_down_end;
        MIN_RPM + (BLIP_RPM - MIN_RPM) * (blip_phase / BLIP_TIME)
    } else if phase < blip2_down_end {
        // Blip 2 down
        let blip_phase = phase - blip2_up_end;
        BLIP_RPM - (BLIP_RPM - MIN_RPM) * (blip_phase / BLIP_TIME)
    } else {
        // Hold at min before next cycle
        MIN_RPM
    };

    let scaled = rpm * 4.0;
    assert!(scaled >= 0.0 && scaled <= f64::from(u32::MAX));
    // Workaround: allow on statement requires https://github.com/rust-lang/rust/issues/15701
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let result = scaled as u32;
    result
}

/// Coolant temperature in OBD2 encoded byte.
///
/// Normal cycles (~90°C), spike cycles (ramp to ~125°C and back).
/// Alternates every RPM cycle (`CYCLE_TIME` = 14s).
fn get_coolant_temp_byte(start_time: &Instant) -> u8 {
    const CYCLE_TIME: f64 = 14.0; // must match RPM CYCLE_TIME
    const NORMAL_TEMP: f64 = 90.0;
    const SPIKE_PEAK: f64 = 125.0;

    let elapsed = start_time.elapsed().as_secs_f64();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let cycle_number = (elapsed / CYCLE_TIME) as u64;
    let phase = elapsed % CYCLE_TIME;

    let temp = if cycle_number % 2 == 1 {
        // Spike cycle: ramp up to peak at midpoint, then back down
        let half = CYCLE_TIME / 2.0;
        let t = if phase < half {
            phase / half
        } else {
            (CYCLE_TIME - phase) / half
        };
        // Smooth with sine easing
        let eased = (t * std::f64::consts::FRAC_PI_2).sin();
        NORMAL_TEMP + (SPIKE_PEAK - NORMAL_TEMP) * eased
    } else {
        NORMAL_TEMP
    };

    // OBD2 encoding: byte = temp_C + 40
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let byte = (temp + 40.0).clamp(0.0, 255.0) as u8;
    byte
}

/// Vehicle speed in OBD2 encoded byte (= km/h directly).
///
/// Sine wave: 60 ± 3 km/h with ~8s period.
fn get_speed_byte(start_time: &Instant) -> u8 {
    const CENTER: f64 = 60.0;
    const AMPLITUDE: f64 = 3.0;
    const PERIOD: f64 = 8.0;

    let elapsed = start_time.elapsed().as_secs_f64();
    let speed = CENTER + AMPLITUDE * (2.0 * std::f64::consts::PI * elapsed / PERIOD).sin();

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let byte = speed.round().clamp(0.0, 255.0) as u8;
    byte
}

fn get_pid_response(pid: &str, start_time: &Instant) -> Option<String> {
    match pid {
        "00" => Some("BE3FA813".to_string()), // PIDs supported 01-20
        "04" => Some("64".to_string()),       // Engine load: 39.2%
        "05" => Some(format!("{:02X}", get_coolant_temp_byte(start_time))), // Coolant temp (dynamic)
        "0C" => Some(format!("{:04X}", get_rpm_value(start_time))),         // RPM
        "0D" => Some(format!("{:02X}", get_speed_byte(start_time))),        // Speed (dynamic)
        "0F" => Some("38".to_string()),                                     // Intake air temp: 16°C
        "11" => Some("45".to_string()),                                     // Throttle: 27%
        "20" => Some("80000001".to_string()),                               // PIDs supported 21-40
        "40" => Some("FED08000".to_string()),                               // PIDs supported 41-60
        #[allow(clippy::match_same_arms)] // Different PIDs, same value is coincidental
        "49" => Some("45".to_string()), // Accelerator pedal position D: 27%
        _ => None,
    }
}

/// The mock engine ECU's answer to a non-AT command, canonical (uppercase,
/// no spaces; the repeated command if the client sent a bare CR): PID
/// dispatch over simulated values.
fn respond_obd(command: &str, start_time: &Instant) -> Answer {
    let hex_data = match command {
        // Mode 03 - Show stored DTCs
        "03" => "4300".to_string(),

        // Mode 09 - Vehicle info
        "0902" => "490213455034353637383930".to_string(),

        // Mode 01 - Current data (single or multi-PID)
        c if c.starts_with("01") && c.len() >= 4 => {
            let pid_data = &c[2..]; // Everything after "01"

            // A response count is one digit after the PID pairs ("0C1"):
            // it tells the ELM327 how many ECU responses to wait for.
            // Without one, simulate the adaptive timing wait.
            let pid_data = if pid_data.len() % 2 == 1 {
                &pid_data[..pid_data.len() - 1]
            } else {
                std::thread::sleep(Duration::from_millis(200));
                pid_data
            };

            let mut response = String::from("41");
            for pid in pid_data.as_bytes().chunks_exact(2) {
                let pid = String::from_utf8_lossy(pid).to_uppercase();
                let Some(data) = get_pid_response(&pid, start_time) else {
                    // Unknown PID
                    return Answer::Reject("NO DATA");
                };
                response.push_str(&pid);
                response.push_str(&data);
            }
            response
        }

        // Unknown command
        _ => return Answer::Reject("?"),
    };

    Answer::Reply(ReplyLine {
        // 7E8 is the engine ECU's usual reply address.
        ecu: Some(0x7E8),
        data: hex_to_bytes(&hex_data),
    })
}

/// Decode an even-length uppercase hex string.
fn hex_to_bytes(hex: &str) -> tachtalk_elm327_lib::WireBytes {
    hex.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).expect("ASCII hex");
            u8::from_str_radix(pair, 16).expect("valid hex")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read, Write};

    /// A client connection in memory: scripted input, collected output.
    struct Duplex {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl Read for Duplex {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for Duplex {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Serve `input` the way `handle_client` does, without rate limiting,
    /// and return what the client received.
    fn serve(input: &[u8]) -> String {
        let duplex = Duplex {
            input: Cursor::new(input.to_vec()),
            output: Vec::new(),
        };
        let mut session = ServerSession::new(duplex, identity());
        let start = Instant::now();
        loop {
            match session.next_command() {
                Ok(Command::At(at)) => at.answer().unwrap(),
                Ok(Command::Obd(obd)) => {
                    let answer = respond_obd(obd.command(), &start);
                    answer_with(obd, answer).unwrap();
                }
                Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        String::from_utf8(session.into_inner().output).unwrap()
    }

    fn reply(hex: &str) -> Answer {
        Answer::Reply(ReplyLine {
            ecu: Some(0x7E8),
            data: hex_to_bytes(hex),
        })
    }

    #[test]
    fn identity_ati_and_reset_banner() {
        assert_eq!(
            serve(b"ATE0\rATI\rATZ\r"),
            "ATE0\rOK\r\n\r\n>ELM327 v1.5\r\n\r\n>\r\n\r\nELM327 v1.5\r\n\r\n>"
        );
    }

    #[test]
    fn identity_device_description() {
        // The spaced spelling reaches the same answer (the divergence #21
        // retired).
        assert_eq!(
            serve(b"ATE0\rAT@1\rAT @1\r"),
            "ATE0\rOK\r\n\r\n>Mock ELM327\r\n\r\n>Mock ELM327\r\n\r\n>"
        );
    }

    #[test]
    fn static_pid_with_response_count() {
        let start = Instant::now();
        assert_eq!(respond_obd("01041", &start), reply("410464"));
    }

    #[test]
    fn multi_pid_request() {
        let start = Instant::now();
        assert_eq!(respond_obd("0104201", &start), reply("4104642080000001"));
    }

    #[test]
    fn a_spaced_lowercase_command_is_dispatched() {
        // The session hands the mock "010C1"; the reply is RPM.
        let written = serve(b"ATE0\r01 0c 1\r");
        assert!(
            written.starts_with("ATE0\rOK\r\n\r\n>41 0C "),
            "got {written:?}"
        );
    }

    #[test]
    fn mode_03_stored_dtcs() {
        let start = Instant::now();
        assert_eq!(respond_obd("03", &start), reply("4300"));
    }

    #[test]
    fn mode_09_vehicle_info() {
        let start = Instant::now();
        assert_eq!(
            respond_obd("0902", &start),
            reply("490213455034353637383930")
        );
    }

    #[test]
    fn unknown_pid_is_no_data() {
        let start = Instant::now();
        assert_eq!(respond_obd("01991", &start), Answer::Reject("NO DATA"));
    }

    #[test]
    fn unknown_command_is_question_mark() {
        let start = Instant::now();
        assert_eq!(respond_obd("FF", &start), Answer::Reject("?"));
    }

    #[test]
    fn replies_take_the_session_shape() {
        assert_eq!(
            serve(b"ATE0\r0104 1\rATL0\rATS0\r0104 1\r0199 1\r"),
            "ATE0\rOK\r\n\r\n>\
             41 04 64\r\n\r\n>\
             OK\r\n\r\n>\
             OK\r\r>\
             410464\r\r>\
             NO DATA\r\r>"
        );
    }

    #[test]
    fn headers_carry_the_engine_ecu() {
        assert_eq!(
            serve(b"ATE0\rATH1\r0104 1\r"),
            "ATE0\rOK\r\n\r\n>OK\r\n\r\n>7E8 03 41 04 64\r\n\r\n>"
        );
    }

    #[test]
    fn a_bare_cr_repeats_the_last_replied_command() {
        // A rejected command does not replace what a bare CR repeats, and
        // the retired `1` is just an unknown command.
        assert_eq!(
            serve(b"ATE0\rATS0\r0104 1\r0199 1\r\r\r1\r"),
            "ATE0\rOK\r\n\r\n>\
             OK\r\n\r\n>\
             410464\r\n\r\n>\
             NO DATA\r\n\r\n>\
             410464\r\n\r\n>\
             410464\r\n\r\n>\
             ?\r\n\r\n>"
        );
    }
}
