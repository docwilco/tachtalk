//! ELM327 protocol implementation for OBD2 communication
//!
//! This library provides types and functions for implementing ELM327-compatible
//! OBD2 adapters and clients.

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
}

impl Default for ClientState {
    fn default() -> Self {
        Self {
            echo_enabled: true,
            linefeeds_enabled: true,
            spaces_enabled: true,
            headers_enabled: false,
        }
    }
}

impl ClientState {
    /// Create a new client state with default settings
    pub fn new() -> Self {
        Self::default()
    }

    /// Format a line ending based on current settings
    pub fn line_ending(&self) -> &'static str {
        if self.linefeeds_enabled {
            "\r\n"
        } else {
            "\r"
        }
    }

    /// Format a dongle response according to client settings
    /// The dongle sends compact hex (no spaces), so we add spaces if enabled
    pub fn format_response(&self, response: &[u8]) -> Vec<u8> {
        if !self.spaces_enabled {
            // No formatting needed, return as-is
            return response.to_vec();
        }

        let mut result = Vec::with_capacity(response.len() * 3 / 2);
        let mut hex_count = 0;

        for &byte in response {
            // Check if this is a hex digit
            let is_hex = byte.is_ascii_hexdigit();

            if is_hex {
                // Add space before every pair of hex digits (except the first)
                if hex_count > 0 && hex_count % 2 == 0 {
                    result.push(b' ');
                }
                hex_count += 1;
            } else {
                // Reset hex count on non-hex (line endings, prompt, etc.)
                hex_count = 0;
            }

            result.push(byte);
        }

        result
    }

    /// Handle an AT command and return the response
    /// Mutates the state if the command changes settings
    pub fn handle_at_command(&mut self, command: &str) -> String {
        let cmd = command.to_uppercase();
        let le = self.line_ending();

        // Determine response content (without line endings)
        let response_text = match cmd.as_str() {
            "ATZ" => {
                // Reset all settings to defaults
                *self = ClientState::default();
                // Use new state's line ending for response
                let le = self.line_ending();
                return format!("{le}ELM327 v1.5{le}>");
            }
            "ATE0" => {
                self.echo_enabled = false;
                "OK"
            }
            "ATE1" => {
                self.echo_enabled = true;
                "OK"
            }
            "ATL0" => {
                self.linefeeds_enabled = false;
                "OK"
            }
            "ATL1" => {
                self.linefeeds_enabled = true;
                "OK"
            }
            "ATS0" => {
                self.spaces_enabled = false;
                "OK"
            }
            "ATS1" => {
                self.spaces_enabled = true;
                "OK"
            }
            "ATH0" => {
                self.headers_enabled = false;
                "OK"
            }
            "ATH1" => {
                self.headers_enabled = true;
                "OK"
            }
            "ATSP0" | "ATAT1" | "ATAT2" => "OK",
            _ if cmd.starts_with("ATSP") => "OK",
            _ if cmd.starts_with("ATST") => "OK",
            _ if cmd.starts_with("ATAT") => "OK",
            "ATI" => "ELM327 v1.5",
            "AT@1" => return self.device_description(),
            _ => "?",
        };

        // Build response with proper line endings (echo already sent)
        // Note: for commands that change linefeed setting, we use the OLD setting
        // since le was captured before the match
        format!("{le}{response_text}{le}>")
    }

    /// Override this to provide a custom device description for AT@1
    /// Default implementation returns generic ELM327
    pub fn device_description(&self) -> String {
        let le = self.line_ending();
        format!("{le}ELM327{le}>")
    }
}

/// Extract RPM from an OBD2 response
///
/// OBD2 response format for RPM (PID 0C): "41 0C XX XX" or "410CXX XX"
/// RPM = ((A * 256) + B) / 4
pub fn extract_rpm_from_response(data: &[u8]) -> Option<u32> {
    let text = std::str::from_utf8(data).ok()?;

    // Look for "41 0C" or "410C" pattern
    let text_upper = text.to_uppercase();
    if let Some(pos) = text_upper.find("410C") {
        let after = &text_upper[pos + 4..];
        // Try to parse hex bytes (with or without spaces)
        let hex_chars: String = after.chars().filter(char::is_ascii_hexdigit).collect();

        if hex_chars.len() >= 4 {
            let a = u32::from_str_radix(&hex_chars[0..2], 16).ok()?;
            let b = u32::from_str_radix(&hex_chars[2..4], 16).ok()?;
            let rpm = ((a * 256) + b) / 4;
            return Some(rpm);
        }
    }

    None
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

    #[test]
    fn test_at_commands() {
        let mut state = ClientState::default();
        
        // Test echo off
        let resp = state.handle_at_command("ATE0");
        assert!(resp.contains("OK"));
        assert!(!state.echo_enabled);
        
        // Test spaces off
        let resp = state.handle_at_command("ATS0");
        assert!(resp.contains("OK"));
        assert!(!state.spaces_enabled);
        
        // Test reset
        let resp = state.handle_at_command("ATZ");
        assert!(resp.contains("ELM327"));
        assert!(state.echo_enabled);
        assert!(state.spaces_enabled);
    }

    #[test]
    fn test_extract_rpm() {
        // Test with spaces
        let data = b"41 0C 1A F8\r\r>";
        assert_eq!(extract_rpm_from_response(data), Some(1726));
        
        // Test without spaces
        let data = b"410C1AF8\r\r>";
        assert_eq!(extract_rpm_from_response(data), Some(1726));
        
        // Test no RPM data
        let data = b"41 0D 28\r\r>";
        assert_eq!(extract_rpm_from_response(data), None);
    }

    #[test]
    fn test_format_response_with_spaces() {
        let state = ClientState::default();
        let input = b"410C1AF8\r\r>";
        let output = state.format_response(input);
        assert_eq!(&output, b"41 0C 1A F8\r\r>");
    }

    #[test]
    fn test_format_response_without_spaces() {
        let mut state = ClientState::default();
        state.spaces_enabled = false;
        let input = b"410C1AF8\r\r>";
        let output = state.format_response(input);
        assert_eq!(&output, b"410C1AF8\r\r>");
    }
}
