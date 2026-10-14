//! Test support shared by the library's test files.

use std::collections::VecDeque;
use std::io::{self, Read, Write};

/// One step of a [`FakeStream`] script.
#[derive(Debug, Clone)]
pub enum Step {
    /// Bytes the peer sends; reads hand them out, at most `max_read` at a time.
    Read(Vec<u8>),
    /// The next read fails with `TimedOut`, as a socket read timeout does.
    Timeout,
    /// Bytes the session must write next; any other write fails the test.
    Expect(Vec<u8>),
}

/// An in-memory stream standing in for a TCP connection.
///
/// It plays a script of [`Step`]s: reads hand out scripted bytes or a
/// timeout, writes are recorded and checked against `Expect` steps. With
/// the script exhausted a read returns end of file.
#[derive(Debug)]
pub struct FakeStream {
    steps: VecDeque<Step>,
    max_read: usize,
    written: Vec<u8>,
    /// Bytes written since the last matched `Expect` step.
    unmatched: Vec<u8>,
    fail_writes: bool,
}

impl FakeStream {
    /// A stream that hands out `input` as fast as the reader asks.
    pub fn new(input: &[u8]) -> Self {
        Self::script(vec![Step::Read(input.to_vec())])
    }

    /// A stream that hands out `input` one byte per read, like a torn packet.
    pub fn torn(input: &[u8]) -> Self {
        Self::new(input).max_read(1)
    }

    /// A stream playing `steps` in order.
    pub fn script(steps: Vec<Step>) -> Self {
        Self {
            steps: steps.into(),
            max_read: usize::MAX,
            written: Vec::new(),
            unmatched: Vec::new(),
            fail_writes: false,
        }
    }

    /// Hand out at most `n` bytes per read.
    pub fn max_read(mut self, n: usize) -> Self {
        self.max_read = n;
        self
    }

    /// Make every write fail with `BrokenPipe`.
    pub fn fail_writes(&mut self) {
        self.fail_writes = true;
    }

    /// Everything written so far.
    pub fn written(&self) -> &[u8] {
        &self.written
    }

    /// Everything written so far, as text with `\r` and `\n` escaped.
    pub fn written_text(&self) -> String {
        escape(&self.written)
    }

    fn match_expects(&mut self) {
        while let Some(Step::Expect(expected)) = self.steps.front() {
            if self.unmatched.len() < expected.len() {
                assert!(
                    expected.starts_with(&self.unmatched),
                    "expected write {:?}, got {:?}",
                    escape(expected),
                    escape(&self.unmatched),
                );
                return;
            }
            assert!(
                self.unmatched.starts_with(expected),
                "expected write {:?}, got {:?}",
                escape(expected),
                escape(&self.unmatched),
            );
            self.unmatched.drain(..expected.len());
            self.steps.pop_front();
        }
    }
}

impl Read for FakeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.steps.front_mut() {
            None => Ok(0),
            Some(Step::Timeout) => {
                self.steps.pop_front();
                Err(io::ErrorKind::TimedOut.into())
            }
            Some(Step::Expect(expected)) => panic!(
                "read while the script still expects a write of {:?} (written so far: {:?})",
                escape(expected),
                escape(&self.unmatched),
            ),
            Some(Step::Read(data)) => {
                let n = data.len().min(buf.len()).min(self.max_read);
                buf[..n].copy_from_slice(&data[..n]);
                data.drain(..n);
                if data.is_empty() {
                    self.steps.pop_front();
                }
                Ok(n)
            }
        }
    }
}

impl Write for FakeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.fail_writes {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        self.written.extend_from_slice(buf);
        if matches!(self.steps.front(), Some(Step::Expect(_))) {
            self.unmatched.extend_from_slice(buf);
            self.match_expects();
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Bytes as text with `\r` and `\n` spelled out, for readable failures.
pub fn escape(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &b in bytes {
        match b {
            b'\r' => out.push_str("\\r"),
            b'\n' => out.push_str("\\n"),
            _ => out.push(char::from(b)),
        }
    }
    out
}
