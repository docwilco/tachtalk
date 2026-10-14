//! Tests for `ClientSession`: hand-written exchanges, then whole captured
//! sessions against a real ELM327 v2.3 replayed from `fixtures/`.
//!
//! Behavior is a faithful move of the firmware dongle task's protocol half
//! (`DongleState` in tachtalk-firmware/src/obd2.rs as of 3a543b9), except
//! where a test says otherwise.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::time::Duration;

use tachtalk_elm327_lib::{ClientConfig, ClientSession, FeatureSupport, Lines, Pid, ReplyLine};

use crate::support::{FakeStream, Step};

fn sent(bytes: &[u8]) -> Step {
    Step::Expect(bytes.to_vec())
}

fn received(bytes: &[u8]) -> Step {
    Step::Read(bytes.to_vec())
}

fn session(steps: Vec<Step>) -> ClientSession<FakeStream> {
    session_with(steps, ClientConfig::default())
}

fn session_with(steps: Vec<Step>, config: ClientConfig) -> ClientSession<FakeStream> {
    ClientSession::new(FakeStream::script(steps), config)
}

fn line(ecu: Option<u16>, data: &[u8]) -> ReplyLine {
    ReplyLine {
        ecu,
        data: data.into(),
    }
}

fn entries(pairs: &[(Pid, &[ReplyLine])]) -> HashMap<Pid, Lines> {
    pairs
        .iter()
        .map(|(pid, lines)| (*pid, lines.iter().cloned().collect()))
        .collect()
}

#[test]
fn init_resets_then_sends_the_init_commands() {
    let config = ClientConfig {
        reset_wait: Duration::ZERO,
        ..ClientConfig::default()
    };
    let mut client = session_with(
        vec![
            sent(b"ATZ\r"),
            received(b"\r\rELM327 v2.3\r\r>"),
            sent(b"ATE0\r"),
            received(b"ATE0\rOK\r\r>"),
            sent(b"ATL0\r"),
            received(b"OK\r\r>"),
            sent(b"ATS0\r"),
            received(b"OK\r\r>"),
            sent(b"ATSP0\r"),
            received(b"OK\r\r>"),
            sent(b"ATH0\r"),
            received(b"OK\r\r>"),
        ],
        config,
    );
    client.init().unwrap();
    assert!(!client.server_state().framing());
}

#[test]
fn init_with_framing_sends_ath1() {
    let config = ClientConfig {
        framing: true,
        reset_wait: Duration::ZERO,
        ..ClientConfig::default()
    };
    let mut steps = vec![sent(b"ATZ\r"), received(b"\r\rELM327 v2.3\r\r>")];
    for cmd in ["ATE0", "ATL0", "ATS0", "ATSP0", "ATH1"] {
        steps.push(sent(format!("{cmd}\r").as_bytes()));
        steps.push(received(b"OK\r\r>"));
    }
    let mut client = session_with(steps, config);
    client.init().unwrap();
    assert!(client.server_state().framing());
}

#[test]
fn command_returns_the_raw_reply() {
    let mut client = session(vec![sent(b"0100\r"), received(b"4100BE3FA813\r\r>")]);
    assert_eq!(&client.command(b"0100").unwrap()[..], b"4100BE3FA813\r\r>");
}

#[test]
fn query_pid_sends_the_count_only_when_given() {
    let mut client = session(vec![
        sent(b"010C\r"),
        received(b"410C1AF8\r\r>"),
        sent(b"010D 1\r"),
        received(b"410D32\r\r>"),
    ]);
    assert_eq!(
        client.query_pid(0x0C, None).unwrap(),
        entries(&[(0x0C, &[line(None, &[0x41, 0x0C, 0x1A, 0xF8])])])
    );
    assert_eq!(
        client.query_pid(0x0D, Some(1)).unwrap(),
        entries(&[(0x0D, &[line(None, &[0x41, 0x0D, 0x32])])])
    );
}

#[test]
fn a_reply_torn_across_reads_is_reassembled() {
    let stream = FakeStream::script(vec![sent(b"010C\r"), received(b"410C1AF8\r\r>")]).max_read(1);
    let mut client = ClientSession::new(stream, ClientConfig::default());
    assert_eq!(client.query_pid(0x0C, None).unwrap().len(), 1);
}

#[test]
fn every_answering_ecu_is_a_line_and_counted() {
    let mut client = session(vec![sent(b"010C\r"), received(b"410C1AF8\r410C1B00\r\r>")]);
    assert_eq!(
        client.query_pid(0x0C, None).unwrap(),
        entries(&[(
            0x0C,
            &[
                line(None, &[0x41, 0x0C, 0x1A, 0xF8]),
                line(None, &[0x41, 0x0C, 0x1B, 0x00]),
            ]
        )])
    );
    assert_eq!(client.server_state().ecu_count(0x0C), Some(2));
}

