//! Spec §11.4 — ServerQuery line-protocol wire parsing for the SSH bridge.
//!
//! The SSH session yields a CR-LF terminated stream. Three line shapes matter:
//!
//! - `notify*` — events. First space-separated token is the event name; the
//!   remainder is a pipe-separated list of records, each record a
//!   space-separated list of `key=value` pairs (values [un]escaped per
//!   spec §10.4).
//! - `error id=<n> msg=<text>` — terminates the in-flight command. `id == 0`
//!   resolves the command with the accumulated body lines; non-zero rejects
//!   with a typed [`crate::sshbridge::SshBridgeError::Upstream`].
//! - Anything else (between command issue and `error …`) — body lines that
//!   accumulate as the response.
//!
//! This module parses the wire bytes; it does NOT do I/O. The transport
//! layer (russh integration in a follow-up child issue) feeds it bytes via
//! [`LineBuffer::push`] and consumes terminated frames via
//! [`LineBuffer::drain_lines`].
//!
//! `key=value` records are normalised to `Vec<HashMap<String, String>>` with
//! values §10.4-unescaped. The shape is JSON-compatible; the SSH command
//! surface re-uses the existing `crate::webquery::models` types by feeding
//! the records through `serde_json::Value` and the `stringy` deserialisers
//! the WebQuery models already define for TS6's untyped wire numerics.

use std::collections::HashMap;

use crate::webquery::escape::unescape;

/// Buffers incoming bytes from the SSH channel and yields complete lines.
///
/// The transport layer reads bytes off the SSH channel as they arrive (which
/// rarely aligns to line boundaries) and pushes them into this buffer. The
/// buffer holds a trailing partial line until the next push completes it.
///
/// Per spec §11.4: split on `/\r?\n/`, discard empty lines.
#[derive(Debug, Default)]
pub struct LineBuffer {
    pending: Vec<u8>,
}

impl LineBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append raw bytes from the channel.
    pub fn push(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
    }

    /// Drain every complete (CR-LF terminated) line from the buffer.
    ///
    /// Empty lines are discarded per spec §11.4. The trailing partial line
    /// (if any) stays in the buffer for the next [`push`].
    ///
    /// Lines are returned as owned `String`s — TS6 ServerQuery is ASCII for
    /// keywords with UTF-8 only inside escaped values, and the §10.4 escape
    /// table renders the wire bytes safe to UTF-8-decode after splitting.
    /// We treat invalid UTF-8 as a pass-through byte sequence by lossy
    /// decoding so a malformed frame still surfaces (and the upstream
    /// `error id=…` or non-parseable record visibility is preserved).
    ///
    /// [`push`]: LineBuffer::push
    pub fn drain_lines(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        let mut start = 0usize;
        let mut i = 0usize;
        while i < self.pending.len() {
            if self.pending[i] == b'\n' {
                let mut end = i;
                if end > start && self.pending[end - 1] == b'\r' {
                    end -= 1;
                }
                if end > start {
                    out.push(String::from_utf8_lossy(&self.pending[start..end]).into_owned());
                }
                // empty lines silently discarded
                start = i + 1;
            }
            i += 1;
        }
        if start > 0 {
            self.pending.drain(0..start);
        }
        out
    }
}

/// A parsed `error id=<n> msg=<text>` frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorFrame {
    pub id: i64,
    pub msg: String,
}

/// A parsed `notify*` frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyFrame {
    pub event: String,
    /// One record per pipe-separated entry. Each record is the §10.4-unescaped
    /// `key=value` pairs from that segment.
    pub records: Vec<HashMap<String, String>>,
}

/// What kind of frame a single ServerQuery line represents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// `error id=<n> msg=<text>` — terminator for the in-flight command.
    Error(ErrorFrame),
    /// `notify*` — event frame; routed to event subscribers, never to a
    /// pending command.
    Notify(NotifyFrame),
    /// Any other non-empty line. While a command is in flight it is body;
    /// outside a command's window it is unsolicited and the transport layer
    /// logs+drops it.
    Body(String),
}

