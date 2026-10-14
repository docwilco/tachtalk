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
    /// The command as received: trimmed, original case.
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
    raw: WireBytes,
    answered: bool,
}

impl<S: Read + Write> ObdCommand<'_, S> {
    /// The command, trimmed (the repeated command for a bare carriage
    /// return).
    #[must_use]
    pub fn command(&self) -> &str {
        &self.command
    }

    /// The command bytes as received (untrimmed, no CR or LF).
    #[must_use]
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    /// The client's settings (echo, line feeds, spaces, headers).
    #[must_use]
    pub fn client_state(&self) -> &ClientState {
        &self.session.state
    }

    /// Answer with one line per ECU, formatted to the client's settings.
    /// The command becomes the one a later bare carriage return repeats.
    ///
    /// # Errors
    /// Returns the stream's error if writing fails.
    pub fn reply(mut self, lines: &[ReplyLine]) -> io::Result<()> {
        self.answered = true;
        let state = &self.session.state;
        let le = state.line_ending().as_bytes();
        let mut out: SmallVec<u8, 64> = SmallVec::new();
        for line in lines {
            out.extend_from_slice(le);
            push_hex(&mut out, &line.data, state.spaces_enabled);
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
    Obd(String, WireBytes),
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
            Line::Obd(command, raw) => Command::Obd(ObdCommand {
                session: self,
                command,
                raw,
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
        let text = String::from_utf8_lossy(&self.line);
        let command = text.trim();
        let line = if command.is_empty() {
            // The datasheet's repeat: a bare carriage return re-runs the
            // last command replied to; with none, it is answered `?`.
            if let Some(last) = &self.state.last_obd_command {
                Some(Line::Obd(last.clone(), WireBytes::from(last.as_bytes())))
            } else {
                self.line.clear();
                self.write_line("?")?;
                return Ok(None);
            }
        } else if command.to_uppercase().starts_with("AT") {
            Some(Line::At(command.to_string()))
        } else {
            Some(Line::Obd(
                command.to_string(),
                WireBytes::from(self.line.as_slice()),
            ))
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
        let response = handle_at_command(&mut self.state, &self.identity, command);
        self.stream.write_all(response.as_bytes())
    }

    /// Write `{le}{text}{le}>`.
    fn write_line(&mut self, text: &str) -> io::Result<()> {
        let le = self.state.line_ending().as_bytes();
        let mut out = WireBytes::new();
        out.extend_from_slice(le);
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(le);
        out.push(b'>');
        self.stream.write_all(&out)
    }
}

/// Append `data` as uppercase hex, a space between bytes when `spaces`.
fn push_hex(out: &mut SmallVec<u8, 64>, data: &[u8], spaces: bool) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for (i, &b) in data.iter().enumerate() {
        if spaces && i > 0 {
            out.push(b' ');
        }
        out.push(HEX[usize::from(b >> 4)]);
        out.push(HEX[usize::from(b & 0x0F)]);
    }
}

/// Handle an AT command against `state`, answering identity queries from
/// `identity`. Returns the full response including line endings and prompt.
///
/// This is the firmware's AT dialect, moved verbatim.
fn handle_at_command(state: &mut ClientState, identity: &Identity, command: &str) -> String {
    // ELM327 ignores spaces in AT commands
    let cmd: String = command
        .to_uppercase()
        .chars()
        .filter(|c| *c != ' ')
        .collect();
    let le = state.line_ending();

    // Determine response content (without line endings)
    let response_text = match cmd.as_str() {
        "ATZ" => {
            // Reset all settings to defaults
            *state = ClientState::default();
            // Use new state's line ending for response
            let le = state.line_ending();
            let ati = &identity.ati;
            return format!("{le}{ati}{le}>");
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
        "ATRV" => {
            // Return simulated battery voltage (12.6V)
            return format!("{le}12.6V{le}>");
        }
        "ATSP0" | "ATAT1" | "ATAT2" => "OK",
        "ATDP" => {
            // Describe protocol
            return format!("{le}AUTO, ISO 15765-4 (CAN 11/500){le}>");
        }
        "ATDPN" => {
            // Describe protocol number
            return format!("{le}A6{le}>");
        }
        _ if cmd.starts_with("ATSP") => "OK",
        _ if cmd.starts_with("ATST") => "OK",
        _ if cmd.starts_with("ATAT") => "OK",
        "ATI" => return format!("{le}{}{le}>", identity.ati),
        "AT@1" => return format!("{le}{}{le}>", identity.device_description),
        _ => "?",
    };

    // Build response with proper line endings (echo already sent)
    // Note: for commands that change linefeed setting, we use the OLD setting
    // since le was captured before the match
    format!("{le}{response_text}{le}>")
}
