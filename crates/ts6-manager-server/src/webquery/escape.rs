//! Spec §10.4 — ServerQuery wire escaping.
//!
//! These escapes are mandatory on the **SSH bridge** (Chapter 11), where
//! values are dispatched as raw `key=value` ServerQuery frames. They are
//! intentionally **not** applied to WebQuery HTTP requests: the spec is
//! explicit that WebQuery's URL-encoder is responsible on the wire and the
//! implementation MUST NOT double-escape.
//!
//! The pair is exposed publicly so SSHBRIDGE (PURA-?, future stream) can
//! consume them when it lands. WebQuery itself does not call [`escape`].

/// Apply spec §10.4 escapes to a value byte. Returns the escape sequence
/// that replaces the input character, or `None` to keep the character as-is.
fn escape_byte(b: u8) -> Option<&'static [u8]> {
    match b {
        b'\\' => Some(b"\\\\"),
        b'/' => Some(b"\\/"),
        b' ' => Some(b"\\s"),
        b'|' => Some(b"\\p"),
        b'\n' => Some(b"\\n"),
        b'\r' => Some(b"\\r"),
        b'\t' => Some(b"\\t"),
        0x08 => Some(b"\\b"),
        0x0c => Some(b"\\f"),
        _ => None,
    }
}

/// Escape a ServerQuery *value* per spec §10.4. The escape table is applied
/// byte-by-byte; UTF-8 multi-byte characters pass through untouched.
pub fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match escape_byte(byte) {
            Some(seq) => out.push_str(std::str::from_utf8(seq).expect("seq is ASCII")),
            None => out.push(char::from(byte)),
        }
    }
    out
}

/// Reverse of [`escape`]. Used when parsing ServerQuery responses received
/// over the SSH bridge.
pub fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut bytes = value.bytes().peekable();
    while let Some(b) = bytes.next() {
        if b != b'\\' {
            out.push(char::from(b));
            continue;
        }
        match bytes.next() {
            Some(b'\\') => out.push('\\'),
            Some(b'/') => out.push('/'),
            Some(b's') => out.push(' '),
            Some(b'p') => out.push('|'),
            Some(b'n') => out.push('\n'),
            Some(b'r') => out.push('\r'),
            Some(b't') => out.push('\t'),
            Some(b'b') => out.push(0x08 as char),
            Some(b'f') => out.push(0x0c as char),
            Some(other) => {
                // Unknown sequence: preserve verbatim so the round-trip is
                // lossless on inputs that were never escaped on the way in.
                out.push('\\');
                out.push(char::from(other));
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_table_per_spec() {
        assert_eq!(escape("hello world"), "hello\\sworld");
        assert_eq!(escape("a|b"), "a\\pb");
        assert_eq!(escape("a/b"), "a\\/b");
        assert_eq!(escape("a\\b"), "a\\\\b");
        assert_eq!(escape("line1\nline2"), "line1\\nline2");
        assert_eq!(escape("col1\tcol2"), "col1\\tcol2");
        assert_eq!(escape("cr\rlf\n"), "cr\\rlf\\n");
        assert_eq!(escape("bs\u{0008}ff\u{000c}"), "bs\\bff\\f");
    }

    #[test]
    fn round_trip_preserves_value() {
        for raw in [
            "plain",
            "a b c",
            "pipes|and|stuff",
            "weird\\\\stuff",
            "with /slash",
            "tabs\tand\nnewlines",
        ] {
            assert_eq!(unescape(&escape(raw)), raw);
        }
    }

    #[test]
    fn unescape_passes_through_unknown_sequences() {
        assert_eq!(unescape("a\\zb"), "a\\zb");
        assert_eq!(unescape("trailing\\"), "trailing\\");
    }
}
