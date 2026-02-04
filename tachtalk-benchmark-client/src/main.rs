//! ELM327 RPM benchmark client
//!
//! Connects to an ELM327-compatible server and requests RPM as fast as possible,
//! printing statistics for benchmarking.
//!
//! Usage: cargo run -p tachtalk-benchmark-client -- [OPTIONS]

use clap::Parser;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(name = "tachtalk-benchmark")]
#[command(about = "Benchmark ELM327 RPM request rate")]
struct Args {
    /// Server address to connect to
    #[arg(short, long, default_value = "127.0.0.1:35000")]
    address: String,

    /// Duration to run the benchmark in seconds (0 = run forever)
    #[arg(short, long, default_value = "10")]
    duration: u64,

    /// Print individual RPM values
    #[arg(short, long)]
    verbose: bool,

    /// Interval between stats printouts in seconds
    #[arg(short, long, default_value = "1")]
    interval: f64,

    /// Use "1" repeat command instead of full "010C" for subsequent requests
    #[arg(short, long)]
    repeat: bool,
}

struct Stats {
    requests: u64,
    errors: u64,
    min_latency: Duration,
    max_latency: Duration,
    total_latency: Duration,
    interval_requests: u64,
    interval_errors: u64,
    interval_start: Instant,
    last_rpm: Option<u32>,
}

impl Stats {
    fn new() -> Self {
        Self {
            requests: 0,
            errors: 0,
            min_latency: Duration::MAX,
            max_latency: Duration::ZERO,
            total_latency: Duration::ZERO,
            interval_requests: 0,
            interval_errors: 0,
            interval_start: Instant::now(),
            last_rpm: None,
        }
    }

    fn record_success(&mut self, latency: Duration, rpm: u32) {
        self.requests += 1;
        self.interval_requests += 1;
        self.total_latency += latency;
        self.min_latency = self.min_latency.min(latency);
        self.max_latency = self.max_latency.max(latency);
        self.last_rpm = Some(rpm);
    }

    fn record_error(&mut self) {
        self.errors += 1;
        self.interval_errors += 1;
    }

    fn print_interval(&mut self, verbose: bool) {
        let elapsed = self.interval_start.elapsed();
        #[allow(clippy::cast_precision_loss)] // interval_requests won't approach 2^53
        let rate = self.interval_requests as f64 / elapsed.as_secs_f64();

        if verbose {
            if let Some(rpm) = self.last_rpm {
                println!(
                    "  {:.1} req/s | {} requests | {} errors | last RPM: {}",
                    rate, self.interval_requests, self.interval_errors, rpm
                );
            } else {
                println!(
                    "  {:.1} req/s | {} requests | {} errors",
                    rate, self.interval_requests, self.interval_errors
                );
            }
        } else {
            print!(
                "\r  {:.1} req/s | {} total | {} errors",
                rate, self.requests, self.errors
            );
            std::io::stdout().flush().ok();
        }

        self.interval_requests = 0;
        self.interval_errors = 0;
        self.interval_start = Instant::now();
    }

    fn print_summary(&self, total_elapsed: Duration) {
        println!("\n\n=== Benchmark Summary ===");
        println!("Total time:     {:.2}s", total_elapsed.as_secs_f64());
        println!("Total requests: {}", self.requests);
        println!("Total errors:   {}", self.errors);

        if self.requests > 0 {
            #[allow(clippy::cast_precision_loss)] // requests won't approach 2^53
            let rate = self.requests as f64 / total_elapsed.as_secs_f64();
            let avg_latency = self.total_latency
                / u32::try_from(self.requests).expect("request count exceeded u32::MAX");

            println!("Request rate:   {rate:.1} req/s");
            println!("Min latency:    {:.3}ms", self.min_latency.as_secs_f64() * 1000.0);
            println!("Max latency:    {:.3}ms", self.max_latency.as_secs_f64() * 1000.0);
            println!("Avg latency:    {:.3}ms", avg_latency.as_secs_f64() * 1000.0);
        }
    }
}