impl Frame {
    /// Classify a raw (already-CRLF-stripped, non-empty) line.
    pub fn classify(line: &str) -> Frame {
        if let Some(rest) = strip_prefix_word(line, "error") {
            // strip_prefix_word ensures `rest` starts at the first non-space
            // byte after the command word — `error id=0 msg=ok`.
            return Frame::Error(parse_error(rest));
        }
        if line.starts_with("notify") {
            // event name = everything up to the first space; remainder is
            // the records segment.
            let (event, rest) = split_first_space(line);
            return Frame::Notify(NotifyFrame {
                event: event.to_string(),
                records: parse_records(rest),
            });
        }
        Frame::Body(line.to_string())
    }
}

/// Parse the `id=<n> msg=<text>` portion of an error frame.
fn parse_error(rest: &str) -> ErrorFrame {
    // The error-frame body is itself a record: `id=<n> msg=<text>` with the
    // §10.4 escape table. Re-use the record parser, then pull out the two
    // canonical keys. Missing `id` is reported as -1 (transport-style) so
    // the caller sees a non-zero failure rather than a silent success.
    let mut record = parse_record(rest);
    let id = record
        .remove("id")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(-1);
    let msg = record.remove("msg").unwrap_or_default();
    ErrorFrame { id, msg }
}

/// Parse the records segment of a frame into one `HashMap` per pipe-separated
/// entry. Empty input yields an empty `Vec`.
pub fn parse_records(rest: &str) -> Vec<HashMap<String, String>> {
    if rest.is_empty() {
        return Vec::new();
    }
    rest.split('|').map(parse_record).collect()
}

/// Parse a single record (one pipe segment) into `key=value` pairs.
///
/// Pairs are space-separated. Values are §10.4-unescaped. A bare token (no
/// `=`) is recorded as `key="" `.
pub fn parse_record(segment: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for token in segment.split(' ') {
        if token.is_empty() {
            continue;
        }
        match token.split_once('=') {
            Some((k, v)) => {
                if !k.is_empty() {
                    out.insert(k.to_string(), unescape(v));
                }
            }
            None => {
                out.insert(token.to_string(), String::new());
            }
        }
    }
    out
}

/// Strip a leading whole word, returning the remainder with leading spaces
/// trimmed. Returns `None` if `line` does not start with `word` followed by
/// either a space or end-of-line.
fn strip_prefix_word<'a>(line: &'a str, word: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(word)?;
    match rest.chars().next() {
        None => Some(""),
        Some(' ') => Some(rest.trim_start_matches(' ')),
        _ => None,
    }
}

