//! Server side of the ELM327 wire protocol.
//!
//! [`ServerSession`] owns the stream to a client that speaks to us as if we
//! were an ELM327. It reads, echoes, frames commands at the carriage return,
//! and answers AT commands; the program answers OBD commands through the
//! [`ObdCommand`] it is handed. Timeouts, readiness and where answers come
//! from stay with the program.
//!
//! Behavior is a faithful move of the firmware proxy's client loop, warts
//! included; see the characterization tests for the details it locks.

use std::io::{self, Read, Write};

use smallvec::SmallVec;

use crate::{ClientState, ReplyLine, WireBytes};

/// Identity strings a server presents to clients.
#[derive(Debug, Clone)]
pub struct Identity {
    /// Response body for `ATI`, also the `ATZ` reset banner.
    pub ati: String,
    /// Response body for `AT@1` (device description).
    pub device_description: String,
}

impl Default for Identity {
    /// The firmware proxy's identity strings.
    fn default() -> Self {
        Self {
            ati: "ELM327 v1.5".to_string(),
            device_description: "ELM327".to_string(),
        }
    }
}

/// A command read from the client, to be answered before the next one is
/// read. It borrows the session; answering consumes it.
#[derive(Debug)]
pub enum Command<'a, S: Read + Write> {
    /// An AT command; the session knows the answer.
    At(AtCommand<'a, S>),
    /// Anything else; the program answers it.
    Obd(ObdCommand<'a, S>),
}

/// An AT command read from the client. Dropping it unanswered writes the
/// answer anyway; a write error then is returned by the next
/// [`ServerSession::next_command`].
#[derive(Debug)]
pub struct AtCommand<'a, S: Read + Write> {
    session: &'a mut ServerSession<S>,
    command: String,
    answered: bool,
}

impl<S: Read + Write> AtCommand<'_, S> {
    /// The command, canonical: uppercase, spaces removed.
    #[must_use]
    pub fn command(&self) -> &str {
        &self.command
    }

    /// Apply the command to the session's settings and write its answer.
    ///
    /// # Errors
    /// Returns the stream's error if writing fails.
    pub fn answer(mut self) -> io::Result<()> {
        self.answered = true;
        self.session.answer_at(&self.command)
    }
}

impl<S: Read + Write> Drop for AtCommand<'_, S> {
    fn drop(&mut self) {
        if self.answered || std::thread::panicking() {
            return;
        }
        if let Err(e) = self.session.answer_at(&self.command) {
            self.session.pending_error = Some(e);
        }
    }
}

/// A non-AT command read from the client, for the program to answer with
/// [`reply`](Self::reply) or [`reject`](Self::reject).
///
/// # Panics
/// Dropping it unanswered panics: every command gets exactly one reply, and
/// a program that forgets one has a bug.
#[derive(Debug)]
pub struct ObdCommand<'a, S: Read + Write> {
    session: &'a mut ServerSession<S>,
    command: String,
    answered: bool,
}

impl<S: Read + Write> ObdCommand<'_, S> {
    /// The command, canonical: uppercase, spaces removed (the repeated
    /// command for a bare carriage return).
    #[must_use]
    pub fn command(&self) -> &str {
        &self.command
    }

    /// The client's settings (echo, line feeds, spaces, headers).
    #[must_use]
    pub fn client_state(&self) -> &ClientState {
        &self.session.state
    }

    /// Answer with one line per ECU, formatted to the client's settings;
    /// no lines at all is `NO DATA`. The command becomes the one a later
    /// bare carriage return repeats.
    ///
    /// # Errors
    /// Returns the stream's error if writing fails.
    pub fn reply(mut self, lines: &[ReplyLine]) -> io::Result<()> {
        self.answered = true;
        if lines.is_empty() {
            // No ECU answered: what a real ELM327 says then.
            return self.session.write_line("NO DATA");
        }
        let state = &self.session.state;
        let le = state.line_ending().as_bytes();
        let mut out: SmallVec<u8, 64> = SmallVec::new();
        for line in lines {
            if state.headers_enabled {
                push_header(&mut out, line, state.spaces_enabled);
            }
            push_hex(&mut out, &line.data, state.spaces_enabled);
            out.extend_from_slice(le);
        }
        out.extend_from_slice(le);
        out.push(b'>');
        self.session.stream.write_all(&out)?;
        self.session.state.last_obd_command = Some(std::mem::take(&mut self.command));
        Ok(())
    }

    /// Answer with a rejection line such as `?`, `NO DATA` or
    /// `UNABLE TO CONNECT`.
    ///
    /// # Errors
    /// Returns the stream's error if writing fails.
    pub fn reject(mut self, text: &str) -> io::Result<()> {
        self.answered = true;
        self.session.write_line(text)
    }
}

