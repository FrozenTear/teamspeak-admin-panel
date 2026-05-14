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

/// Apply spec §10.4 escapes to a single ASCII codepoint. Returns the
/// escape sequence that replaces the input character, or `None` to keep
/// the character as-is.
///
/// Every spec-§10.4 escape source is a single-byte ASCII character, so
/// callers iterate `value.chars()` and only consult this table for chars
/// in `..0x80`. Non-ASCII codepoints always pass through untouched —
/// preserving the byte-exact UTF-8 sequence the operator sent.
fn escape_byte(b: u8) -> Option<&'static str> {
    match b {
        b'\\' => Some("\\\\"),
        b'/' => Some("\\/"),
        b' ' => Some("\\s"),
        b'|' => Some("\\p"),
        b'\n' => Some("\\n"),
        b'\r' => Some("\\r"),
        b'\t' => Some("\\t"),
        0x08 => Some("\\b"),
        0x0c => Some("\\f"),
        _ => None,
    }
}

/// Escape a ServerQuery *value* per spec §10.4. The escape table is
/// applied to ASCII codepoints; UTF-8 multi-byte characters pass through
/// untouched.
///
/// R7 note (PURA-161): an earlier byte-oriented implementation pushed
/// each byte through `char::from(byte)`, which interprets the byte as a
/// Latin-1 codepoint and corrupts every UTF-8 multi-byte sequence on the
/// way out. The fuzz harness below now catches that regression class.
pub fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if let Some(byte) = ascii_byte(ch) {
            if let Some(seq) = escape_byte(byte) {
                out.push_str(seq);
                continue;
            }
        }
        out.push(ch);
    }
    out
}

/// Reverse of [`escape`]. Used when parsing ServerQuery responses
/// received over the SSH bridge. Operates on `chars`, so non-ASCII
/// codepoints inside the input survive intact.
pub fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('s') => out.push(' '),
            Some('p') => out.push('|'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('b') => out.push('\u{0008}'),
            Some('f') => out.push('\u{000c}'),
            Some(other) => {
                // Unknown sequence: preserve verbatim so the round-trip
                // is lossless on inputs that were never escaped on the
                // way in.
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// `Some(b)` if the codepoint is a single-byte ASCII character, else
/// `None`. Anything `< 0x80` fits in a `u8` and corresponds to a
/// single-byte UTF-8 codepoint that the escape table can match against.
fn ascii_byte(ch: char) -> Option<u8> {
    if (ch as u32) < 0x80 { Some(ch as u8) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

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

    // -----------------------------------------------------------------
    // R7 — proptest harness for the ServerQuery escaper. PURA-161.
    //
    // The impl-plan risk register's R7 calls for "fuzz the escaper against
    // the spec rules; add a roundtrip test." These property checks act as
    // the in-tree fuzz harness: every assertion runs against thousands of
    // random byte sequences (UTF-8 and otherwise) per `cargo test` and
    // catches every escaper bypass class we care about for spec §10.4.
    // -----------------------------------------------------------------

    /// Spec §10.4 ServerQuery metacharacters. An attacker controlling a
    /// value mustn't be able to forge a `|`-delimited frame separator, a
    /// space-delimited `key=value` separator, or any of the C-style
    /// escapes in the operand text after [`escape`] has been applied.
    /// The injection-resistance check below special-cases backslash as
    /// the *escape prefix* (skip the trailing payload byte) rather than
    /// as a forbidden metacharacter.
    const METACHARS: &[u8] = b"\\/ |\n\r\t\x08\x0c";

    // `unescape` MUST handle arbitrary user-supplied bytes (received over
    // the wire from the TS6 server through the SSH bridge) without
    // panicking, regardless of what trailing or unknown escape sequences
    // the input contains.
    proptest! {
        // Property: no panic on arbitrary input. The output is
        // unconstrained.
        #[test]
        fn unescape_never_panics_on_arbitrary_input(input in ".*") {
            let _ = unescape(&input);
        }

        // The R7 roundtrip property — for every input we put on the
        // wire, the receiver must observe exactly the bytes we sent.
        #[test]
        fn roundtrip_escape_then_unescape_is_identity(input in ".*") {
            prop_assert_eq!(unescape(&escape(&input)), input);
        }

        // Escape-injection resistance: the output of `escape` MUST NOT
        // contain any *bare* ServerQuery metacharacter. The only place
        // these bytes are allowed is as the second byte of an escape
        // pair (e.g. `\\` after a leading `\`). If a metacharacter
        // slipped through unescaped, an attacker controlling the value
        // could forge a frame separator or wire-level command boundary.
        #[test]
        fn escape_output_contains_no_unescaped_metacharacter(input in ".*") {
            let escaped = escape(&input);
            let bytes = escaped.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    prop_assert!(
                        i + 1 < bytes.len(),
                        "escape() must not emit a trailing solitary backslash"
                    );
                    i += 2;
                    continue;
                }
                prop_assert!(
                    !METACHARS.contains(&bytes[i]),
                    "escape() leaked an unescaped metacharacter byte {:#04x} at index {} in output {:?}",
                    bytes[i],
                    i,
                    escaped
                );
                i += 1;
            }
        }

        // `unescape` on any string that contains no backslashes is the
        // identity. (Sanity invariant — guards against `unescape` ever
        // growing a side-channel that decodes non-escape sequences.)
        #[test]
        fn unescape_is_identity_on_unescaped_input(
            input in "[^\\\\]*"
        ) {
            prop_assert_eq!(unescape(&input), input);
        }

        // Reverse direction: the unescape→escape round trip is the
        // identity for any *already-escaped* value (escape outputs).
        #[test]
        fn escape_after_unescape_is_identity_on_escaped_input(input in ".*") {
            let once = escape(&input);
            let decoded = unescape(&once);
            prop_assert_eq!(escape(&decoded), once);
        }
    }

    // Random byte vectors (including invalid UTF-8) fed through
    // `unescape` must not panic. Inputs are routed via lossy UTF-8
    // conversion because the escaper's API is `&str`, but a real TS6
    // SSH frame can contain arbitrary bytes that `russh`/`tracing`
    // surface to us through lossy decode paths.
    proptest! {
        #[test]
        fn unescape_never_panics_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
            let cow = String::from_utf8_lossy(&bytes);
            let _ = unescape(&cow);
        }
    }
}
