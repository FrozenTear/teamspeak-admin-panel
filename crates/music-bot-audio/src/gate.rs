//! Playback SSRF gate and library jail.
//!
//! Every user-supplied source reaches ffmpeg, yt-dlp, or the ICY client
//! through [`AudioPipeline::spawn`](crate::AudioPipeline::spawn). That is
//! the choke point for REST play, queue, playlist, library, radio, and
//! TeamSpeak chat `!play` / `!radio` — the route-level check on
//! `POST /api/music-bots/{id}/play` does not see those other writes.
//!
//! Rules match the play-route gate (`gate_play_url` /
//! `confine_library_path` in the manager):
//!
//! * [`ts6_ssrf::is_url_allowed`] decides the host. Plaintext `http` with
//!   no pinned address is refused. `https` with a DNS miss is allowed,
//!   same split as manager webhooks.
//! * A local path is joined to the configured music library root,
//!   canonicalised, and must be a regular file inside that root. The
//!   string ffmpeg opens is that canonical path, not the caller's
//!   original (which may be relative to the process cwd or an absolute
//!   path outside the library).
//!
//! Redirect following, HLS segment re-checks, and ffmpeg `tls_verify`
//! are out of scope here. The gate runs on the URL or path that is
//! about to be opened.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};

use ts6_ssrf::{Resolver, SsrfError, is_url_allowed};

use crate::source::ffmpeg::ffmpeg_input_is_remote_http;
use crate::source::{AudioSourceSpec, normalize_radio_url};

static MUSIC_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Install the process music-library root. The music unit calls this
/// with the `MUSIC_DIR` it was configured with (default included) so
/// playback cannot ignore that root. A later call is ignored.
pub fn install_music_dir(path: PathBuf) {
    if path.as_os_str().is_empty() {
        return;
    }
    if MUSIC_DIR.set(path).is_err() {
        tracing::debug!("MUSIC_DIR jail root already installed");
    }
}

/// The root installed by [`install_music_dir`], if any.
pub fn installed_music_dir() -> Option<PathBuf> {
    MUSIC_DIR.get().cloned()
}

#[derive(Debug, thiserror::Error)]
pub enum GateError {
    #[error("URL rejected by SSRF policy: {0}")]
    Ssrf(SsrfError),
    #[error("plaintext HTTP URL has no pinned address")]
    UnpinnedHttp,
    #[error("MUSIC_DIR is not configured")]
    MusicDirMissing,
    #[error("{0}")]
    Library(&'static str),
}

/// Same allow rule as the manager play route.
///
/// Returns the URL that passed the check. `icy` / `icecast` /
/// `shoutcast` are rewritten to `http` first, because that is the URL
/// the ICY client fetches. Other URLs are returned trimmed, not
/// re-serialised, so this gate does not paper over parser differences.
pub async fn allow_playback_url(raw: &str, resolver: &dyn Resolver) -> Result<String, GateError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(GateError::Ssrf(SsrfError::InvalidUrlFormat));
    }
    if is_ytsearch(trimmed) {
        return Ok(trimmed.to_string());
    }
    let fetched = normalize_radio_url(trimmed);
    allow_remote_url(&fetched, resolver).await?;
    Ok(fetched)
}

async fn allow_remote_url(raw: &str, resolver: &dyn Resolver) -> Result<(), GateError> {
    let target = is_url_allowed(raw, resolver)
        .await
        .map_err(GateError::Ssrf)?;
    if target.url.scheme() == "http" && target.resolved_ip.is_none() {
        return Err(GateError::UnpinnedHttp);
    }
    Ok(())
}

/// Canonicalise `raw` and require a regular file inside `music_dir`.
///
/// Relative paths are joined onto the root. Absolute paths are accepted
/// only when they canonicalise inside the root — the play route forwards
/// the canonical path it already jailed, and a stored absolute escape
/// (`/etc/passwd`, `/dev/urandom`) must still be rejected here.
pub fn confine_library_path(music_dir: &Path, raw: &str) -> Result<PathBuf, GateError> {
    if raw.is_empty() || raw.contains('\0') {
        return Err(GateError::Library("library path is empty or contains NUL"));
    }
    let raw_path = Path::new(raw);
    if raw.contains('\\') || (!raw_path.is_absolute() && raw.contains(':')) {
        return Err(GateError::Library(
            "library path must be a relative path under MUSIC_DIR",
        ));
    }
    if !raw_path.is_absolute()
        && raw_path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(GateError::Library("library path escapes MUSIC_DIR"));
    }
    let root = music_dir
        .canonicalize()
        .map_err(|_| GateError::Library("MUSIC_DIR is not available"))?;
    let candidate = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        root.join(raw_path)
    };
    let canon = candidate
        .canonicalize()
        .map_err(|_| GateError::Library("library path is not a file under MUSIC_DIR"))?;
    if !canon.starts_with(&root) || !canon.is_file() {
        return Err(GateError::Library("library path escapes MUSIC_DIR"));
    }
    Ok(canon)
}

