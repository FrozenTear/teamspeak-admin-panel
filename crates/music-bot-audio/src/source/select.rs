//! Choose a native source for a play URL.
//!
//! Icecast / Shoutcast and direct media (HLS `.m3u8`, audio/video
//! files) used to fall through [`super::AudioSourceSpec::YtDlp`] and
//! a background `yt-dlp -g` ([`crate::resolve::resolve_direct_url`]).
//! yt-dlp is the extractor path (YouTube and the other sites below).
//! Live radio is not seekable, so it must not pay that resolve.

/// Which pipeline source a user-supplied URL should open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackRoute {
    /// Extractor site or `ytsearch…:` query. Warm resolver, then yt-dlp.
    YtDlp,
    /// Shoutcast / Icecast. [`super::IcyRadioSource`]. Not seekable.
    IcyRadio,
    /// HLS playlist or a direct media URL ffmpeg can open as `-i`.
    /// Already a media URL — do not run `yt-dlp -g`.
    Ffmpeg,
}

impl PlaybackRoute {
    /// `yt-dlp -g` is only meaningful for extractor pages. Live radio
    /// is not seekable; a direct file / HLS URL is already the ffmpeg
    /// input.
    pub fn skips_ytdlp_resolve(self) -> bool {
        !matches!(self, Self::YtDlp)
    }
}

/// Classify `url` for [`super::AudioSourceSpec`] selection.
///
/// Order:
/// 1. `ytsearch…:` → [`PlaybackRoute::YtDlp`]
/// 2. known extractor host → [`PlaybackRoute::YtDlp`]
/// 3. `.m3u8` / `.m3u` / `.pls` → [`PlaybackRoute::Ffmpeg`]
/// 4. Icecast / Shoutcast signals → [`PlaybackRoute::IcyRadio`]
/// 5. direct media extension → [`PlaybackRoute::Ffmpeg`]
/// 6. anything else (unknown page) → [`PlaybackRoute::YtDlp`]
pub fn classify_playback_url(url: &str) -> PlaybackRoute {
    let trimmed = url.trim();
    if is_ytsearch(trimmed) {
        return PlaybackRoute::YtDlp;
    }
    let Some(parts) = parse_url(trimmed) else {
        return PlaybackRoute::YtDlp;
    };
    if is_extractor_host(&parts.host) {
        return PlaybackRoute::YtDlp;
    }
    if is_playlist_ext(parts.extension()) {
        return PlaybackRoute::Ffmpeg;
    }
    if parts.is_icecast_or_shoutcast() {
        return PlaybackRoute::IcyRadio;
    }
    if is_direct_media_ext(parts.extension()) {
        return PlaybackRoute::Ffmpeg;
    }
    PlaybackRoute::YtDlp
}

/// Rewrite `icy://` / `icecast://` / `shoutcast://` to `http://` so the
/// ICY fetcher (reqwest) can GET it. Other URLs are returned trimmed.
pub fn normalize_radio_url(url: &str) -> String {
    let trimmed = url.trim();
    for prefix in ["icy://", "icecast://", "shoutcast://"] {
        if trimmed.len() >= prefix.len() && trimmed[..prefix.len()].eq_ignore_ascii_case(prefix) {
            return format!("http://{}", &trimmed[prefix.len()..]);
        }
    }
    trimmed.to_string()
}

fn is_ytsearch(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("ytsearch")
}

/// Hosts whose pages need an extractor. Matched on the registrable
/// suffix so `m.youtube.com` hits and `notyoutube.com` does not.
const EXTRACTOR_HOSTS: &[&str] = &[
    "youtube.com",
    "youtu.be",
    "youtube-nocookie.com",
    "soundcloud.com",
    "snd.sc",
    "twitch.tv",
    "vimeo.com",
    "bandcamp.com",
    "mixcloud.com",
    "dailymotion.com",
    "dai.ly",
    "tiktok.com",
    "nicovideo.jp",
    "nico.ms",
    "bilibili.com",
    "b23.tv",
    "facebook.com",
    "fb.watch",
    "fb.com",
    "instagram.com",
    "twitter.com",
    "x.com",
    "reddit.com",
    "streamable.com",
];

const PLAYLIST_EXTS: &[&str] = &["m3u8", "m3u", "pls"];

