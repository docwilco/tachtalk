//! Client side of the ELM327 wire protocol.
//!
//! [`ClientSession`] owns the stream to something that speaks ELM327 (a
//! dongle, the firmware proxy, the mock server): it encodes commands, reads
//! replies up to the `>` prompt, parses multi-ECU and multi-PID replies, and
//! learns what the peer does (ECU counts, vendor PID data lengths, repeat
//! and multi-PID support). Timeouts, reconnects and the poll schedule stay
//! with the program.
//!
//! Behavior is a faithful move of the firmware dongle task's `DongleState`;
//! see the tests for the details it locks and the changes since.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use log::{debug, info, warn};
use smallvec::SmallVec;

use crate::pid::{Pid, PidDataLengths, PidGroup, MODE01_PID_DATA_LENGTHS};
use crate::{Lines, ReplyLine, WireBytes};

/// Whether the peer supports an optional feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureSupport {
    /// Not yet tested.
    Unknown,
    /// Confirmed supported.
    Yes,
    /// Confirmed unsupported.
    No,
}

/// Settings fixed for a session's life.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Ask for CAN headers (`ATH1`) during [`ClientSession::init`].
    pub framing: bool,
    /// Repeat an identical consecutive query with a bare carriage return.
    pub repeat: bool,
    /// How long to wait after `ATZ` before reading and discarding its reply.
    pub reset_wait: Duration,
    /// How long to keep retrying reads that return `WouldBlock` on a
    /// blocking stream (an lwIP quirk on ESP-IDF) before giving up.
    pub timeout: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            framing: false,
            repeat: false,
            reset_wait: Duration::from_millis(500),
            timeout: Duration::from_millis(4500),
        }
    }
}

/// What a [`ClientSession`] has set on, or learned about, the ELM327 it
/// talks to. It starts afresh with every session.
#[derive(Debug, Clone)]
pub struct ServerState {
    framing: bool,
    repeat: FeatureSupport,
    multi_pid: FeatureSupport,
    ecu_counts: HashMap<Pid, u8>,
    pid_lengths: PidDataLengths,
}

impl ServerState {
    /// Whether CAN headers are on (`ATH1` succeeded last).
    #[must_use]
    pub fn framing(&self) -> bool {
        self.framing
    }

    /// Whether a bare carriage return repeats a query.
    #[must_use]
    pub fn repeat(&self) -> FeatureSupport {
        self.repeat
    }

    /// Whether the ECU answers multi-PID queries.
    #[must_use]
    pub fn multi_pid(&self) -> FeatureSupport {
        self.multi_pid
    }

    /// How many ECUs answered `pid`, once learned.
    #[must_use]
    pub fn ecu_count(&self, pid: Pid) -> Option<u8> {
        self.ecu_counts.get(&pid).copied()
    }

    /// How many data bytes follow `pid` in a reply: from the J1979 table, or
    /// learned from a single-PID query.
    #[must_use]
    pub fn data_length(&self, pid: Pid) -> Option<u8> {
        self.pid_lengths[usize::from(pid)]
    }
}

/// Client side of the wire protocol over a stream it owns.
#[derive(Debug)]
pub struct ClientSession<S> {
    stream: S,
    config: ClientConfig,
    server: ServerState,
    /// Last query sent (with its count), the one a repeat stands for.
    last_command: Option<WireBytes>,
}

impl<S: Read + Write> ClientSession<S> {
    /// A session over `stream`; call [`init`](Self::init) before querying a
    /// dongle that was just connected.
    #[must_use]
    pub fn new(stream: S, config: ClientConfig) -> Self {
        let repeat = if config.repeat {
            FeatureSupport::Unknown
        } else {
            FeatureSupport::No
        };
        Self {
            stream,
            config,
            server: ServerState {
                framing: false,
                repeat,
                multi_pid: FeatureSupport::Unknown,
                ecu_counts: HashMap::new(),
                pid_lengths: MODE01_PID_DATA_LENGTHS,
            },
            last_command: None,
        }
    }