/// Rewrite `spec` so the string a source is about to open has passed
/// the SSRF check or the library jail.
pub async fn gate_playback_spec(
    spec: AudioSourceSpec,
    music_dir: Option<&Path>,
    resolver: &dyn Resolver,
) -> Result<AudioSourceSpec, GateError> {
    match spec {
        AudioSourceSpec::SyntheticTone { .. } => Ok(spec),
        AudioSourceSpec::Ffmpeg { input } => {
            let input = gate_ffmpeg_input(&input, music_dir, resolver).await?;
            Ok(AudioSourceSpec::Ffmpeg { input })
        }
        AudioSourceSpec::FfmpegAt { input, start_secs } => {
            let input = gate_ffmpeg_input(&input, music_dir, resolver).await?;
            Ok(AudioSourceSpec::FfmpegAt { input, start_secs })
        }
        AudioSourceSpec::YtDlp { url } => {
            let url = allow_playback_url(&url, resolver).await?;
            Ok(AudioSourceSpec::YtDlp { url })
        }
        AudioSourceSpec::IcyRadio { url } => {
            let normalized = normalize_radio_url(url.trim());
            allow_remote_url(&normalized, resolver).await?;
            Ok(AudioSourceSpec::IcyRadio { url: normalized })
        }
    }
}

async fn gate_ffmpeg_input(
    input: &str,
    music_dir: Option<&Path>,
    resolver: &dyn Resolver,
) -> Result<String, GateError> {
    let trimmed = input.trim();
    if ffmpeg_input_is_remote_http(trimmed) {
        return allow_playback_url(trimmed, resolver).await;
    }
    let root = music_dir.ok_or(GateError::MusicDirMissing)?;
    let canon = confine_library_path(root, trimmed)?;
    canon
        .to_str()
        .map(str::to_string)
        .ok_or(GateError::Library("library path is not valid UTF-8"))
}

fn is_ytsearch(url: &str) -> bool {
    url.to_ascii_lowercase().starts_with("ytsearch")
}

pub(crate) fn process_resolver() -> &'static dyn Resolver {
    static RESOLVER: OnceLock<Arc<dyn Resolver>> = OnceLock::new();
    RESOLVER
        .get_or_init(|| -> Arc<dyn Resolver> {
            match ts6_ssrf::HickoryResolver::from_system() {
                Ok(resolver) => Arc::new(resolver),
                Err(err) => {
                    tracing::error!(
                        error = %err,
                        "SSRF resolver failed to start; plaintext HTTP playback will fail closed"
                    );
                    Arc::new(FailClosedResolver)
                }
            }
        })
        .as_ref()
}

/// DNS is unavailable. Name lookups fail, so `is_url_allowed` returns
/// `resolved_ip: None` and plaintext HTTP is refused. IP literals are
/// still range-checked without DNS.
struct FailClosedResolver;

