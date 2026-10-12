//! Server-side session machine for the ELM327 wire protocol.
//!
//! [`ServerSession`] is sans-io: it is fed raw bytes from the transport and
//! returns [`Action`]s for the adapter to execute. Sockets, timeouts, and
//! business rules (readiness gates, caches) stay in the adapter.
//!
//! Behavior is a faithful extraction of the firmware proxy's client loop,
//! warts included; see the characterization tests for the details it locks.

use smallvec::SmallVec;

use crate::{ClientState, WireBytes};

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

/// What a [`ServerSession`] returns to its adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Bytes to write to the client (echo).
    Send(WireBytes),
    /// An AT command handled internally, with its full response (line
    /// endings and prompt) for the adapter to write. No separate
    /// [`Action::Send`] follows it.
    At {
        /// The command as received (trimmed, original case), for logging;
        /// the firmware logs it uppercased, so its adapter must uppercase.
        command: String,
        /// Bytes to write to the client.
        response: WireBytes,
    },
    /// A non-AT command for the adapter to answer. `command` is the
    /// trimmed line; `raw` is the framed bytes as received (untrimmed,
    /// CR excluded). Run it through [`ServerSession::expand_repeat`]
    /// before forwarding.
    Obd {
        /// Trimmed command text.
        command: String,
        /// Raw command bytes as buffered (untrimmed, no CR/LF).
        raw: WireBytes,
    },
}

/// Result of expanding the invented `1` repeat command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepeatExpansion {
    /// The command to forward: unchanged, or the repeated last command.
    Command {
        /// Trimmed command text (the last command, if repeated).
        command: String,
        /// Raw bytes to forward.
        raw: WireBytes,
    },
    /// `1` with no previous command: send these bytes (a `?` response).
    NoLastCommand(WireBytes),
}

/// Sans-io server-side session machine: CR framing, echo, the AT dialect,
/// repeat expansion, response formatting, the `>` prompt.
#[derive(Debug)]
pub struct ServerSession {
    state: ClientState,
    identity: Identity,
    cmd_buffer: Vec<u8>,
}

impl ServerSession {
    /// Create a session with default client settings and the given identity.
    #[must_use]
    pub fn new(identity: Identity) -> Self {
        Self {
            state: ClientState::default(),
            identity,
            cmd_buffer: Vec::with_capacity(64),
        }
    }

    /// Feed bytes received from the client; returns the actions to execute,
    /// in order. Feeding one byte at a time yields the same byte stream and
    /// dispatches as feeding everything at once.
    ///
    /// Every line in `bytes` is processed before this returns, so an AT
    /// line late in the call has already changed the settings by the time
    /// the adapter answers an [`Action::Obd`] from earlier in the call. An
    /// adapter that answers from [`state`](Self::state) or
    /// [`format_line`](Self::format_line) feeds at most one line per call.
    #[must_use = "discarding the actions loses echo and responses"]
    pub fn feed(&mut self, bytes: &[u8]) -> SmallVec<Action, 4> {
        let mut actions = SmallVec::new();
        let mut echo = WireBytes::new();

        for &byte in bytes {
            // Echo each byte as read, before any processing, as the
            // firmware's byte-at-a-time loop does.
            if self.state.echo_enabled {
                echo.push(byte);
            }

            if byte == b'\r' {
                if !echo.is_empty() {
                    actions.push(Action::Send(std::mem::take(&mut echo)));
                }
                self.process_line(&mut actions);
                self.cmd_buffer.clear();
            } else if byte != b'\n' {
                self.cmd_buffer.push(byte);
            }
        }

        if !echo.is_empty() {
            actions.push(Action::Send(echo));
        }
        actions
    }

    fn process_line(&mut self, actions: &mut SmallVec<Action, 4>) {
        let command = String::from_utf8_lossy(&self.cmd_buffer);
        let command = command.trim();

        // Empty commands are silently ignored (firmware behavior; the
        // datasheet's bare-CR repeat is a later, deliberate change).
        if command.is_empty() {
            return;
        }

        if command.to_uppercase().starts_with("AT") {
            let response = handle_at_command(&mut self.state, &self.identity, command);
            actions.push(Action::At {
                command: command.to_string(),
                response: WireBytes::from(response.as_bytes()),
            });
        } else {
            actions.push(Action::Obd {
                command: command.to_string(),
                raw: WireBytes::from(self.cmd_buffer.as_slice()),
            });
        }
    }

    /// Expand the invented `1` repeat command against the last recorded
    /// OBD command. Call after any adapter-side gating (the firmware
    /// checks readiness before expanding), and before forwarding.
    ///
    /// Transitional: `1` is not ELM327 (see ADR 0001) and this method
    /// leaves with the datasheet bare-CR repeat spec fix (#23).
    #[must_use]
    pub fn expand_repeat(&self, command: &str, raw: &[u8]) -> RepeatExpansion {
        if command == "1" {
            match &self.state.last_obd_command {
                Some(last) => RepeatExpansion::Command {
                    command: last.clone(),
                    raw: WireBytes::from(last.as_bytes()),
                },
                None => RepeatExpansion::NoLastCommand(self.format_line("?")),
            }
        } else {
            RepeatExpansion::Command {
                command: command.to_string(),
                raw: WireBytes::from(raw),
            }
        }
    }

    /// Format a text line with the current line ending and prompt:
    /// `{le}{text}{le}>`. This is how the adapter formats rejections
    /// (`UNABLE TO CONNECT`, `?`, dongle error strings).
    #[must_use]
    pub fn format_line(&self, text: &str) -> WireBytes {
        let le = self.state.line_ending().as_bytes();
        let mut line = WireBytes::new();
        line.extend_from_slice(le);
        line.extend_from_slice(text.as_bytes());
        line.extend_from_slice(le);
        line.push(b'>');
        line
    }

    /// Format a dongle data response according to the client's space
    /// setting; see [`ClientState::format_response`].
    #[must_use]
    pub fn format_data_response(&self, response: &[u8]) -> WireBytes {
        self.state.format_response(response)
    }

    /// The session's client settings (echo, linefeeds, spaces, headers,
    /// last OBD command).
    #[must_use]
    pub fn state(&self) -> &ClientState {
        &self.state
    }

    /// Mutable client settings; the adapter records `last_obd_command`
    /// here after a successful response, as the firmware does.
    pub fn state_mut(&mut self) -> &mut ClientState {
        &mut self.state
    }
}

/// Handle an AT command against `state`, answering identity queries from
/// `identity`. Returns the full response including line endings and prompt.
///
/// This is the firmware's AT dialect, moved verbatim;
/// [`ClientState::handle_at_command`] delegates here with the default
/// (firmware) identity.
pub(crate) fn handle_at_command(
    state: &mut ClientState,
    identity: &Identity,
    command: &str,
) -> String {
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