const DIRECT_MEDIA_EXTS: &[&str] = &[
    "mp3", "aac", "ogg", "opus", "flac", "wav", "m4a", "mp4", "webm", "mkv", "ts", "mpga", "wma",
    "aif", "aiff", "oga", "m4b", "mp2", "caf", "ac3",
];

struct UrlParts {
    scheme: String,
    host: String,
    port: Option<u16>,
    path: String,
}

impl UrlParts {
    fn extension(&self) -> Option<&str> {
        let seg = self.path.rsplit('/').next().unwrap_or(self.path.as_str());
        let ext = seg.rsplit_once('.')?.1;
        if ext.is_empty() || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        Some(ext)
    }

    fn is_icecast_or_shoutcast(&self) -> bool {
        matches!(self.scheme.as_str(), "icy" | "icecast" | "shoutcast")
            || host_marks_radio(&self.host)
            || matches!(self.port, Some(8000) | Some(8001))
            || self.path.contains(';')
    }
}

fn is_playlist_ext(ext: Option<&str>) -> bool {
    ext.is_some_and(|ext| {
        PLAYLIST_EXTS
            .iter()
            .any(|known| ext.eq_ignore_ascii_case(known))
    })
}

fn is_direct_media_ext(ext: Option<&str>) -> bool {
    ext.is_some_and(|ext| {
        DIRECT_MEDIA_EXTS
            .iter()
            .any(|known| ext.eq_ignore_ascii_case(known))
    })
}

fn is_extractor_host(host: &str) -> bool {
    EXTRACTOR_HOSTS.iter().any(|root| host_is(host, root))
}

fn host_is(host: &str, root: &str) -> bool {
    host == root
        || host
            .strip_suffix(root)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

/// `ice1.somafm.com`, a host label `icecast` / `shoutcast`.
fn host_marks_radio(host: &str) -> bool {
    if host.contains("icecast") || host.contains("shoutcast") {
        return true;
    }
    let first = host.split('.').next().unwrap_or("");
    let Some(rest) = first.strip_prefix("ice") else {
        return false;
    };
    rest.is_empty() || rest.chars().all(|c| c.is_ascii_digit())
}

/// Parse the bits classification needs. `None` when `url` is not an
/// `http` / `https` / ICY-family URL (callers then keep the yt-dlp path).
fn parse_url(url: &str) -> Option<UrlParts> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if !matches!(
        scheme.as_str(),
        "http" | "https" | "icy" | "icecast" | "shoutcast"
    ) {
        return None;
    }
    let rest_no_frag = rest.split('#').next().unwrap_or(rest);
    let (before_query, _query) = match rest_no_frag.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (rest_no_frag, None),
    };
    let (authority, path) = match before_query.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (before_query, String::new()),
    };
    let authority = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    let (host, port) = split_host_port(authority)?;
    if host.is_empty() {
        return None;
    }
    Some(UrlParts {
        scheme,
        host: host.to_ascii_lowercase(),
        port,
        path,
    })
}