    /// The standard initialization: `ATZ`, wait, discard its reply, then
    /// `ATE0`, `ATL0`, `ATS0`, `ATSP0` and `ATH1` or `ATH0` per the config.
    ///
    /// # Errors
    /// Returns the stream's error from any of the commands.
    pub fn init(&mut self) -> io::Result<()> {
        // ATZ does not follow the usual echo and prompt pattern after a
        // reset, so its reply is read once and dropped.
        debug!("Sending ATZ reset");
        self.stream.write_all(b"ATZ\r")?;
        std::thread::sleep(self.config.reset_wait);
        let mut buf = [0u8; 256];
        let _ = self.stream.read(&mut buf);

        for command in [&b"ATE0"[..], b"ATL0", b"ATS0", b"ATSP0"] {
            self.command(command)?;
        }
        if self.config.framing {
            self.command(b"ATH1")?;
            info!("Framing enabled (ATH1)");
        } else {
            self.command(b"ATH0")?;
        }
        Ok(())
    }

    /// Send any command and return its raw reply, up to and including the
    /// `>` prompt. A completed `ATH1` or `ATH0` sets framing.
    ///
    /// # Errors
    /// Returns the stream's errors; a closed connection is `UnexpectedEof`.
    pub fn command(&mut self, command: &[u8]) -> io::Result<WireBytes> {
        let reply = self.round_trip(command)?;
        if command.eq_ignore_ascii_case(b"ATH1") {
            self.server.framing = true;
        } else if command.eq_ignore_ascii_case(b"ATH0") {
            self.server.framing = false;
        }
        Ok(reply)
    }

    /// Query one Mode 01 PID; `ecu_count` is appended (`010C 1`) when
    /// given. Returns the reply parsed per PID, one line per answering ECU.
    ///
    /// # Errors
    /// Returns the stream's errors; any error forgets the last command.
    pub fn query_pid(
        &mut self,
        pid: Pid,
        ecu_count: Option<u8>,
    ) -> io::Result<HashMap<Pid, Lines>> {
        let reply = self.execute(wire_command(&[pid], ecu_count))?;
        self.learn_pid_data_length(pid, &reply);
        self.learn_ecu_counts(&reply);
        Ok(self.parse_reply(&reply))
    }

    /// Query several Mode 01 PIDs in one command (`010C0D 1`). The first
    /// such query decides multi-PID support: confirmed when the reply holds
    /// at least two of the PIDs, refused otherwise or on failure.
    ///
    /// # Errors
    /// Returns the stream's errors; any error forgets the last command.
    pub fn query_pids(
        &mut self,
        pids: &[Pid],
        ecu_count: Option<u8>,
    ) -> io::Result<HashMap<Pid, Lines>> {
        let result = self.execute(wire_command(pids, ecu_count));
        let parsed = result.map(|reply| {
            self.learn_ecu_counts(&reply);
            self.parse_reply(&reply)
        });
        self.validate_multi_pid_support(parsed.as_ref().ok(), pids);
        parsed
    }

    /// What the session has set on, or learned about, its peer.
    #[must_use]
    pub fn server_state(&self) -> &ServerState {
        &self.server
    }

    /// The stream.
    #[must_use]
    pub fn get_ref(&self) -> &S {
        &self.stream
    }