/// Split at the first space; if there is no space, the whole line is the
/// first piece and the second is empty.
fn split_first_space(line: &str) -> (&str, &str) {
    match line.find(' ') {
        Some(ix) => (&line[..ix], line[ix + 1..].trim_start_matches(' ')),
        None => (line, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_buffer_yields_complete_lines_only() {
        let mut buf = LineBuffer::new();
        buf.push(b"error id=0 msg=ok\r\nnotifycliententer ");
        let lines = buf.drain_lines();
        assert_eq!(lines, vec!["error id=0 msg=ok"]);
        // partial line stays in the buffer
        assert_eq!(buf.drain_lines(), Vec::<String>::new());
        buf.push(b"clid=5\r\n");
        assert_eq!(buf.drain_lines(), vec!["notifycliententer clid=5"]);
    }

    #[test]
    fn line_buffer_handles_lf_only() {
        // Some servers strip the CR; spec allows `/\r?\n/`.
        let mut buf = LineBuffer::new();
        buf.push(b"first\nsecond\n");
        assert_eq!(buf.drain_lines(), vec!["first", "second"]);
    }

    #[test]
    fn line_buffer_discards_empty_lines() {
        let mut buf = LineBuffer::new();
        buf.push(b"\r\n\r\nreal\r\n\r\n");
        assert_eq!(buf.drain_lines(), vec!["real"]);
    }

    #[test]
    fn classify_error_frame() {
        let f = Frame::classify("error id=0 msg=ok");
        assert_eq!(
            f,
            Frame::Error(ErrorFrame {
                id: 0,
                msg: "ok".into()
            })
        );
    }

    #[test]
    fn classify_error_frame_with_escaped_message() {
        // `\s` is the §10.4 escape for space.
        let f = Frame::classify("error id=2568 msg=insufficient\\sclient\\spermissions");
        assert_eq!(
            f,
            Frame::Error(ErrorFrame {
                id: 2568,
                msg: "insufficient client permissions".into()
            })
        );
    }

    #[test]
    fn classify_notify_with_one_record() {
        let f = Frame::classify("notifyclientmoved ctid=5 reasonid=0 clid=12");
        match f {
            Frame::Notify(n) => {
                assert_eq!(n.event, "notifyclientmoved");
                assert_eq!(n.records.len(), 1);
                let r = &n.records[0];
                assert_eq!(r.get("ctid").map(String::as_str), Some("5"));
                assert_eq!(r.get("clid").map(String::as_str), Some("12"));
                assert_eq!(r.get("reasonid").map(String::as_str), Some("0"));
            }
            other => panic!("expected Notify, got {other:?}"),
        }
    }

    #[test]
    fn classify_notify_with_pipe_separated_records() {
        let f = Frame::classify(
            "notifyclientupdated clid=12 client_nickname=alice|clid=13 client_nickname=bob",
        );
        match f {
            Frame::Notify(n) => {
                assert_eq!(n.records.len(), 2);
                assert_eq!(n.records[0].get("clid").map(String::as_str), Some("12"));
                assert_eq!(
                    n.records[0].get("client_nickname").map(String::as_str),
                    Some("alice")
                );
                assert_eq!(n.records[1].get("clid").map(String::as_str), Some("13"));
                assert_eq!(
                    n.records[1].get("client_nickname").map(String::as_str),
                    Some("bob")
                );
            }
            other => panic!("expected Notify, got {other:?}"),
        }
    }

    #[test]
    fn classify_body_frame() {
        let f = Frame::classify("virtualserver_name=My\\sServer virtualserver_id=3");
        match f {
            Frame::Body(s) => {
                assert!(s.contains("virtualserver_id=3"));
                assert!(s.contains("My\\sServer"));
            }
            other => panic!("expected Body, got {other:?}"),
        }
    }

    #[test]
    fn parse_record_unescapes_value() {
        let r = parse_record("client_nickname=alice\\sthe\\sgreat client_id=42");
        assert_eq!(
            r.get("client_nickname").map(String::as_str),
            Some("alice the great")
        );
        assert_eq!(r.get("client_id").map(String::as_str), Some("42"));
    }

    #[test]
    fn parse_record_handles_empty_value() {
        let r = parse_record("client_away_message= client_away=1");
        assert_eq!(r.get("client_away_message").map(String::as_str), Some(""));
        assert_eq!(r.get("client_away").map(String::as_str), Some("1"));
    }

    #[test]
    fn parse_record_keeps_bare_token() {
        // Some banner/OK frames carry a bare keyword.
        let r = parse_record("ok");
        assert_eq!(r.get("ok").map(String::as_str), Some(""));
    }

    #[test]
    fn parse_records_empty_input_is_empty_vec() {
        assert_eq!(parse_records(""), Vec::<HashMap<String, String>>::new());
    }

    #[test]
    fn strip_prefix_word_requires_word_boundary() {
        // `errors` should NOT match `error` — needs space or EOL after.
        assert!(strip_prefix_word("errors id=0", "error").is_none());
        assert_eq!(strip_prefix_word("error id=0", "error"), Some("id=0"));
        assert_eq!(strip_prefix_word("error", "error"), Some(""));
        assert_eq!(strip_prefix_word("error  id=0", "error"), Some("id=0"));
    }

    #[test]
    fn missing_error_id_yields_minus_one() {
        let f = Frame::classify("error msg=mystery");
        match f {
            Frame::Error(ErrorFrame { id, msg }) => {
                assert_eq!(id, -1, "missing id reports as -1, never as success");
                assert_eq!(msg, "mystery");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }
}
