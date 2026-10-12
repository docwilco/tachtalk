//! Characterization tests for `ServerSession`.
//!
//! Expected bytes are transcribed from the firmware proxy's client loop
//! (tachtalk-firmware/src/obd2.rs, `handle_client` + `process_command` as of
//! bcaa695) and lock its behavior byte-for-byte, warts included: echo per
//! byte read, CR framing with LF dropped, empty commands silently ignored,
//! AT responses formatted with the line ending from before the command
//! took effect, and the invented `1` repeat.

use tachtalk_elm327_lib::{Action, Identity, RepeatExpansion, ServerSession, WireBytes};

/// Concatenate every `Send` payload and every `At` response, in order: the
/// byte stream a client would receive.
fn sent_bytes(actions: &[Action]) -> Vec<u8> {
    let mut out = Vec::new();
    for action in actions {
        match action {
            Action::Send(bytes)
            | Action::At {
                response: bytes, ..
            } => {
                out.extend_from_slice(bytes);
            }
            Action::Obd { .. } => {}
        }
    }
    out
}

/// All non-`Send` actions, cloned, for comparing dispatch sequences.
fn dispatches(actions: &[Action]) -> Vec<Action> {
    actions
        .iter()
        .filter(|a| !matches!(a, Action::Send(_)))
        .cloned()
        .collect()
}

/// Bytes as the library hands them out.
fn wire(bytes: &[u8]) -> WireBytes {
    WireBytes::from(bytes)
}

/// An `At` action with the given command and response.
fn at(command: &str, response: &[u8]) -> Action {
    Action::At {
        command: command.to_string(),
        response: wire(response),
    }
}

fn firmware_session() -> ServerSession {
    ServerSession::new(Identity::default())
}

#[test]
fn atz_echoes_and_prints_banner() {
    let mut session = firmware_session();
    let actions = session.feed(b"ATZ\r");

    // Echo is on by default: every byte comes back, then the banner.
    assert_eq!(sent_bytes(&actions), b"ATZ\r\r\nELM327 v1.5\r\n>");
    assert_eq!(
        dispatches(&actions),
        vec![at("ATZ", b"\r\nELM327 v1.5\r\n>")]
    );
}

#[test]
fn at_line_is_echo_then_at_carrying_its_response() {
    let mut session = firmware_session();
    let actions = session.feed(b"ATE0\r");
    // No separate response `Send` follows the `At`: the response rides
    // inside it.
    assert_eq!(
        actions[..],
        [Action::Send(wire(b"ATE0\r")), at("ATE0", b"\r\nOK\r\n>")]
    );
}

#[test]
fn ate0_disables_echo_from_the_next_byte() {
    let mut session = firmware_session();
    let actions = session.feed(b"ATE0\r");
    // The ATE0 command itself is still echoed (firmware echoes each byte
    // before processing the CR).
    assert_eq!(sent_bytes(&actions), b"ATE0\r\r\nOK\r\n>");

    let actions = session.feed(b"ATI\r");
    // No echo now, just the response.
    assert_eq!(sent_bytes(&actions), b"\r\nELM327 v1.5\r\n>");
}

#[test]
fn echo_stops_mid_feed_when_ate0_lands() {
    let mut session = firmware_session();
    let actions = session.feed(b"ATE0\rAB");
    // "ATE0\r" echoed, "AB" read after echo turned off.
    assert_eq!(sent_bytes(&actions), b"ATE0\r\r\nOK\r\n>");
}

#[test]
fn linefeeds_are_echoed_but_not_buffered() {
    let mut session = firmware_session();
    let actions = session.feed(b"ATI\r\n");
    // The trailing LF is echoed after the response, as the firmware's
    // byte-at-a-time loop does.
    assert_eq!(sent_bytes(&actions), b"ATI\r\r\nELM327 v1.5\r\n>\n");
    // And an LF inside a command does not split or pollute it.
    let actions = session.feed(b"AT\nI\r");
    assert_eq!(
        dispatches(&actions),
        vec![at("ATI", b"\r\nELM327 v1.5\r\n>")]
    );
}

#[test]
fn empty_and_whitespace_commands_are_silently_ignored() {
    let mut session = firmware_session();
    let actions = session.feed(b"\r");
    assert_eq!(sent_bytes(&actions), b"\r"); // echo only
    assert_eq!(dispatches(&actions), vec![]);

    let actions = session.feed(b"   \r");
    assert_eq!(sent_bytes(&actions), b"   \r");
    assert_eq!(dispatches(&actions), vec![]);
}

#[test]
fn at_commands_are_case_insensitive_and_ignore_spaces() {
    let mut session = firmware_session();
    let actions = session.feed(b"ate0\r");
    assert_eq!(sent_bytes(&actions), b"ate0\r\r\nOK\r\n>");
    assert!(!session.state().echo_enabled);

    let actions = session.feed(b"AT H1\r");
    assert_eq!(sent_bytes(&actions), b"\r\nOK\r\n>");
    assert!(session.state().headers_enabled);
    // The At action carries the command as received (trimmed, original case).
    assert_eq!(dispatches(&actions), vec![at("AT H1", b"\r\nOK\r\n>")]);
}

#[test]
fn unknown_at_command_gets_question_mark() {
    let mut session = firmware_session();
    let actions = session.feed(b"ATXX\r");
    assert_eq!(sent_bytes(&actions), b"ATXX\r\r\n?\r\n>");
}