    /// The stream, mutably. Reading or writing through it bypasses the
    /// session.
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.stream
    }

    /// The stream, giving up the session.
    pub fn into_inner(self) -> S {
        self.stream
    }

    /// Send a query with the repeat optimization: if it equals the last
    /// query and repeat is on or untested, send a bare carriage return
    /// instead; a reply containing `?` marks repeat unsupported and the
    /// full query is sent after all.
    fn execute(&mut self, command: WireBytes) -> io::Result<WireBytes> {
        let can_try_repeat = self.server.repeat != FeatureSupport::No
            && self.last_command.as_ref() == Some(&command);

        if !can_try_repeat {
            let result = self.round_trip(&command);
            self.last_command = result.is_ok().then_some(command);
            return result;
        }

        debug!("Trying repeat command");
        let repeat_result = self.round_trip(b"");
        match &repeat_result {
            Ok(reply) if reply.contains(&b'?') => {
                info!("Dongle does not support repeat command");
                self.server.repeat = FeatureSupport::No;
                let result = self.round_trip(&command);
                self.last_command = result.is_ok().then_some(command);
                result
            }
            Ok(_) => {
                if self.server.repeat == FeatureSupport::Unknown {
                    info!("Dongle supports repeat command");
                    self.server.repeat = FeatureSupport::Yes;
                }
                repeat_result
            }
            Err(_) => {
                self.last_command = None;
                repeat_result
            }
        }
    }

    /// Write `command` with a carriage return and read the reply up to the
    /// `>` prompt.
    fn round_trip(&mut self, command: &[u8]) -> io::Result<WireBytes> {
        let mut line = WireBytes::from(command);
        if !line.ends_with(b"\r") {
            line.push(b'\r');
        }
        debug!("Sending to dongle: {:?}", String::from_utf8_lossy(&line));
        self.stream.write_all(&line)?;
        self.read_reply()
    }

    fn read_reply(&mut self) -> io::Result<WireBytes> {
        let mut buffer = [0u8; 128];
        let mut reply = WireBytes::new();
        let start = Instant::now();

        loop {
            match self.stream.read(&mut buffer) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    reply.extend_from_slice(&buffer[..n]);
                    debug!("Read {} bytes from dongle, total: {}", n, reply.len());
                    if reply.contains(&b'>') {
                        debug!("Complete response: {:?}", String::from_utf8_lossy(&reply));
                        return Ok(reply);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    warn!("Unexpected WouldBlock on blocking socket - lwIP quirk?");
                    if start.elapsed() > self.config.timeout {
                        return Err(io::ErrorKind::TimedOut.into());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Split a reply into per-PID lines, one per answering ECU, each as a
    /// single-PID reply line (`41 0C 1A F8`). Only Mode 01 replies parse.
    fn parse_reply(&self, reply: &[u8]) -> HashMap<Pid, Lines> {
        let mut result: HashMap<Pid, Lines> = HashMap::new();
        for line in parse_lines(reply, self.server.framing) {
            let data = &line.data;
            if data.len() < 2 || data[0] != 0x41 {
                continue;
            }
            let mut i = 1;
            while i < data.len() {
                let pid = data[i];
                let end = match self.server.data_length(pid) {
                    Some(len) => (i + 1 + usize::from(len)).min(data.len()),
                    None => data.len(),
                };
                let mut single = WireBytes::new();
                single.push(0x41);
                single.extend_from_slice(&data[i..end]);
                result.entry(pid).or_default().push(ReplyLine {
                    ecu: line.ecu,
                    data: single,
                });
                i = end;
            }
        }
        result
    }

    /// Learn how many ECUs answer each PID in `reply`.
    fn learn_ecu_counts(&mut self, reply: &[u8]) {
        let lines = parse_lines(reply, self.server.framing);
        let counts = count_ecus_per_pid(&lines, self.server.framing, &self.server.pid_lengths);
        for (pid, count) in counts {
            if !self.server.ecu_counts.contains_key(&pid) && count > 0 {
                info!("Learned ECU count for PID {pid:02X}: {count}");
                self.server.ecu_counts.insert(pid, count);
            }
        }
    }

    /// Learn the data byte count for a PID missing from the table, from a
    /// single-PID reply: each ECU's line is `41 XX DD...`, and all must agree.
    fn learn_pid_data_length(&mut self, pid: Pid, reply: &[u8]) {
        if self.server.data_length(pid).is_some() {
            return;
        }

        let mut learned_len: Option<u8> = None;
        for line in parse_lines(reply, self.server.framing) {
            if line.data.len() < 2 || line.data[0] != 0x41 || line.data[1] != pid {
                continue;
            }
            // An OBD2 CAN single frame carries at most 7 bytes, so the data
            // after the mode and PID bytes always fits u8.
            let Ok(data_len) = u8::try_from(line.data.len() - 2) else {
                continue;
            };
            match learned_len {
                None => learned_len = Some(data_len),
                Some(prev) if prev != data_len => {
                    warn!(
                        "Inconsistent data length for PID {pid:02X}: {prev} vs {data_len}, \
                         not learning"
                    );
                    return;
                }
                Some(_) => {}
            }
        }

        if let Some(len) = learned_len {
            info!("Learned data length for PID {pid:02X}: {len} bytes");
            self.server.pid_lengths[usize::from(pid)] = Some(len);
        }
    }

    fn validate_multi_pid_support(&mut self, parsed: Option<&HashMap<Pid, Lines>>, pids: &[Pid]) {
        if self.server.multi_pid != FeatureSupport::Unknown {
            return;
        }
        let Some(parsed) = parsed else {
            info!("Multi-PID query failed, assuming not supported");
            self.server.multi_pid = FeatureSupport::No;
            return;
        };
        let matched = pids.iter().filter(|pid| parsed.contains_key(pid)).count();
        if matched >= 2 {
            info!("ECU supports multi-PID queries ({matched} PIDs in response)");
            self.server.multi_pid = FeatureSupport::Yes;
        } else {
            info!("ECU does not support multi-PID queries (only {matched} PIDs in response)");
            self.server.multi_pid = FeatureSupport::No;
        }
    }
}

/// `01` and the PIDs in hex, then ` {ecu_count}` when given.
fn wire_command(pids: &[Pid], ecu_count: Option<u8>) -> WireBytes {
    // At most 6 PIDs: "01" + 6×"XX" + " N" = 16 bytes, fits inline.
    let mut command = WireBytes::new();
    command.extend_from_slice(b"01");
    for &pid in pids {
        write!(command, "{pid:02X}").expect("WireBytes writes are infallible");
    }
    if let Some(count) = ecu_count {
        write!(command, " {count}").expect("WireBytes writes are infallible");
    }
    command
}

/// Parse a reply into lines of data bytes.
///
/// With framing (`ATH1`) a line is `{3-char CAN id}{PCI byte}{data...}`,
/// e.g. `7E8 03 41 0C 1A F8`; without, it is the data alone. Lines are
/// separated by bare carriage returns (`ATL0`).
fn parse_lines(reply: &[u8], framing: bool) -> SmallVec<ReplyLine, 4> {
    let text = String::from_utf8_lossy(reply);
    let mut lines = SmallVec::new();

    for line in text.split('\r') {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.contains('>') {
            continue;
        }
        let hex: SmallVec<u8, 24> = trimmed.replace(' ', "").into_bytes().into();

        if !framing {
            lines.push(ReplyLine {
                ecu: None,
                data: decode_hex(&hex),
            });
            continue;
        }
        if hex.len() < 5 {
            // Too short for the framing format: keep as is.
            lines.push(ReplyLine {
                ecu: None,
                data: hex.iter().copied().collect(),
            });
            continue;
        }

        let ecu = std::str::from_utf8(&hex[..3])
            .ok()
            .and_then(|s| u16::from_str_radix(s, 16).ok())
            .unwrap_or(0);
        // The PCI byte after the CAN id gives the data length.
        let pci = std::str::from_utf8(&hex[3..5])
            .ok()
            .and_then(|s| u8::from_str_radix(s, 16).ok())
            .unwrap_or(0);
        let data = decode_hex(&hex[5..]);
        if data.len() != usize::from(pci) {
            warn!(
                "PCI byte mismatch: PCI={pci}, actual data bytes={}, line={trimmed}",
                data.len()
            );
        }
        lines.push(ReplyLine {
            ecu: Some(ecu),
            data,
        });
    }

    lines
}

/// Decode hex pairs, skipping pairs that are not hex and an odd last digit.
fn decode_hex(hex: &[u8]) -> WireBytes {
    hex.chunks_exact(2)
        .filter_map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|s| u8::from_str_radix(s, 16).ok())
        })
        .collect()
}