fn read_until_prompt(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut response = Vec::new();
    let mut byte = [0u8; 1];
    
    loop {
        stream.read_exact(&mut byte)?;
        response.push(byte[0]);
        if byte[0] == b'>' {
            break;
        }
    }
    
    Ok(String::from_utf8_lossy(&response).to_string())
}

fn initialize_connection(stream: &mut TcpStream) -> std::io::Result<()> {
    // Send ATZ to reset
    stream.write_all(b"ATZ\r")?;
    read_until_prompt(stream)?;

    // Disable echo (ATE0) for faster communication
    stream.write_all(b"ATE0\r")?;
    read_until_prompt(stream)?;

    // Disable spaces (ATS0) for faster parsing
    stream.write_all(b"ATS0\r")?;
    read_until_prompt(stream)?;

    // Disable linefeeds (ATL0) for simpler parsing
    stream.write_all(b"ATL0\r")?;
    read_until_prompt(stream)?;

    Ok(())
}

fn parse_rpm_response(response: &str) -> Option<u32> {
    // Response format: "410CXXXX" where XXXX is RPM * 4 in hex
    // May have trailing \r or > characters
    let clean = response.trim().trim_end_matches('>').trim();

    if clean.len() >= 8 && clean.starts_with("410C") {
        let hex_value = &clean[4..8];
        if let Ok(value) = u32::from_str_radix(hex_value, 16) {
            return Some(value / 4);
        }
    }
    None
}

fn request_rpm(stream: &mut TcpStream, use_repeat: bool) -> std::io::Result<Option<u32>> {
    // Send RPM request (PID 0x0C)
    // Use "1" to repeat last command if enabled (saves 3 bytes per request)
    if use_repeat {
        stream.write_all(b"1\r")?;
    } else {
        stream.write_all(b"010C\r")?;
    }

    // Read until we get the prompt
    let response = read_until_prompt(stream)?;

    Ok(parse_rpm_response(&response))
}

fn run_benchmark(args: &Args) -> std::io::Result<()> {
    println!("Connecting to {}...", args.address);

    let mut stream = TcpStream::connect(&args.address)?;
    stream.set_nodelay(true)?;

    println!("Connected. Initializing ELM327...");
    initialize_connection(&mut stream)?;

    println!("Starting benchmark{}...\n",
        if args.duration > 0 {
            format!(" for {}s", args.duration)
        } else {
            " (press Ctrl+C to stop)".to_string()
        }
    );

    let mut stats = Stats::new();
    let start = Instant::now();
    let duration = if args.duration > 0 {
        Some(Duration::from_secs(args.duration))
    } else {
        None
    };
    let interval = Duration::from_secs_f64(args.interval);
    let use_repeat = args.repeat;
    let mut can_repeat = false; // Need to send full command first

    loop {
        // Check if we should stop
        if let Some(d) = duration {
            if start.elapsed() >= d {
                break;
            }
        }

        // Request RPM
        let request_start = Instant::now();
        match request_rpm(&mut stream, use_repeat && can_repeat) {
            Ok(Some(rpm)) => {
                can_repeat = true; // After first successful request, we can use repeat
                let latency = request_start.elapsed();
                stats.record_success(latency, rpm);

                if args.verbose {
                    println!("RPM: {} (latency: {:.2}ms)", rpm, latency.as_secs_f64() * 1000.0);
                }
            }
            Ok(None) => {
                stats.record_error();
                if args.verbose {
                    println!("Error: Invalid response");
                }
            }
            Err(e) => {
                eprintln!("\nConnection error: {e}");
                break;
            }
        }

        // Print interval stats
        if stats.interval_start.elapsed() >= interval {
            stats.print_interval(args.verbose);
        }
    }

    stats.print_summary(start.elapsed());
    Ok(())
}

fn main() {
    let args = Args::parse();

    if let Err(e) = run_benchmark(&args) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