impl<S: Read + Write> Drop for ObdCommand<'_, S> {
    fn drop(&mut self) {
        assert!(
            self.answered || std::thread::panicking(),
            "OBD command {:?} dropped unanswered",
            self.command
        );
    }
}

/// A complete line read from the client, before it is handed out.
enum Line {
    At(String),
    Obd(String),
}

/// Server side of the wire protocol over a stream it owns: CR framing,
/// echo, the AT dialect, the bare-CR repeat, reply formatting, the `>`
/// prompt.
#[derive(Debug)]
pub struct ServerSession<S> {
    stream: S,
    state: ClientState,
    identity: Identity,
    /// The command being read, up to its carriage return.
    line: Vec<u8>,
    /// Bytes read but not yet processed, `buf[pos..len]`.
    buf: [u8; 64],
    pos: usize,
    len: usize,
    /// Echo of processed bytes, not yet written.
    echo: WireBytes,
    /// A write error from an AT command answered on drop.
    pending_error: Option<io::Error>,
}

impl<S: Read + Write> ServerSession<S> {
    /// A session over `stream` with default client settings.
    #[must_use]
    pub fn new(stream: S, identity: Identity) -> Self {
        Self {
            stream,
            state: ClientState::default(),
            identity,
            line: Vec::with_capacity(64),
            buf: [0; 64],
            pos: 0,
            len: 0,
            echo: WireBytes::new(),
            pending_error: None,
        }
    }

    /// Read until the next command, echoing as the client's settings say,
    /// and hand it out. An empty line repeats the last command replied to,
    /// or is answered `?` here when there is none.
    ///
    /// # Errors
    /// Returns the stream's errors: a read timeout (`TimedOut` or
    /// `WouldBlock`) leaves a partly read command in place for the next
    /// call; a closed connection is `UnexpectedEof`. A write error from an
    /// AT command answered on drop is returned first.
    pub fn next_command(&mut self) -> io::Result<Command<'_, S>> {
        if let Some(e) = self.pending_error.take() {
            return Err(e);
        }
        let line = loop {
            if self.pos == self.len {
                self.write_echo()?;
                let n = self.stream.read(&mut self.buf)?;
                if n == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                self.pos = 0;
                self.len = n;
            }
            let byte = self.buf[self.pos];
            self.pos += 1;
            // Echo each byte as it is processed, with the settings in force
            // at that point, as the firmware's byte-at-a-time loop did.
            if self.state.echo_enabled {
                self.echo.push(byte);
            }
            if byte == b'\r' {
                self.write_echo()?;
                let line = self.take_line()?;
                if let Some(line) = line {
                    break line;
                }
            } else if byte != b'\n' {
                self.line.push(byte);
            }
        };
        Ok(match line {
            Line::At(command) => Command::At(AtCommand {
                session: self,
                command,
                answered: false,
            }),
            Line::Obd(command) => Command::Obd(ObdCommand {
                session: self,
                command,
                answered: false,
            }),
        })
    }

    /// The client's settings (echo, line feeds, spaces, headers, the last
    /// replied OBD command).
    #[must_use]
    pub fn client_state(&self) -> &ClientState {
        &self.state
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

    /// Turn the buffered line into a command, or `None` for a line the
    /// session handles itself.
    fn take_line(&mut self) -> io::Result<Option<Line>> {
        // The ELM327 ignores case and spaces: hand commands out canonical.
        let command: String = String::from_utf8_lossy(&self.line)
            .chars()
            .filter(|c| !c.is_whitespace())
            .map(|c| c.to_ascii_uppercase())
            .collect();
        let line = if command.is_empty() {
            // The datasheet's repeat: a bare carriage return re-runs the
            // last command replied to; with none, it is answered `?`.
            if let Some(last) = &self.state.last_obd_command {
                Some(Line::Obd(last.clone()))
            } else {
                self.line.clear();
                self.write_line("?")?;
                return Ok(None);
            }
        } else if command.starts_with("AT") {
            Some(Line::At(command))
        } else {
            Some(Line::Obd(command))
        };
        self.line.clear();
        Ok(line)
    }

    fn write_echo(&mut self) -> io::Result<()> {
        if self.echo.is_empty() {
            return Ok(());
        }
        let result = self.stream.write_all(&self.echo);
        self.echo.clear();
        result
    }

    fn answer_at(&mut self, command: &str) -> io::Result<()> {
        let answer = handle_at_command(&mut self.state, &self.identity, command);
        // The line ending in force now: ATL0 and ATL1 answer with the one
        // they just chose.
        let le = self.state.line_ending();
        write_reply_line(&mut self.stream, le, answer.reset, answer.text)
    }

    fn write_line(&mut self, text: &str) -> io::Result<()> {
        write_reply_line(&mut self.stream, self.state.line_ending(), false, text)
    }
}