/// Count ECUs per PID: by unique CAN id with framing, by line without.
fn count_ecus_per_pid(
    lines: &[ReplyLine],
    framing: bool,
    pid_lengths: &PidDataLengths,
) -> HashMap<Pid, u8> {
    let mut counts: HashMap<Pid, u8> = HashMap::new();
    if framing {
        let mut seen: HashMap<Pid, SmallVec<u16, 4>> = HashMap::new();
        for line in lines {
            if let Some(ecu) = line.ecu {
                for pid in extract_pids(&line.data, pid_lengths) {
                    let ecus = seen.entry(pid).or_default();
                    if !ecus.contains(&ecu) {
                        ecus.push(ecu);
                    }
                }
            }
        }
        for (pid, ecus) in seen {
            counts.insert(pid, u8::try_from(ecus.len()).unwrap_or(u8::MAX));
        }
    } else {
        for line in lines {
            for pid in extract_pids(&line.data, pid_lengths) {
                *counts.entry(pid).or_insert(0) += 1;
            }
        }
    }
    counts
}

/// The PIDs in a Mode 01 reply line (`41 PID DATA... PID DATA...`).
fn extract_pids(data: &[u8], pid_lengths: &PidDataLengths) -> PidGroup {
    let mut pids = PidGroup::new();
    if data.len() < 2 || data[0] != 0x41 {
        return pids;
    }
    let mut i = 1;
    while i < data.len() {
        let pid = data[i];
        pids.push(pid);
        let Some(len) = pid_lengths[usize::from(pid)] else {
            // A PID of unknown length can only be the single PID of a
            // single-PID reply, so it ends the line.
            break;
        };
        i += 1 + usize::from(len);
    }
    pids
}
