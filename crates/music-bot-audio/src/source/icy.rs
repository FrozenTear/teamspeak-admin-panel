//! ICY (Shoutcast / Icecast) radio source.
//!
//! Protocol: client sends `Icy-MetaData: 1`. Server interleaves audio bytes
//! with metadata blocks every `icy-metaint` bytes. A metadata block is one
//! length byte L (actual size is `L * 16`), followed by `L*16` bytes of
//! metadata (`StreamTitle='…';` padded with NULs).
//!
//! We split the stream: audio bytes go to ffmpeg's stdin, metadata bytes are
//! parsed for `StreamTitle` and surfaced as `PipelineEvent::NowPlaying`.

use std::io;

use async_trait::async_trait;
use futures::StreamExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::PcmSource;
use super::ffmpeg::FfmpegSource;
use crate::icy;
use crate::types::PipelineEvent;

pub struct IcyRadioSource {
    inner: FfmpegSource,
    fetcher: Option<JoinHandle<()>>,
    events: mpsc::Receiver<PipelineEvent>,
    pending_warnings: Vec<PipelineEvent>,
}

impl IcyRadioSource {
    pub async fn new(url: &str, channels: u8) -> io::Result<Self> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| io::Error::other(format!("reqwest build: {e}")))?;
        let resp = client
            .get(url)
            .header("Icy-MetaData", "1")
            .header("User-Agent", "music-bot-audio/0.0 (PURA-119)")
            .send()
            .await
            .map_err(|e| io::Error::other(format!("icy GET {url}: {e}")))?;
        let resp = resp
            .error_for_status()
            .map_err(|e| io::Error::other(format!("icy http status: {e}")))?;

        let metaint = resp
            .headers()
            .get("icy-metaint")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok());

        let (inner, mut ffmpeg_stdin) = FfmpegSource::from_stdin(channels).await?;
        let (event_tx, event_rx) = mpsc::channel::<PipelineEvent>(32);
        let url_owned = url.to_string();

        let mut pending_warnings = Vec::new();
        if metaint.is_none() {
            pending_warnings.push(PipelineEvent::Warning(format!(
                "icy stream {url} did not return icy-metaint; ICY metadata disabled"
            )));
        }

        // Spawn a task that splits the body into audio (→ ffmpeg stdin) and
        // metadata (→ NowPlaying events).
        let fetcher = tokio::spawn(async move {
            let mut stream = resp.bytes_stream();
            let mut splitter = icy::IcyStreamSplitter::new(metaint);
            let mut last_title = String::new();
            while let Some(chunk) = stream.next().await {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = event_tx
                            .send(PipelineEvent::Warning(format!("icy stream error: {e}")))
                            .await;
                        break;
                    }
                };
                splitter.feed(&bytes);
                while let Some(piece) = splitter.next_piece() {
                    match piece {
                        icy::IcyPiece::Audio(bs) => {
                            if ffmpeg_stdin.write_all(&bs).await.is_err() {
                                // Consumer dropped — cleanly exit. The reader
                                // side closed; ffmpeg will see EOF.
                                return;
                            }
                        }
                        icy::IcyPiece::Metadata(bs) => {
                            if let Some(title) = icy::parse_stream_title(&bs) {
                                if title != last_title {
                                    last_title = title.clone();
                                    if event_tx
                                        .send(PipelineEvent::NowPlaying {
                                            title,
                                            source: url_owned.clone(),
                                        })
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // Stream ended; close ffmpeg stdin so it knows there's no more
            // audio coming.
            let _ = ffmpeg_stdin.shutdown().await;
        });

        Ok(Self {
            inner,
            fetcher: Some(fetcher),
            events: event_rx,
            pending_warnings,
        })
    }
}

impl Drop for IcyRadioSource {
    fn drop(&mut self) {
        if let Some(handle) = self.fetcher.take() {
            handle.abort();
        }
    }
}

#[async_trait]
impl PcmSource for IcyRadioSource {
    async fn read_samples(&mut self, buf: &mut [i16]) -> io::Result<usize> {
        self.inner.read_samples(buf).await
    }

    fn try_drain_events(&mut self) -> Vec<PipelineEvent> {
        let mut out = std::mem::take(&mut self.pending_warnings);
        // Drain anything the fetcher pushed since last call.
        loop {
            match self.events.try_recv() {
                Ok(ev) => out.push(ev),
                Err(_) => break,
            }
        }
        out.extend(self.inner.try_drain_events());
        out
    }
}