#[test]
fn with_framing_lines_carry_their_ecu_and_ecus_are_counted_by_id() {
    // Changed from the firmware: its cache parser read the CAN id as data
    // and dropped framed replies; lines now keep the id they arrived with.
    let mut client = session(vec![
        sent(b"ATH1\r"),
        received(b"OK\r\r>"),
        sent(b"010C\r"),
        received(b"7E804410C1AF8\r7E904410C1B00\r7E804410C1AF8\r\r>"),
    ]);
    client.command(b"ATH1").unwrap();
    let parsed = client.query_pid(0x0C, None).unwrap();
    assert_eq!(
        parsed[&0x0C][0],
        line(Some(0x7E8), &[0x41, 0x0C, 0x1A, 0xF8])
    );
    assert_eq!(
        parsed[&0x0C][1],
        line(Some(0x7E9), &[0x41, 0x0C, 0x1B, 0x00])
    );
    // Two distinct ECUs, though three lines arrived.
    assert_eq!(client.server_state().ecu_count(0x0C), Some(2));
}

#[test]
fn an_unknown_pids_data_length_is_learned() {
    // PID A6 (odometer) is not in the J1979 table this library carries.
    let mut client = session(vec![sent(b"01A6\r"), received(b"41A600012345\r\r>")]);
    assert_eq!(client.server_state().data_length(0xA6), None);
    client.query_pid(0xA6, None).unwrap();
    assert_eq!(client.server_state().data_length(0xA6), Some(4));
}

#[test]
fn ecus_disagreeing_on_a_data_length_teach_nothing() {
    let mut client = session(vec![
        sent(b"01A6\r"),
        received(b"41A600012345\r41A60001\r\r>"),
    ]);
    client.query_pid(0xA6, None).unwrap();
    assert_eq!(client.server_state().data_length(0xA6), None);
}

#[test]
fn query_pids_splits_a_multi_pid_reply_and_confirms_support() {
    let mut client = session(vec![sent(b"010C05 1\r"), received(b"410C0CB80562\r\r>")]);
    assert_eq!(client.server_state().multi_pid(), FeatureSupport::Unknown);
    assert_eq!(
        client.query_pids(&[0x0C, 0x05], Some(1)).unwrap(),
        entries(&[
            (0x0C, &[line(None, &[0x41, 0x0C, 0x0C, 0xB8])]),
            (0x05, &[line(None, &[0x41, 0x05, 0x62])]),
        ])
    );
    assert_eq!(client.server_state().multi_pid(), FeatureSupport::Yes);
}

#[test]
fn a_multi_pid_reply_with_one_pid_marks_it_unsupported() {
    let mut client = session(vec![sent(b"010C49 1\r"), received(b"410C0CD4\r\r>")]);
    client.query_pids(&[0x0C, 0x49], Some(1)).unwrap();
    assert_eq!(client.server_state().multi_pid(), FeatureSupport::No);
}

#[test]
fn a_failed_multi_pid_query_marks_it_unsupported() {
    let mut client = session(vec![sent(b"010C05\r"), Step::Timeout]);
    let err = client.query_pids(&[0x0C, 0x05], None).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::TimedOut);
    assert_eq!(client.server_state().multi_pid(), FeatureSupport::No);
}

#[test]
fn no_data_parses_to_nothing() {
    let mut client = session(vec![sent(b"0149 1\r"), received(b"NO DATA\r\r>")]);
    assert!(client.query_pid(0x49, Some(1)).unwrap().is_empty());
}

#[test]
fn a_closed_connection_is_unexpected_eof() {
    let mut client = session(vec![sent(b"010C\r")]);
    let err = client.query_pid(0x0C, None).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
}

fn repeat_on() -> ClientConfig {
    ClientConfig {
        repeat: true,
        ..ClientConfig::default()
    }
}

#[test]
fn repeat_sends_a_bare_cr_for_an_identical_query() {
    let mut client = session_with(
        vec![
            sent(b"010C 1\r"),
            received(b"410C1AF8\r\r>"),
            sent(b"\r"),
            received(b"410C1B00\r\r>"),
        ],
        repeat_on(),
    );
    client.query_pid(0x0C, Some(1)).unwrap();
    assert_eq!(client.server_state().repeat(), FeatureSupport::Unknown);
    let parsed = client.query_pid(0x0C, Some(1)).unwrap();
    assert_eq!(parsed[&0x0C][0], line(None, &[0x41, 0x0C, 0x1B, 0x00]));
    assert_eq!(client.server_state().repeat(), FeatureSupport::Yes);
}

#[test]
fn a_refused_repeat_resends_the_full_query_and_stops_repeating() {
    let mut client = session_with(
        vec![
            sent(b"010C\r"),
            received(b"410C1AF8\r\r>"),
            sent(b"\r"),
            received(b"?\r\r>"),
            sent(b"010C\r"),
            received(b"410C1B00\r\r>"),
            sent(b"010C\r"),
            received(b"410C1B04\r\r>"),
        ],
        repeat_on(),
    );
    client.query_pid(0x0C, None).unwrap();
    let parsed = client.query_pid(0x0C, None).unwrap();
    assert_eq!(parsed[&0x0C][0], line(None, &[0x41, 0x0C, 0x1B, 0x00]));
    assert_eq!(client.server_state().repeat(), FeatureSupport::No);
    client.query_pid(0x0C, None).unwrap();
}

