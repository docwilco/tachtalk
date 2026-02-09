use clap::Parser;
use std::fmt::Write;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::net::Ipv4Addr;
use std::path::PathBuf;
use tachtalk_capture_format::{CaptureHeader, RecordIter, RecordType, HEADER_SIZE};

/// Decode TachTalk `.ttcap` capture files into human-readable output.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Path to the .ttcap capture file.
    file: PathBuf,

    /// Show raw hex dump of data payloads.
    #[arg(short = 'x', long)]
    hex: bool,

    /// Only show records of this type (tx, rx, connect, disconnect).
    #[arg(short, long)]
    filter: Option<String>,

    /// Maximum number of records to display.
    #[arg(short = 'n', long)]
    limit: Option<usize>,
}

fn main() {
    let args = Args::parse();

    let file = match File::open(&args.file) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Error opening {}: {e}", args.file.display());
            std::process::exit(1);
        }
    };
    let file_size = file.metadata().map(|m| m.len()).ok();
    let mut reader = BufReader::new(file);

    let header = match CaptureHeader::from_reader(&mut reader) {
        Ok(Some(h)) => h,
        Ok(None) => {
            eprintln!("Empty file");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Invalid capture file: {e}");
            std::process::exit(1);
        }
    };

    print_header(&header, file_size);

    // Skip any extra header bytes (future format versions may have a larger header)
    let extra = usize::from(header.header_size).saturating_sub(HEADER_SIZE);
    if extra > 0 {
        io::copy(&mut reader.by_ref().take(extra as u64), &mut io::sink()).unwrap_or_else(|e| {
            eprintln!("Error skipping extended header: {e}");
            std::process::exit(1);
        });
    }

    let filter_type = args.filter.as_deref().map(parse_filter);

    println!();
    println!("Records:");
    println!(
        "{:>8}  {:>10}  {:>12}  {:>6}  Data",
        "#", "Time (ms)", "Type", "Bytes"
    );
    println!("{}", "-".repeat(72));

    let mut count = 0u32;
    for result in RecordIter::new(reader) {
        match result {
            Ok(record) => {
                if let Some(filter) = filter_type {
                    if record.record_type != filter {
                        continue;
                    }
                }

                count += 1;
                if let Some(limit) = args.limit {
                    if count as usize > limit {
                        println!("... (truncated at {limit} records)");
                        break;
                    }
                }

                let type_label = record.record_type.label();
                let data_display = format_data(&record.data, args.hex);

                println!(
                    "{count:>8}  {ts:>10}  {ty:>12}  {len:>6}  {data}",
                    ts = record.timestamp_ms,
                    ty = type_label,
                    len = record.data.len(),
                    data = data_display,
                );
            }
            Err(e) => {
                eprintln!("Error parsing record: {e}");
                break;
            }
        }
    }

    println!("{}", "-".repeat(72));
    println!("Total records displayed: {count}");
}

fn print_header(header: &CaptureHeader, file_size: Option<u64>) {
    let ip = Ipv4Addr::from(header.dongle_ip);

    println!("=== TachTalk Capture File ===");
    match file_size {
        Some(size) => println!("File size:        {size} bytes"),
        None => println!("File size:        (unknown)"),
    }
    println!("Format version:   {}", header.version);
    println!("Header size:      {} bytes", header.header_size);
    println!("Record count:     {}", header.record_count);
    println!("Data length:      {} bytes", header.data_length);
    println!("Firmware:         {}", header.firmware_version_str());
    println!("Dongle:           {ip}:{}", header.dongle_port);

    if header.capture_start_ms > 0 {
        println!("Capture start:    {} (epoch ms)", header.capture_start_ms);
    } else {
        println!("Capture start:    (no NTP)");
    }

    let mut flags = Vec::new();
    if header.overflow() {
        flags.push("OVERFLOW");
    }
    if header.ntp_synced() {
        flags.push("NTP_SYNCED");
    }
    if flags.is_empty() {
        println!("Flags:            (none)");
    } else {
        println!("Flags:            {}", flags.join(", "));
    }
}

fn parse_filter(s: &str) -> RecordType {
    match s.to_lowercase().as_str() {
        "tx" | "client" | "send" => RecordType::ClientToDongle,
        "rx" | "dongle" | "recv" => RecordType::DongleToClient,
        "connect" | "conn" => RecordType::Connect,
        "disconnect" | "disc" => RecordType::Disconnect,
        _ => {
            eprintln!("Unknown filter: {s}. Use: tx, rx, connect, disconnect");
            std::process::exit(1);
        }
    }
}

fn format_data(data: &[u8], hex: bool) -> String {
    if data.is_empty() {
        return String::from("(no data)");
    }

    if hex {
        data.iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        // Show as text with non-printable bytes escaped
        let mut out = String::with_capacity(data.len());
        for &b in data {
            match b {
                b'\r' => out.push_str("\\r"),
                b'\n' => out.push_str("\\n"),
                b'\t' => out.push_str("\\t"),
                0x20..=0x7e => out.push(b as char),
                _ => {
                    write!(out, "\\x{b:02x}").unwrap();
                }
            }
        }
        out
    }
}