#[async_trait::async_trait]
impl Resolver for FailClosedResolver {
    async fn resolve(&self, host: &str) -> Result<Vec<std::net::IpAddr>, ts6_ssrf::ResolveError> {
        Err(ts6_ssrf::ResolveError::Other(format!(
            "ssrf resolver unavailable for {host}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::net::IpAddr;
    use std::sync::atomic::{AtomicU64, Ordering};

    use ts6_ssrf::MockResolver;

    fn scratch() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ts6-playback-jail-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn public_resolver() -> MockResolver {
        MockResolver::new().with("example.com", vec![IpAddr::from([203, 0, 113, 10])])
    }

    #[tokio::test]
    async fn blocks_loopback_metadata_and_unpinned_http() {
        let resolver = MockResolver::new();
        for url in [
            "http://127.0.0.1/latest/meta-data",
            "http://169.254.169.254/latest/meta-data",
            "http://[::1]/",
            "http://metadata.google.internal/computeMetadata/v1/",
            "http://metadata.google.internal./",
        ] {
            let err = allow_playback_url(url, &resolver).await.unwrap_err();
            assert!(
                matches!(err, GateError::Ssrf(_)),
                "{url} should be SSRF-blocked, got {err}"
            );
        }
        let err = allow_playback_url("http://missing.example/a.mp3", &resolver)
            .await
            .unwrap_err();
        assert!(
            matches!(err, GateError::UnpinnedHttp),
            "unpinned plaintext HTTP must fail closed, got {err}"
        );
    }

    #[tokio::test]
    async fn allows_public_url_and_ytsearch() {
        let resolver = public_resolver();
        let https = allow_playback_url("https://example.com/a.mp3", &resolver)
            .await
            .unwrap();
        assert_eq!(https, "https://example.com/a.mp3");
        let http = allow_playback_url("http://203.0.113.10/a.mp3", &resolver)
            .await
            .unwrap();
        assert_eq!(http, "http://203.0.113.10/a.mp3");
        // HTTPS DNS miss matches the play route: TLS binds the name.
        assert!(
            allow_playback_url("https://missing.example/a.mp3", &MockResolver::new())
                .await
                .is_ok()
        );
        let search = allow_playback_url("ytsearch1:never gonna", &MockResolver::new())
            .await
            .unwrap();
        assert_eq!(search, "ytsearch1:never gonna");

        let spec = gate_playback_spec(
            AudioSourceSpec::YtDlp {
                url: "https://example.com/watch".into(),
            },
            None,
            &resolver,
        )
        .await
        .unwrap();
        assert!(matches!(spec, AudioSourceSpec::YtDlp { .. }));

        let err = gate_playback_spec(
            AudioSourceSpec::IcyRadio {
                url: "icy://127.0.0.1/live".into(),
            },
            None,
            &MockResolver::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, GateError::Ssrf(_)), "{err}");

        let err = gate_playback_spec(
            AudioSourceSpec::YtDlp {
                url: "file:///etc/passwd".into(),
            },
            None,
            &MockResolver::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, GateError::Ssrf(_)), "{err}");
    }

    #[test]
    fn library_path_happy_and_escape() {
        let root = scratch();
        fs::create_dir_all(root.join("a")).unwrap();
        fs::write(root.join("a/b.mp3"), b"x").unwrap();
        let got = confine_library_path(&root, "a/b.mp3").unwrap();
        let expected = root.canonicalize().unwrap().join("a/b.mp3");
        assert_eq!(got, expected);
        // The play route forwards this canonical absolute path. Playback
        // must accept it, still inside the root.
        let again = confine_library_path(&root, got.to_str().unwrap()).unwrap();
        assert_eq!(again, expected);

        assert!(confine_library_path(&root, "../ok.mp3").is_err());
        assert!(confine_library_path(&root, "/etc/passwd").is_err());
        assert!(confine_library_path(&root, "http://127.0.0.1/a.mp3").is_err());
        assert!(confine_library_path(&root, "concat:ok.mp3").is_err());
        assert!(confine_library_path(&root, "/dev/urandom").is_err());

        let outside = scratch();
        fs::write(outside.join("secret.mp3"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.mp3"), root.join("link.mp3")).unwrap();
        assert!(
            confine_library_path(&root, "link.mp3").is_err(),
            "symlink out of MUSIC_DIR must be rejected"
        );
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }

    #[tokio::test]
    async fn spec_rewrites_library_path_and_spawn_rejects_metadata() {
        let root = scratch();
        fs::write(root.join("ok.mp3"), b"x").unwrap();
        let spec = gate_playback_spec(
            AudioSourceSpec::Ffmpeg {
                input: "ok.mp3".into(),
            },
            Some(&root),
            &MockResolver::new(),
        )
        .await
        .unwrap();
        match spec {
            AudioSourceSpec::Ffmpeg { input } => {
                assert_eq!(
                    input,
                    root.canonicalize()
                        .unwrap()
                        .join("ok.mp3")
                        .to_str()
                        .unwrap()
                );
            }
            other => panic!("expected ffmpeg spec, got {other:?}"),
        }
        assert!(
            gate_playback_spec(
                AudioSourceSpec::Ffmpeg {
                    input: "../ok.mp3".into(),
                },
                Some(&root),
                &MockResolver::new(),
            )
            .await
            .is_err()
        );
        assert!(matches!(
            gate_playback_spec(
                AudioSourceSpec::Ffmpeg {
                    input: "rel.mp3".into(),
                },
                None,
                &MockResolver::new(),
            )
            .await
            .unwrap_err(),
            GateError::MusicDirMissing
        ));

        let err = match crate::AudioPipeline::spawn(
            AudioSourceSpec::Ffmpeg {
                input: "http://169.254.169.254/latest/meta-data".into(),
            },
            crate::types::PipelineConfig::default(),
        )
        .await
        {
            Err(err) => err,
            Ok(_) => panic!("metadata URL must be rejected before ffmpeg"),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("SSRF") || msg.contains("not allowed"),
            "pipeline spawn must reject metadata URLs before ffmpeg, got {msg}"
        );

        let err = match crate::AudioPipeline::spawn(
            AudioSourceSpec::YtDlp {
                url: "http://127.0.0.1:3002/v1/bots".into(),
            },
            crate::types::PipelineConfig::default(),
        )
        .await
        {
            Err(err) => err,
            Ok(_) => panic!("loopback URL must be rejected before yt-dlp"),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("SSRF"),
            "yt-dlp spawn must reject loopback, got {msg}"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