#[test]
fn atz_resets_settings_and_uses_the_new_line_ending() {
    let mut session = firmware_session();
    let _ = session.feed(b"ATE0\r");
    let _ = session.feed(b"ATL0\r");
    session.state_mut().last_obd_command = Some("010C".to_string());

    let actions = session.feed(b"ATZ\r");
    // Reset re-enables linefeeds before the banner is formatted, so the
    // banner uses \r\n even though ATL0 was in effect (no echo: still off
    // while the bytes were read).
    assert_eq!(sent_bytes(&actions), b"\r\nELM327 v1.5\r\n>");
    assert!(session.state().echo_enabled);
    assert!(session.state().linefeeds_enabled);
    assert_eq!(session.state().last_obd_command, None);
}

#[test]
fn identity_strings_come_from_the_constructor() {
    let identity = Identity {
        ati: "MOCK ELM327 v9.9".to_string(),
        device_description: "tachtalk mock".to_string(),
    };
    let mut session = ServerSession::new(identity);
    let _ = session.feed(b"ATE0\r");

    let actions = session.feed(b"ATI\r");
    assert_eq!(sent_bytes(&actions), b"\r\nMOCK ELM327 v9.9\r\n>");
    let actions = session.feed(b"AT@1\r");
    assert_eq!(sent_bytes(&actions), b"\r\ntachtalk mock\r\n>");
    // Echo is still off while ATZ's bytes are read; the reset banner uses
    // the constructor's ATI string.
    let actions = session.feed(b"ATZ\r");
    assert_eq!(sent_bytes(&actions), b"\r\nMOCK ELM327 v9.9\r\n>");
}

#[test]
fn non_at_command_is_dispatched_trimmed_with_raw_bytes() {
    let mut session = firmware_session();
    let _ = session.feed(b"ATE0\r");
    let actions = session.feed(b" 010c 1 \r");
    // Trimmed text for dispatch, untrimmed raw bytes (CR excluded), as the
    // firmware passes both to process_command.
    assert_eq!(
        dispatches(&actions),
        vec![Action::Obd {
            command: "010c 1".to_string(),
            raw: wire(b" 010c 1 "),
        }]
    );
    // The session sends nothing itself for an OBD command.
    assert_eq!(sent_bytes(&actions), b"");
}

#[test]
fn commands_split_across_feeds_are_reassembled() {
    let mut session = firmware_session();
    let _ = session.feed(b"ATE0\r");
    assert_eq!(dispatches(&session.feed(b"01")), vec![]);
    assert_eq!(
        dispatches(&session.feed(b"0C\r")),
        vec![Action::Obd {
            command: "010C".to_string(),
            raw: wire(b"010C"),
        }]
    );
}

#[test]
fn repeat_expands_to_the_last_recorded_command() {
    let mut session = firmware_session();
    session.state_mut().last_obd_command = Some("010C".to_string());
    assert_eq!(
        session.expand_repeat("1", b"1"),
        RepeatExpansion::Command {
            command: "010C".to_string(),
            raw: wire(b"010C"),
        }
    );
}

#[test]
fn repeat_without_history_formats_a_question_mark() {
    let session = firmware_session();
    assert_eq!(
        session.expand_repeat("1", b"1"),
        RepeatExpansion::NoLastCommand(wire(b"\r\n?\r\n>"))
    );
}

#[test]
fn non_repeat_commands_pass_through_expansion_unchanged() {
    let session = firmware_session();
    assert_eq!(
        session.expand_repeat("010D", b" 010D"),
        RepeatExpansion::Command {
            command: "010D".to_string(),
            raw: wire(b" 010D"),
        }
    );
}

#[test]
fn format_line_uses_the_current_line_ending() {
    let mut session = firmware_session();
    assert_eq!(
        session.format_line("UNABLE TO CONNECT"),
        b"\r\nUNABLE TO CONNECT\r\n>"
    );
    let _ = session.feed(b"ATL0\r");
    assert_eq!(session.format_line("?"), b"\r?\r>");
}

#[test]
fn format_data_response_spaces_hex_pairs() {
    let mut session = firmware_session();
    assert_eq!(
        session.format_data_response(b"410C1AF8\r\r>"),
        b"41 0C 1A F8\r\r>"
    );
    let _ = session.feed(b"ATS0\r");
    assert_eq!(
        session.format_data_response(b"410C1AF8\r\r>"),
        b"410C1AF8\r\r>"
    );
}

#[test]
fn torn_packet_feed_matches_whole_feed() {
    let script: &[u8] = b"ATZ\rATE0\r010C\rAT H1\r\r  \rATL0\ratdp\r1\r\n010D\r";

    let mut whole = firmware_session();
    let whole_actions = whole.feed(script);

    let mut torn = firmware_session();
    let mut torn_actions = Vec::new();
    for &byte in script {
        torn_actions.extend(torn.feed(&[byte]));
    }

    assert_eq!(sent_bytes(&whole_actions), sent_bytes(&torn_actions));
    assert_eq!(dispatches(&whole_actions), dispatches(&torn_actions));
    assert_eq!(whole.state().echo_enabled, torn.state().echo_enabled);
    assert_eq!(
        whole.state().linefeeds_enabled,
        torn.state().linefeeds_enabled
    );
    // And the dispatch sequence is what the firmware loop would produce.
    assert_eq!(
        dispatches(&whole_actions),
        vec![
            at("ATZ", b"\r\nELM327 v1.5\r\n>"),
            at("ATE0", b"\r\nOK\r\n>"),
            Action::Obd {
                command: "010C".to_string(),
                raw: wire(b"010C"),
            },
            at("AT H1", b"\r\nOK\r\n>"),
            at("ATL0", b"\r\nOK\r\n>"),
            at("atdp", b"\rAUTO, ISO 15765-4 (CAN 11/500)\r>"),
            Action::Obd {
                command: "1".to_string(),
                raw: wire(b"1"),
            },
            Action::Obd {
                command: "010D".to_string(),
                raw: wire(b"010D"),
            },
        ]
    );
}