fn split_host_port(authority: &str) -> Option<(&str, Option<u16>)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(raw) if !raw.is_empty() => raw.parse().ok(),
            _ => None,
        };
        return Some((host, port));
    }
    // `host:port` — only when the tail is all digits, so a bare host is
    // not split on an unrelated colon (IPv6 is the bracket form above).
    if let Some((host, raw_port)) = authority.rsplit_once(':')
        && !host.is_empty()
        && raw_port.chars().all(|c| c.is_ascii_digit())
    {
        return Some((host, raw_port.parse().ok()));
    }
    Some((authority, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extractor_sites_and_ytsearch_stay_on_ytdlp() {
        assert_eq!(
            classify_playback_url("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            PlaybackRoute::YtDlp
        );
        assert_eq!(
            classify_playback_url("https://youtu.be/dQw4w9WgXcQ"),
            PlaybackRoute::YtDlp
        );
        assert_eq!(
            classify_playback_url("https://music.youtube.com/watch?v=abc"),
            PlaybackRoute::YtDlp
        );
        assert_eq!(
            classify_playback_url("https://soundcloud.com/artist/track"),
            PlaybackRoute::YtDlp
        );
        assert_eq!(
            classify_playback_url("https://www.twitch.tv/videos/1"),
            PlaybackRoute::YtDlp
        );
        assert_eq!(
            classify_playback_url("https://artist.bandcamp.com/track/song"),
            PlaybackRoute::YtDlp
        );
        assert_eq!(
            classify_playback_url("ytsearch1:never gonna give you up"),
            PlaybackRoute::YtDlp
        );
        assert_eq!(
            classify_playback_url("YTSEARCH5:something"),
            PlaybackRoute::YtDlp
        );
        // Suffix must be a dot-boundary, not a substring of the label.
        // `notyoutube.com` is not an extractor; it stays on yt-dlp only
        // because it is an unknown page (rule 6), not because the host matched.
        assert!(!is_extractor_host("notyoutube.com"));
        assert!(is_extractor_host("youtube.com"));
        assert!(is_extractor_host("m.youtube.com"));
        assert_eq!(
            classify_playback_url("https://notyoutube.com/watch"),
            PlaybackRoute::YtDlp
        );
        assert!(!classify_playback_url("https://youtu.be/abc").skips_ytdlp_resolve());
    }

    #[test]
    fn icecast_and_shoutcast_use_icy_radio() {
        assert_eq!(
            classify_playback_url("https://ice1.somafm.com/groovesalad-128-mp3"),
            PlaybackRoute::IcyRadio
        );
        assert_eq!(
            classify_playback_url("http://radio.example.com:8000/stream"),
            PlaybackRoute::IcyRadio
        );
        assert_eq!(
            classify_playback_url("http://example.com:8000/live.mp3"),
            PlaybackRoute::IcyRadio
        );
        assert_eq!(
            classify_playback_url("http://example.com:8001/;"),
            PlaybackRoute::IcyRadio
        );
        assert_eq!(
            classify_playback_url("http://shoutcast.example.com/foo"),
            PlaybackRoute::IcyRadio
        );
        assert_eq!(
            classify_playback_url("icy://stream.example.com/live"),
            PlaybackRoute::IcyRadio
        );
        assert_eq!(
            classify_playback_url("http://user:pass@ice1.example.com/mount"),
            PlaybackRoute::IcyRadio
        );
        assert_eq!(
            classify_playback_url("http://[2001:db8::1]:8000/stream"),
            PlaybackRoute::IcyRadio
        );
        assert!(
            classify_playback_url("https://ice1.somafm.com/groovesalad-128-mp3")
                .skips_ytdlp_resolve()
        );
    }

    #[test]
    fn hls_and_direct_media_use_ffmpeg() {
        assert_eq!(
            classify_playback_url("https://cdn.example.com/live/index.m3u8"),
            PlaybackRoute::Ffmpeg
        );
        assert_eq!(
            classify_playback_url("https://cdn.example.com/live/index.m3u8?token=abc"),
            PlaybackRoute::Ffmpeg
        );
        assert_eq!(
            classify_playback_url("https://example.com/x.mp3"),
            PlaybackRoute::Ffmpeg
        );
        assert_eq!(
            classify_playback_url("https://example.com/a/b.OPUS"),
            PlaybackRoute::Ffmpeg
        );
        // HLS wins over an Icecast-looking host: ffmpeg opens the playlist.
        assert_eq!(
            classify_playback_url("https://ice1.somafm.com/playlist.m3u8"),
            PlaybackRoute::Ffmpeg
        );
        assert_eq!(
            classify_playback_url("https://example.com/listen.pls"),
            PlaybackRoute::Ffmpeg
        );
        assert!(classify_playback_url("https://cdn.example.com/a.m3u8").skips_ytdlp_resolve());
    }

    #[test]
    fn radio_schemes_normalize_to_http() {
        assert_eq!(
            normalize_radio_url("icy://stream.example.com/live"),
            "http://stream.example.com/live"
        );
        assert_eq!(
            normalize_radio_url("ICECAST://Host:8000/mount"),
            "http://Host:8000/mount"
        );
        assert_eq!(
            normalize_radio_url("https://ice1.somafm.com/groovesalad-128-mp3"),
            "https://ice1.somafm.com/groovesalad-128-mp3"
        );
    }

    #[test]
    fn unknown_page_stays_on_ytdlp() {
        assert_eq!(
            classify_playback_url("https://example.com/some-page"),
            PlaybackRoute::YtDlp
        );
    }
}