/// Write `{text}{le}{le}>`, a real ELM327's shape for a one-line reply,
/// after two blank lines when `blank_lines_first` (the reset banner).
fn write_reply_line(
    stream: &mut impl Write,
    le: &str,
    blank_lines_first: bool,
    text: &str,
) -> io::Result<()> {
    let le = le.as_bytes();
    let mut out = WireBytes::new();
    if blank_lines_first {
        out.extend_from_slice(le);
        out.extend_from_slice(le);
    }
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(le);
    out.extend_from_slice(le);
    out.push(b'>');
    stream.write_all(&out)
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// The CAN id a line is headed with when its ECU is unknown: 7E8, the
/// engine ECU's usual reply id.
const DEFAULT_ECU: u16 = 0x7E8;

/// Append a line's `ATH1` header: the 3-digit CAN id and the data length,
/// each followed by a space when `spaces`.
fn push_header(out: &mut SmallVec<u8, 64>, line: &ReplyLine, spaces: bool) {
    let ecu = line.ecu.unwrap_or(DEFAULT_ECU);
    for shift in [8, 4, 0] {
        out.push(HEX[usize::from((ecu >> shift) & 0x0F)]);
    }
    if spaces {
        out.push(b' ');
    }
    // A CAN single frame carries at most 7 data bytes.
    let len = u8::try_from(line.data.len()).unwrap_or(u8::MAX);
    push_hex(out, &[len], false);
    if spaces {
        out.push(b' ');
    }
}

/// Append `data` as uppercase hex, a space between bytes when `spaces`.
fn push_hex(out: &mut SmallVec<u8, 64>, data: &[u8], spaces: bool) {
    for (i, &b) in data.iter().enumerate() {
        if spaces && i > 0 {
            out.push(b' ');
        }
        out.push(HEX[usize::from(b >> 4)]);
        out.push(HEX[usize::from(b & 0x0F)]);
    }
}

/// An AT command's answer, before line endings and the prompt.
struct AtAnswer<'a> {
    text: &'a str,
    /// The command was `ATZ`: a real ELM327 prints blank lines before its
    /// reset banner.
    reset: bool,
}

/// Apply a canonical AT command to `state` and return its answer, taking
/// identity strings from `identity`. This is the firmware's AT dialect.
fn handle_at_command<'a>(
    state: &mut ClientState,
    identity: &'a Identity,
    command: &str,
) -> AtAnswer<'a> {
    let mut reset = false;
    let text = match command {
        "ATZ" => {
            // Reset all settings to defaults
            *state = ClientState::default();
            reset = true;
            identity.ati.as_str()
        }
        "ATE0" => {
            state.echo_enabled = false;
            "OK"
        }
        "ATE1" => {
            state.echo_enabled = true;
            "OK"
        }
        "ATL0" => {
            state.linefeeds_enabled = false;
            "OK"
        }
        "ATL1" => {
            state.linefeeds_enabled = true;
            "OK"
        }
        "ATS0" => {
            state.spaces_enabled = false;
            "OK"
        }
        "ATS1" => {
            state.spaces_enabled = true;
            "OK"
        }
        "ATH0" => {
            state.headers_enabled = false;
            "OK"
        }
        "ATH1" => {
            state.headers_enabled = true;
            "OK"
        }
        // Simulated battery voltage
        "ATRV" => "12.6V",
        "ATSP0" | "ATAT1" | "ATAT2" => "OK",
        // Describe protocol, and its number
        "ATDP" => "AUTO, ISO 15765-4 (CAN 11/500)",
        "ATDPN" => "A6",
        _ if command.starts_with("ATSP") => "OK",
        _ if command.starts_with("ATST") => "OK",
        _ if command.starts_with("ATAT") => "OK",
        "ATI" => identity.ati.as_str(),
        "AT@1" => identity.device_description.as_str(),
        _ => "?",
    };
    AtAnswer { text, reset }
}
