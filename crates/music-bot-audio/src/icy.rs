//! ICY stream demuxing helpers (no async — pure byte-stream state machine).
//!
//! Sits behind `crate::source::icy::IcyRadioSource` so the splitter logic can
//! be unit-tested without spinning up a real radio server.

use bytes::Bytes;

/// One demuxed piece coming out of the ICY stream.
pub enum IcyPiece {
    Audio(Bytes),
    Metadata(Bytes),
}

#[derive(Debug)]
enum State {
    /// Reading `audio_remaining` more audio bytes before the next metadata
    /// block. If `audio_remaining == usize::MAX`, the stream has no metadata
    /// (no `icy-metaint` header) and we never transition out of this state.
    Audio { audio_remaining: usize },
    /// Read one length byte L; next state is `Meta { remaining: L*16 }`.
    MetaLength,
    /// Reading `remaining` more metadata bytes. Once collected, decoded and
    /// emitted as a single `Metadata` piece.
    Meta { remaining: usize, buf: Vec<u8> },
}

pub struct IcyStreamSplitter {
    metaint: usize,
    state: State,
    out: std::collections::VecDeque<IcyPiece>,
}

impl IcyStreamSplitter {
    pub fn new(metaint: Option<usize>) -> Self {
        let (metaint, state) = match metaint {
            Some(n) if n > 0 => (n, State::Audio { audio_remaining: n }),
            _ => (
                0,
                State::Audio {
                    audio_remaining: usize::MAX,
                },
            ),
        };
        Self {
            metaint,
            state,
            out: std::collections::VecDeque::new(),
        }
    }

    pub fn feed(&mut self, mut input: &[u8]) {
        while !input.is_empty() {
            match &mut self.state {
                State::Audio { audio_remaining } => {
                    if *audio_remaining == usize::MAX {
                        // No metaint — just pass everything through as audio.
                        self.out
                            .push_back(IcyPiece::Audio(Bytes::copy_from_slice(input)));
                        input = &[];
                    } else {
                        let n = (*audio_remaining).min(input.len());
                        self.out
                            .push_back(IcyPiece::Audio(Bytes::copy_from_slice(&input[..n])));
                        *audio_remaining -= n;
                        input = &input[n..];
                        if *audio_remaining == 0 {
                            self.state = State::MetaLength;
                        }
                    }
                }
                State::MetaLength => {
                    let len_byte = input[0];
                    input = &input[1..];
                    let remaining = len_byte as usize * 16;
                    if remaining == 0 {
                        // Empty metadata block — go back to audio.
                        self.state = State::Audio {
                            audio_remaining: self.metaint,
                        };
                    } else {
                        self.state = State::Meta {
                            remaining,
                            buf: Vec::with_capacity(remaining),
                        };
                    }
                }
                State::Meta { remaining, buf } => {
                    let n = (*remaining).min(input.len());
                    buf.extend_from_slice(&input[..n]);
                    *remaining -= n;
                    input = &input[n..];
                    if *remaining == 0 {
                        let buf = std::mem::take(buf);
                        self.out.push_back(IcyPiece::Metadata(Bytes::from(buf)));
                        self.state = State::Audio {
                            audio_remaining: self.metaint,
                        };
                    }
                }
            }
        }
    }

    pub fn next_piece(&mut self) -> Option<IcyPiece> {
        self.out.pop_front()
    }
}

/// Decode an ICY metadata byte slice as text. Modern Icecast emits UTF-8;
/// older Shoutcast/Icecast servers emit Latin-1 (ISO-8859-1). Try UTF-8
/// strictly first; on failure, map each byte 1:1 onto its Unicode codepoint
/// (ISO-8859-1's identity mapping onto U+0000..=U+00FF). This recovers
/// accented characters that `from_utf8_lossy` would replace with U+FFFD.
fn decode_icy_string(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

/// Extract the `StreamTitle='…';` value from an ICY metadata block. Returns
/// `None` if no `StreamTitle` is present (some stations also send `StreamUrl`,
/// which we ignore).
pub fn parse_stream_title(bytes: &[u8]) -> Option<String> {
    let trimmed: Vec<u8> = bytes.iter().copied().take_while(|&b| b != 0).collect();
    let text = decode_icy_string(&trimmed);
    let needle = "StreamTitle='";
    let start = text.find(needle)? + needle.len();
    let rest = &text[start..];
    let end = rest.find("';")?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_title() {
        let raw = b"StreamTitle='Artist - Track';StreamUrl='';\0\0\0\0";
        assert_eq!(parse_stream_title(raw).as_deref(), Some("Artist - Track"));
    }

    #[test]
    fn parse_title_missing() {
        assert_eq!(parse_stream_title(b"\0\0"), None);
    }

    #[test]
    fn parse_title_latin1_accented() {
        let raw = b"StreamTitle='Caf\xe9 del Mar';\0\0";
        assert_eq!(
            parse_stream_title(raw).as_deref(),
            Some("Café del Mar"),
        );
    }

    #[test]
    fn parse_title_utf8_accented() {
        let raw = b"StreamTitle='Caf\xc3\xa9 del Mar';\0\0";
        let got = parse_stream_title(raw).unwrap();
        assert_eq!(got, "Café del Mar");
        assert_eq!(got.chars().count(), 12);
    }

    #[test]
    fn splits_audio_and_metadata() {
        let metaint = 4;
        let mut s = IcyStreamSplitter::new(Some(metaint));
        // 4 bytes audio, length byte 2 (=> 32 bytes meta), 32 bytes meta,
        // 4 more bytes audio. `StreamTitle='hi';` = 17 bytes — needs L=2.
        let mut buf = vec![0xAA, 0xBB, 0xCC, 0xDD, 0x02];
        let title = b"StreamTitle='hi';";
        let mut padded = title.to_vec();
        padded.resize(32, 0);
        buf.extend_from_slice(&padded);
        buf.extend_from_slice(&[0xEE, 0xFF, 0x11, 0x22]);
        s.feed(&buf);

        let mut audio = Vec::new();
        let mut meta = Vec::new();
        while let Some(piece) = s.next_piece() {
            match piece {
                IcyPiece::Audio(a) => audio.extend_from_slice(&a),
                IcyPiece::Metadata(m) => meta.extend_from_slice(&m),
            }
        }
        assert_eq!(audio, vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x11, 0x22]);
        assert_eq!(parse_stream_title(&meta).as_deref(), Some("hi"));
    }

    #[test]
    fn no_metaint_pipes_through() {
        let mut s = IcyStreamSplitter::new(None);
        s.feed(&[1, 2, 3, 4]);
        let mut got = Vec::new();
        while let Some(IcyPiece::Audio(a)) = s.next_piece() {
            got.extend_from_slice(&a);
        }
        assert_eq!(got, vec![1, 2, 3, 4]);
    }

    #[test]
    fn empty_meta_block_resumes_audio() {
        let mut s = IcyStreamSplitter::new(Some(2));
        // 2 audio bytes, length byte 0 (empty meta), 2 audio bytes.
        s.feed(&[0xAA, 0xBB, 0x00, 0xCC, 0xDD]);
        let mut audio = Vec::new();
        while let Some(piece) = s.next_piece() {
            if let IcyPiece::Audio(a) = piece {
                audio.extend_from_slice(&a);
            }
        }
        assert_eq!(audio, vec![0xAA, 0xBB, 0xCC, 0xDD]);
    }
}