#[test]
fn an_error_forgets_the_last_command() {
    let mut client = session_with(
        vec![
            sent(b"010C\r"),
            received(b"410C1AF8\r\r>"),
            sent(b"\r"),
            Step::Timeout,
            sent(b"010C\r"),
            received(b"410C1B00\r\r>"),
        ],
        repeat_on(),
    );
    client.query_pid(0x0C, None).unwrap();
    assert!(client.query_pid(0x0C, None).is_err());
    client.query_pid(0x0C, None).unwrap();
}

#[test]
fn repeat_off_always_sends_the_full_query() {
    let mut client = session(vec![
        sent(b"010C\r"),
        received(b"410C1AF8\r\r>"),
        sent(b"010C\r"),
        received(b"410C1B00\r\r>"),
    ]);
    client.query_pid(0x0C, None).unwrap();
    client.query_pid(0x0C, None).unwrap();
    assert_eq!(client.server_state().repeat(), FeatureSupport::No);
}

/// A captured session: every record in order, `true` for sent.
fn transcript(name: &str) -> Vec<(bool, Vec<u8>)> {
    let path = format!(
        "{}/tests/elm327/fixtures/{name}.txt",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| {
            let (dir, data) = l.split_at(2);
            (dir == "> ", unescape(data))
        })
        .collect()
}

fn unescape(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(u8::try_from(c).expect("ASCII"));
            continue;
        }
        match chars.next() {
            Some('r') => out.push(b'\r'),
            Some('n') => out.push(b'\n'),
            Some('t') => out.push(b'\t'),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                out.push(u8::from_str_radix(&hex, 16).expect("hex escape"));
            }
            other => panic!("unknown escape {other:?}"),
        }
    }
    out
}

/// Replay a captured session: each sent record becomes the call that sends
/// it, each received record the input. Returns what the queries parsed.
fn replay(name: &str) -> (ClientSession<FakeStream>, Vec<HashMap<Pid, Lines>>) {
    let records = transcript(name);
    let steps = records
        .iter()
        .map(
            |(is_sent, data)| {
                if *is_sent {
                    sent(data)
                } else {
                    received(data)
                }
            },
        )
        .collect();
    let mut client = session(steps);
    let mut parsed = Vec::new();
    for (_, command) in records.iter().filter(|(is_sent, _)| *is_sent) {
        let text = std::str::from_utf8(command).unwrap().trim_end_matches('\r');
        if text.starts_with("AT") {
            client.command(text.as_bytes()).unwrap();
            continue;
        }
        let (hex, count) = match text.split_once(' ') {
            Some((hex, count)) => (hex, Some(count.parse().unwrap())),
            None => (text, None),
        };
        let pids: Vec<Pid> = hex.as_bytes()[2..]
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
        let reply = if let [pid] = pids[..] {
            client.query_pid(pid, count)
        } else {
            client.query_pids(&pids, count)
        };
        parsed.push(reply.unwrap_or_else(|e| panic!("{name}: {text}: {e}")));
    }
    (client, parsed)
}

#[test]
fn replays_every_captured_session() {
    for name in [
        "2026-02-10T13-15-24-056Z",
        "2026-02-10T13-19-43-520Z",
        "2026-02-10T13-20-12-385Z",
        "2026-02-10T13-23-44-721Z",
        "2026-02-10T13-24-36-102Z",
        "2026-02-10T13-25-29-928Z",
        "2026-02-10T13-25-49-728Z",
        "2026-02-10T13-26-56-363Z",
    ] {
        let (client, _) = replay(name);
        assert!(!client.get_ref().written().is_empty(), "{name}");
    }
}

#[test]
fn a_captured_single_pid_session_parses_rpm_and_no_data() {
    // ATH0; "010C 1" answered 410C0C88, "0149 1" answered NO DATA.
    let (client, parsed) = replay("2026-02-10T13-25-29-928Z");
    assert_eq!(
        parsed[0],
        entries(&[(0x0C, &[line(None, &[0x41, 0x0C, 0x0C, 0x88])])])
    );
    assert!(parsed[1].is_empty());
    assert_eq!(client.server_state().ecu_count(0x0C), Some(1));
    assert!(!client.server_state().framing());
}

#[test]
fn a_captured_framed_session_keeps_the_ecu_id() {
    // ATH1; "010C49 1" answered only for 0C by ECU 7E8: 7E804410C0CD4.
    let (client, parsed) = replay("2026-02-10T13-24-36-102Z");
    assert!(client.server_state().framing());
    assert_eq!(
        parsed[0],
        entries(&[(0x0C, &[line(Some(0x7E8), &[0x41, 0x0C, 0x0C, 0xD4])])])
    );
    // The ECU answered one of the two PIDs asked for.
    assert_eq!(client.server_state().multi_pid(), FeatureSupport::No);
}
