//! Shared helpers for the music-bot pages — state badge classes, error
//! formatting, audio-source ↔ form-input plumbing.

use ts6_manager_shared::music_bots as wire;

use crate::client::api::ApiError;

/// Map a [`wire::BotState`] to a CSS modifier so the badge styling can
/// stay alongside the existing `data-table` token set without adding a
/// per-state CSS rule today.
pub fn state_badge_class(state: wire::BotState) -> &'static str {
    match state {
        wire::BotState::Disconnected => "bot-badge bot-badge--off",
        wire::BotState::Connecting => "bot-badge bot-badge--pending",
        wire::BotState::Disconnecting => "bot-badge bot-badge--pending",
        wire::BotState::Connected => "bot-badge bot-badge--idle",
        wire::BotState::InChannel => "bot-badge bot-badge--idle",
        wire::BotState::Playing => "bot-badge bot-badge--play",
    }
}

/// Operator-facing label for a bot lifecycle state. Matches the wire
/// vocabulary but humanised.
pub fn state_label(state: wire::BotState) -> &'static str {
    match state {
        wire::BotState::Disconnected => "Disconnected",
        wire::BotState::Connecting => "Connecting…",
        wire::BotState::Connected => "Connected",
        wire::BotState::InChannel => "In channel",
        wire::BotState::Disconnecting => "Disconnecting…",
        wire::BotState::Playing => "Playing",
    }
}

/// One-line summary of an [`wire::AudioSource`] for tables.
pub fn audio_source_summary(source: &wire::AudioSource) -> String {
    match source {
        wire::AudioSource::Url { url } => url.clone(),
        wire::AudioSource::LibraryPath { path } => format!("library:{path}"),
    }
}

/// Compact source label for the now-playing widget and queue rows: the
/// URL host only (so a long query-string URL doesn't blow out the row),
/// or the `library:` path. Falls back to the full summary if a URL has
/// no recognisable host.
pub fn audio_source_host(source: &wire::AudioSource) -> String {
    match source {
        wire::AudioSource::LibraryPath { path } => format!("library:{path}"),
        wire::AudioSource::Url { url } => {
            let after_scheme = url.split("://").nth(1).unwrap_or(url.as_str());
            let host = after_scheme
                .split(['/', '?', '#'])
                .next()
                .unwrap_or(after_scheme);
            let host = host.strip_prefix("www.").unwrap_or(host);
            if host.is_empty() {
                url.clone()
            } else {
                host.to_string()
            }
        }
    }
}

/// Glyph signifying the source type — gives information scent in the
/// now-playing widget without the operator parsing the URL.
pub fn source_glyph(source: &wire::AudioSource) -> &'static str {
    match source {
        wire::AudioSource::Url { .. } => "▶",
        wire::AudioSource::LibraryPath { .. } => "🗀",
    }
}

/// Build an `AudioSource` from a free-form URL string. Anything that
/// doesn't start with `library:` is treated as a URL (yt-dlp resolves
/// most things including direct stream URLs).
pub fn parse_audio_source(raw: &str) -> Option<wire::AudioSource> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(path) = trimmed.strip_prefix("library:") {
        let p = path.trim();
        if p.is_empty() {
            return None;
        }
        return Some(wire::AudioSource::LibraryPath {
            path: p.to_string(),
        });
    }
    Some(wire::AudioSource::Url {
        url: trimmed.to_string(),
    })
}

/// Convert an [`ApiError`] into the operator-facing message banners +
/// toasts use. Mirrors the helper in `clients.rs` so the music-bot
/// pages render errors with the same vocabulary as the rest of the SPA.
pub fn format_error(err: &ApiError) -> String {
    match err {
        ApiError::BadGateway {
            error,
            code,
            details,
        } => {
            let mut s = error.clone();
            if let Some(d) = details.as_deref().filter(|v| !v.is_empty()) {
                s.push_str(": ");
                s.push_str(d);
            }
            if let Some(c) = code {
                s.push_str(&format!(" (code {c})"));
            }
            s
        }
        ApiError::Unauthorized(_) => "Session expired. Sign in again.".into(),
        ApiError::SessionAnonymous => "Loading…".into(),
        ApiError::Client { status, message } => format!("{status}: {message}"),
        ApiError::Server { status, message } => format!("{status}: {message}"),
        ApiError::Transport(m) => format!("Transport error: {m}"),
        ApiError::Deserialise(m) => format!("Unexpected response: {m}"),
        ApiError::UnsupportedTarget => "Action unavailable in this view.".into(),
    }
}

/// Format an optional duration in seconds as `m:ss` for the queue list.
/// `None` returns `"–"` — fixed glyph keeps column widths stable.
pub fn format_duration(secs: Option<u64>) -> String {
    match secs {
        None => "–".into(),
        Some(0) => "0:00".into(),
        Some(s) => {
            let m = s / 60;
            let r = s % 60;
            format!("{m}:{r:02}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_library_prefix_into_library_path_variant() {
        match parse_audio_source("library:lo-fi/track.mp3").unwrap() {
            wire::AudioSource::LibraryPath { path } => assert_eq!(path, "lo-fi/track.mp3"),
            other => panic!("expected LibraryPath, got {other:?}"),
        }
    }

    #[test]
    fn falls_back_to_url_variant_for_anything_else() {
        match parse_audio_source("https://example.com/song.mp3").unwrap() {
            wire::AudioSource::Url { url } => assert_eq!(url, "https://example.com/song.mp3"),
            other => panic!("expected Url, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_or_whitespace_only_input() {
        assert!(parse_audio_source("").is_none());
        assert!(parse_audio_source("   ").is_none());
        assert!(parse_audio_source("library:").is_none());
        assert!(parse_audio_source("library:   ").is_none());
    }

    #[test]
    fn audio_source_host_strips_scheme_path_and_www() {
        let host = |u: &str| audio_source_host(&wire::AudioSource::Url { url: u.into() });
        assert_eq!(host("https://www.youtube.com/watch?v=abc"), "youtube.com");
        assert_eq!(
            host("http://stream.example.com:8000/live"),
            "stream.example.com:8000"
        );
        assert_eq!(host("not-a-url"), "not-a-url");
        assert_eq!(
            audio_source_host(&wire::AudioSource::LibraryPath {
                path: "lo-fi/x.mp3".into()
            }),
            "library:lo-fi/x.mp3"
        );
    }

    #[test]
    fn duration_format_pads_seconds() {
        assert_eq!(format_duration(None), "–");
        assert_eq!(format_duration(Some(0)), "0:00");
        assert_eq!(format_duration(Some(5)), "0:05");
        assert_eq!(format_duration(Some(65)), "1:05");
        assert_eq!(format_duration(Some(3600)), "60:00");
    }

    #[test]
    fn state_badge_class_is_orthogonal_per_state() {
        // Connected + InChannel share an "idle" badge — the bot is on
        // the server but not yet pumping audio. Playing is its own
        // class so the row pops once playback is live.
        assert_eq!(
            state_badge_class(wire::BotState::Connected),
            state_badge_class(wire::BotState::InChannel)
        );
        assert_ne!(
            state_badge_class(wire::BotState::Playing),
            state_badge_class(wire::BotState::Connected)
        );
        assert_ne!(
            state_badge_class(wire::BotState::Disconnected),
            state_badge_class(wire::BotState::Connecting)
        );
    }
}
