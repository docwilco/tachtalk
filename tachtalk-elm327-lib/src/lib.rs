//! ELM327 protocol implementation for OBD2 communication
//!
//! This library provides types and functions for implementing ELM327-compatible
//! OBD2 adapters and clients.

mod client_session;
mod pid;
mod server_session;

pub use client_session::{ClientConfig, ClientSession, FeatureSupport, ServerState};
pub use pid::{Pid, PidDataLengths, PidGroup, MODE01_PID_DATA_LENGTHS};
pub use server_session::{AtCommand, Command, Identity, ObdCommand, ServerSession};

/// Bytes the library hands to an adapter to write. Sixteen bytes inline
/// covers echo, every OBD command, and the common replies; the rare longer
/// lines (`UNABLE TO CONNECT`, the `ATDP` reply) spill to the heap.
pub type WireBytes = smallvec::SmallVec<u8, 16>;

/// One ECU's answer to an OBD command: the ECU's id when known, and the
/// data bytes (`41 0C 1A F8` for an RPM reply).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyLine {
    /// The replying ECU's CAN id (`0x7E8`), when known.
    pub ecu: Option<u16>,
    /// The reply's data bytes, starting with the mode byte.
    pub data: WireBytes,
}

/// The lines answering one PID, one per ECU; most PIDs get exactly one.
pub type Lines = smallvec::SmallVec<ReplyLine, 1>;

/// Per-connection client state (ELM327 settings)
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)] // These are independent ELM327 protocol flags
pub struct ClientState {
    /// Echo received characters back (ATE0/ATE1)
    pub echo_enabled: bool,
    /// Add linefeeds after carriage returns (ATL0/ATL1)
    pub linefeeds_enabled: bool,
    /// Print spaces between response bytes (ATS0/ATS1)
    pub spaces_enabled: bool,
    /// Show header bytes in responses (ATH0/ATH1)
    pub headers_enabled: bool,
    /// The last OBD command replied to, which a bare carriage return repeats
    pub last_obd_command: Option<String>,
}

impl Default for ClientState {
    fn default() -> Self {
        Self {
            echo_enabled: true,
            linefeeds_enabled: true,
            spaces_enabled: true,
            headers_enabled: false,
            last_obd_command: None,
        }
    }
}

impl ClientState {
    /// The line ending the client chose: CR, or CR LF with line feeds on.
    pub(crate) fn line_ending(&self) -> &'static str {
        if self.linefeeds_enabled {
            "\r\n"
        } else {
            "\r"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_state() {
        let state = ClientState::default();
        assert!(state.echo_enabled);
        assert!(state.linefeeds_enabled);
        assert!(state.spaces_enabled);
        assert!(!state.headers_enabled);
    }

    #[test]
    fn test_line_ending() {
        let mut state = ClientState::default();
        assert_eq!(state.line_ending(), "\r\n");

        state.linefeeds_enabled = false;
        assert_eq!(state.line_ending(), "\r");
    }
}
