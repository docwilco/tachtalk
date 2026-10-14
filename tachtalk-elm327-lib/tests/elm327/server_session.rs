//! Characterization tests for `ServerSession`.
//!
//! Expected bytes are transcribed from the firmware proxy's client loop
//! (tachtalk-firmware/src/obd2.rs, `handle_client` + `process_command` as of
//! bcaa695) and lock its behavior byte-for-byte, warts included: echo per
//! byte read, CR framing with LF dropped, empty commands silently ignored,
//! and AT responses formatted with the line ending from before the command
//! took effect. Deliberate changes since are noted at their tests: the
//! datasheet's bare-CR repeat replaced the invented `1`, and every reply
//! takes a real ELM327's shape, `{line}{le}{le}>`, as the captures in
//! `fixtures/` show it (was the firmware's `{le}{line}{le}>`).

use std::io::ErrorKind;

use tachtalk_elm327_lib::{Command, Identity, ObdCommand, ReplyLine, ServerSession};

use crate::support::{FakeStream, Step};

/// How a test answers each OBD command.
enum Answer {
    Reply(Vec<ReplyLine>),
    Reject(&'static str),
}

/// The commands a session handed out, as `AT ATZ` or `OBD 010C | raw`.
type Seen = Vec<String>;

/// Serve the whole stream, answering AT commands and every OBD command with
/// `answer`, until end of input.
fn serve(
    session: &mut ServerSession<FakeStream>,
    mut answer: impl FnMut(&ObdCommand<'_, FakeStream>) -> Answer,
) -> Seen {
    let mut seen = Vec::new();
    loop {
        match session.next_command() {
            Ok(Command::At(at)) => {
                seen.push(format!("AT {}", at.command()));
                at.answer().unwrap();
            }
            Ok(Command::Obd(obd)) => {
                seen.push(format!(
                    "OBD {} | {}",
                    obd.command(),
                    String::from_utf8_lossy(obd.raw())
                ));
                match answer(&obd) {
                    Answer::Reply(lines) => obd.reply(&lines).unwrap(),
                    Answer::Reject(text) => obd.reject(text).unwrap(),
                }
            }
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return seen,
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
}

/// Serve `input` with the firmware identity, answering OBD commands `?`;
/// returns the bytes written and the commands seen.
fn serve_input(input: &[u8]) -> (String, Seen) {
    let mut session = firmware_session(FakeStream::new(input));
    let seen = serve(&mut session, |_| Answer::Reject("?"));
    (session.get_ref().written_text(), seen)
}

fn firmware_session(stream: FakeStream) -> ServerSession<FakeStream> {
    ServerSession::new(stream, Identity::default())
}

/// One ECU's answer for `pid`.
fn line(data: &[u8]) -> ReplyLine {
    ReplyLine {
        ecu: None,
        data: data.into(),
    }
}

#[test]
fn atz_echoes_and_prints_banner() {
    let (written, seen) = serve_input(b"ATZ\r");
    // Echo is on by default: every byte comes back, then the banner.
    assert_eq!(written, "ATZ\\r\\r\\n\\r\\nELM327 v1.5\\r\\n\\r\\n>");
    assert_eq!(seen, ["AT ATZ"]);
}

#[test]
fn ate0_disables_echo_from_the_next_byte() {
    // The ATE0 command itself is still echoed (the CR is read before the
    // command is answered); ATI after it is not.
    let (written, _) = serve_input(b"ATE0\rATI\r");
    assert_eq!(written, "ATE0\\rOK\\r\\n\\r\\n>ELM327 v1.5\\r\\n\\r\\n>");
}

#[test]
fn echo_is_written_before_the_answer() {
    let mut session = firmware_session(FakeStream::script(vec![
        Step::Read(b"ATI\r".to_vec()),
        Step::Expect(b"ATI\r".to_vec()),
        Step::Expect(b"ELM327 v1.5\r\n\r\n>".to_vec()),
    ]));
    serve(&mut session, |_| Answer::Reject("?"));
}

#[test]
fn echo_stops_mid_read_when_ate0_lands() {
    // "ATE0\r" echoed, "AB" read after echo turned off, all in one read.
    let (written, _) = serve_input(b"ATE0\rAB");
    assert_eq!(written, "ATE0\\rOK\\r\\n\\r\\n>");
}

#[test]
fn linefeeds_are_echoed_but_not_buffered() {
    // The trailing LF is echoed after the response, as the firmware's
    // byte-at-a-time loop does.
    let (written, _) = serve_input(b"ATI\r\n");
    assert_eq!(written, "ATI\\rELM327 v1.5\\r\\n\\r\\n>\\n");
    // And an LF inside a command does not split or pollute it.
    let (_, seen) = serve_input(b"AT\nI\r");
    assert_eq!(seen, ["AT ATI"]);
}

#[test]
fn an_empty_line_with_nothing_to_repeat_answers_a_question_mark() {
    // Spaces are ignored, so a line of spaces is an empty line too.
    let (written, seen) = serve_input(b"\r   \r");
    assert_eq!(written, "\\r?\\r\\n\\r\\n>   \\r?\\r\\n\\r\\n>");
    assert!(seen.is_empty());
}

#[test]
fn at_commands_are_case_insensitive_and_ignore_spaces() {
    let mut session = firmware_session(FakeStream::new(b"ate0\rAT H1\r"));
    let seen = serve(&mut session, |_| Answer::Reject("?"));
    assert_eq!(
        session.get_ref().written_text(),
        "ate0\\rOK\\r\\n\\r\\n>OK\\r\\n\\r\\n>"
    );
    assert!(!session.client_state().echo_enabled);
    assert!(session.client_state().headers_enabled);
    // The command is handed out as received (trimmed, original case).
    assert_eq!(seen, ["AT ate0", "AT AT H1"]);
}

#[test]
fn unknown_at_command_gets_question_mark() {
    let (written, _) = serve_input(b"ATXX\r");
    assert_eq!(written, "ATXX\\r?\\r\\n\\r\\n>");
}

#[test]
fn atz_resets_settings_and_uses_the_new_line_ending() {
    let mut session = firmware_session(FakeStream::new(b"ATE0\rATL0\r010C\rATZ\r"));
    serve(&mut session, |_| {
        Answer::Reply(vec![line(&[0x41, 0x0C, 0x1A, 0xF8])])
    });
    // ATZ is answered with the defaults it just restored (CRLF), not ATL0's CR.
    assert!(session
        .get_ref()
        .written_text()
        .ends_with("\\r\\n\\r\\nELM327 v1.5\\r\\n\\r\\n>"));
    let state = session.client_state();
    assert!(state.echo_enabled);
    assert!(state.linefeeds_enabled);
    assert_eq!(state.last_obd_command, None);
}

#[test]
fn identity_strings_come_from_the_constructor() {
    let identity = Identity {
        ati: "MOCK ELM327 v9.9".to_string(),
        device_description: "tachtalk mock".to_string(),
    };
    let mut session = ServerSession::new(FakeStream::new(b"ATE0\rATI\rAT@1\rATZ\r"), identity);
    serve(&mut session, |_| Answer::Reject("?"));
    assert_eq!(
        session.get_ref().written_text(),
        "ATE0\\rOK\\r\\n\\r\\n>\
         MOCK ELM327 v9.9\\r\\n\\r\\n>\
         tachtalk mock\\r\\n\\r\\n>\
         \\r\\n\\r\\nMOCK ELM327 v9.9\\r\\n\\r\\n>"
    );
}

#[test]
fn obd_command_is_handed_out_trimmed_with_raw_bytes() {
    let (_, seen) = serve_input(b"ATE0\r 010c 1 \r");
    assert_eq!(seen, ["AT ATE0", "OBD 010c 1 |  010c 1 "]);
}

#[test]
fn commands_torn_across_reads_are_reassembled() {
    let mut session = firmware_session(FakeStream::torn(b"ATE0\r010C\r"));
    let seen = serve(&mut session, |_| Answer::Reject("?"));
    assert_eq!(seen, ["AT ATE0", "OBD 010C | 010C"]);
}

#[test]
fn a_read_timeout_keeps_the_partial_line() {
    let stream = FakeStream::script(vec![
        Step::Read(b"ATE0\r01".to_vec()),
        Step::Timeout,
        Step::Read(b"0C\r".to_vec()),
    ]);
    let mut session = firmware_session(stream);
    match session.next_command().unwrap() {
        Command::At(at) => at.answer().unwrap(),
        Command::Obd(_) => panic!("expected ATE0"),
    }
    let err = session.next_command().expect_err("a timeout");
    assert_eq!(err.kind(), ErrorKind::TimedOut);
    let seen = serve(&mut session, |_| Answer::Reject("?"));
    assert_eq!(seen, ["OBD 010C | 010C"]);
}

#[test]
fn reply_formats_one_line_per_ecu_with_spaces() {
    let mut session = firmware_session(FakeStream::new(b"ATE0\r010C\rATS0\r010C\r"));
    serve(&mut session, |_| {
        Answer::Reply(vec![
            line(&[0x41, 0x0C, 0x1A, 0xF8]),
            line(&[0x41, 0x0C, 0x1B, 0x00]),
        ])
    });
    assert_eq!(
        session.get_ref().written_text(),
        "ATE0\\rOK\\r\\n\\r\\n>\
         41 0C 1A F8\\r\\n41 0C 1B 00\\r\\n\\r\\n>\
         OK\\r\\n\\r\\n>\
         410C1AF8\\r\\n410C1B00\\r\\n\\r\\n>"
    );
}

#[test]
fn reject_writes_a_line_with_the_current_line_ending() {
    let (written, _) = serve_input(b"ATE0\r010C\rATL0\r010C\r");
    assert_eq!(
        written,
        // ATL0's own reply still uses the old CRLF; the reject after it, CR.
        "ATE0\\rOK\\r\\n\\r\\n>?\\r\\n\\r\\n>OK\\r\\n\\r\\n>?\\r\\r>"
    );
}

#[test]
fn each_command_is_answered_before_the_next_is_read() {
    // One read holds an OBD line and then ATS0: the OBD line is answered
    // with spaces, because ATS0 has not been processed yet.
    let mut session = firmware_session(FakeStream::new(b"ATE0\r010C\rATS0\r"));
    serve(&mut session, |_| {
        Answer::Reply(vec![line(&[0x41, 0x0C, 0x1A, 0xF8])])
    });
    assert_eq!(
        session.get_ref().written_text(),
        "ATE0\\rOK\\r\\n\\r\\n>41 0C 1A F8\\r\\n\\r\\n>OK\\r\\n\\r\\n>"
    );
}

#[test]
fn a_bare_cr_repeats_the_last_replied_command() {
    // The datasheet's repeat: a single carriage return re-runs the last
    // command (changed from the invented `1`, ADR 0001).
    let mut session = firmware_session(FakeStream::new(b"ATE0\r010C 1\r\r  \r"));
    let seen = serve(&mut session, |_| {
        Answer::Reply(vec![line(&[0x41, 0x0C, 0x1A, 0xF8])])
    });
    assert_eq!(
        seen,
        [
            "AT ATE0",
            "OBD 010C 1 | 010C 1",
            "OBD 010C 1 | 010C 1",
            "OBD 010C 1 | 010C 1"
        ]
    );
}

#[test]
fn a_bare_cr_after_a_rejected_command_answers_a_question_mark() {
    // A rejected command is not recorded, so there is nothing to repeat;
    // the session answers the CR itself without handing anything out.
    let (written, seen) = serve_input(b"ATE0\r010C\r\r");
    assert_eq!(seen, ["AT ATE0", "OBD 010C | 010C"]);
    assert_eq!(
        written,
        "ATE0\\rOK\\r\\n\\r\\n>?\\r\\n\\r\\n>?\\r\\n\\r\\n>"
    );
}

#[test]
fn a_1_is_an_ordinary_command() {
    // The invented `1` repeat is retired (ADR 0001): it reaches the program.
    let (_, seen) = serve_input(b"ATE0\r1\r");
    assert_eq!(seen, ["AT ATE0", "OBD 1 | 1"]);
}

#[test]
fn torn_reads_write_the_same_bytes_as_whole_reads() {
    let script: &[u8] = b"ATZ\rATE0\r010C\rAT H1\r\r  \rATL0\ratdp\r\r\n010D\r";
    let answer =
        |_: &ObdCommand<'_, FakeStream>| Answer::Reply(vec![line(&[0x41, 0x0C, 0x1A, 0xF8])]);

    let mut whole = firmware_session(FakeStream::new(script));
    let whole_seen = serve(&mut whole, answer);
    let mut torn = firmware_session(FakeStream::torn(script));
    let torn_seen = serve(&mut torn, answer);

    assert_eq!(whole.get_ref().written(), torn.get_ref().written());
    assert_eq!(whole_seen, torn_seen);
    assert_eq!(
        whole_seen,
        [
            "AT ATZ",
            "AT ATE0",
            "OBD 010C | 010C",
            "AT AT H1",
            "OBD 010C | 010C",
            "OBD 010C | 010C",
            "AT ATL0",
            "AT atdp",
            "OBD 010C | 010C",
            "OBD 010D | 010D",
        ]
    );
}

#[test]
fn an_at_command_dropped_unanswered_is_answered() {
    let mut session = firmware_session(FakeStream::new(b"ATE0\rATI\r"));
    for _ in 0..2 {
        drop(session.next_command().unwrap());
    }
    assert_eq!(
        session.get_ref().written_text(),
        "ATE0\\rOK\\r\\n\\r\\n>ELM327 v1.5\\r\\n\\r\\n>"
    );
}

#[test]
fn a_failed_answer_on_drop_is_returned_by_the_next_call() {
    let mut session = firmware_session(FakeStream::script(vec![
        Step::Read(b"ATE0\r".to_vec()),
        Step::Read(b"ATI\r".to_vec()),
    ]));
    let at = session.next_command().unwrap();
    // The echo of ATE0 is already written; the reply written on drop fails.
    drop(at);
    session.get_mut().fail_writes();
    let at = session.next_command().unwrap();
    drop(at);
    let err = session.next_command().expect_err("the failed write");
    assert_eq!(err.kind(), ErrorKind::BrokenPipe);
}

#[test]
#[should_panic(expected = "dropped unanswered")]
fn an_obd_command_dropped_unanswered_panics() {
    let mut session = firmware_session(FakeStream::new(b"010C\r"));
    drop(session.next_command().unwrap());
}
