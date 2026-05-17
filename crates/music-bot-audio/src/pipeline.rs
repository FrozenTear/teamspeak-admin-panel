//! Pipeline glue — wires `PcmSource → OpusFrameEncoder → WallClockPacer →
//! mpsc<OpusFrame>`. The worker task lives until the source EOFs or the
//! frame consumer drops.

use std::time::Instant;

use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::encoder::OpusFrameEncoder;
use crate::pacer::WallClockPacer;
use crate::source::{
    AudioSourceSpec, FfmpegSource, IcyRadioSource, PcmSource, SyntheticToneSource, YtDlpSource,
};
use crate::types::{OpusFrame, PipelineConfig, PipelineError, PipelineEvent};

/// The handle WS-1 talks to. Drop = cancel.
pub struct AudioPipeline {
    frames_rx: Option<mpsc::Receiver<OpusFrame>>,
    events_tx: broadcast::Sender<PipelineEvent>,
    worker: Option<JoinHandle<()>>,
}

impl AudioPipeline {
    /// Spawn the pipeline. The frame receiver is taken once via
    /// [`take_frames`]; events can be subscribed to repeatedly.
    pub async fn spawn(spec: AudioSourceSpec, cfg: PipelineConfig) -> Result<Self, PipelineError> {
        let source = build_source(spec, &cfg).await?;
        Self::spawn_with_source(source, cfg)
    }

    /// Construct a pipeline from a caller-built source. Useful for tests
    /// (synthetic source) and for sources WS-1 wants to inject directly.
    pub fn spawn_with_source(
        mut source: Box<dyn PcmSource>,
        cfg: PipelineConfig,
    ) -> Result<Self, PipelineError> {
        let mut encoder = OpusFrameEncoder::new(&cfg)?;
        let (frames_tx, frames_rx) = mpsc::channel::<OpusFrame>(cfg.frame_buffer);
        let (events_tx, _) = broadcast::channel::<PipelineEvent>(cfg.event_buffer);
        let events_pub = events_tx.clone();

        let worker = tokio::spawn(async move {
            // The pacer is anchored on the *first emitted frame*, not on
            // worker spawn. yt-dlp/ffmpeg startup latency is multi-second;
            // anchoring at spawn stamps every slot before the first frame in
            // the past, so the consumer's wall-clock wait fires instantly and
            // blasts a multi-second catch-up burst before settling into
            // real-time — audible as choppy playback at the start of every
            // track (PURA-314).
            let mut pacer: Option<WallClockPacer> = None;
            let samples_per_frame = encoder.samples_per_frame();
            // PCM accumulator — holds samples that crossed a `read_samples`
            // boundary. We always emit whole frames; partial leftovers are
            // padded with silence at EOF.
            let mut pcm: Vec<i16> = Vec::with_capacity(samples_per_frame * 2);
            let channels = encoder.channels();
            let mut eof = false;
            loop {
                // Fill `pcm` until we have at least one frame or EOF.
                while pcm.len() < samples_per_frame && !eof {
                    let prev_len = pcm.len();
                    pcm.resize(samples_per_frame * 2, 0);
                    match source.read_samples(&mut pcm[prev_len..]).await {
                        Ok(0) => {
                            pcm.truncate(prev_len);
                            eof = true;
                        }
                        Ok(n) => {
                            pcm.truncate(prev_len + n);
                        }
                        Err(e) => {
                            let _ = events_pub
                                .send(PipelineEvent::Warning(format!("source read: {e}")));
                            pcm.truncate(prev_len);
                            eof = true;
                        }
                    }
                }
                // Forward any out-of-band events.
                for ev in source.try_drain_events() {
                    let _ = events_pub.send(ev);
                }
                if pcm.is_empty() && eof {
                    let _ = events_pub.send(PipelineEvent::EndOfStream);
                    break;
                }
                // Carve off one frame; pad with silence on EOF if short.
                if pcm.len() < samples_per_frame {
                    pcm.resize(samples_per_frame, 0);
                }
                let frame_pcm: Vec<i16> = pcm.drain(..samples_per_frame).collect();
                let opus = match encoder.encode_frame(&frame_pcm) {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = events_pub.send(PipelineEvent::Warning(format!("encode: {e}")));
                        continue;
                    }
                };
                let (index, scheduled_at) = pacer
                    .get_or_insert_with(|| WallClockPacer::new(Instant::now()))
                    .tick();
                let frame = OpusFrame {
                    bytes: opus,
                    index,
                    scheduled_at,
                    channels,
                };
                if frames_tx.send(frame).await.is_err() {
                    // Consumer dropped; we're done.
                    break;
                }
                if eof && pcm.is_empty() {
                    let _ = events_pub.send(PipelineEvent::EndOfStream);
                    break;
                }
            }
            // worker exit drops `source`, which kills any subprocesses.
            drop(source);
        });

        Ok(Self {
            frames_rx: Some(frames_rx),
            events_tx,
            worker: Some(worker),
        })
    }

    /// Returns a freshly-subscribed event receiver. Late subscribers miss
    /// past events.
    pub fn events(&self) -> broadcast::Receiver<PipelineEvent> {
        self.events_tx.subscribe()
    }

    /// Take the frame receiver. Panics if called twice.
    pub fn take_frames(&mut self) -> mpsc::Receiver<OpusFrame> {
        self.frames_rx
            .take()
            .expect("AudioPipeline::take_frames called twice")
    }

    /// Cancel the worker. Returns once the worker has exited.
    pub async fn shutdown(mut self) {
        if let Some(handle) = self.worker.take() {
            handle.abort();
            let _ = handle.await;
        }
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        if let Some(handle) = self.worker.take() {
            handle.abort();
        }
    }
}

async fn build_source(
    spec: AudioSourceSpec,
    cfg: &PipelineConfig,
) -> Result<Box<dyn PcmSource>, PipelineError> {
    match spec {
        AudioSourceSpec::SyntheticTone {
            hz,
            amplitude,
            duration_ms,
        } => Ok(Box::new(SyntheticToneSource::new(
            hz,
            amplitude,
            cfg.channels,
            duration_ms,
        ))),
        AudioSourceSpec::Ffmpeg { input } => {
            let src = FfmpegSource::from_input(&input, cfg.channels)
                .await
                .map_err(|e| PipelineError::Source(format!("ffmpeg spawn: {e}")))?;
            Ok(Box::new(src))
        }
        AudioSourceSpec::YtDlp { url } => {
            let src = YtDlpSource::new(&url, cfg.channels, cfg.yt_cookie_file.as_deref())
                .await
                .map_err(|e| PipelineError::Source(format!("yt-dlp spawn: {e}")))?;
            Ok(Box::new(src))
        }
        AudioSourceSpec::IcyRadio { url } => {
            let src = IcyRadioSource::new(&url, cfg.channels)
                .await
                .map_err(|e| PipelineError::Source(format!("icy radio: {e}")))?;
            Ok(Box::new(src))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pipeline drains the synthetic source and emits valid Opus frames with
    /// drift-free `scheduled_at` indices. Pure logic — no wall-clock waits.
    #[tokio::test]
    async fn synthetic_emits_frames_with_paced_schedule() {
        let cfg = PipelineConfig::default();
        let mut pipeline = AudioPipeline::spawn(
            AudioSourceSpec::SyntheticTone {
                hz: 440.0,
                amplitude: 0.5,
                duration_ms: Some(200),
            },
            cfg,
        )
        .await
        .expect("spawn");
        let mut frames = pipeline.take_frames();
        let mut events = pipeline.events();

        let mut collected: Vec<OpusFrame> = Vec::new();
        while let Some(f) = frames.recv().await {
            collected.push(f);
        }

        // 200 ms at 20 ms cadence = 10 frames.
        assert_eq!(collected.len(), 10, "10 frames for 200 ms");

        // Indices monotonic from 0.
        for (i, f) in collected.iter().enumerate() {
            assert_eq!(f.index as usize, i);
            assert!(!f.bytes.is_empty(), "non-empty Opus packet");
        }
        // scheduled_at offsets are exact (drift-free).
        let start = collected[0].scheduled_at;
        for (i, f) in collected.iter().enumerate() {
            let delta = f.scheduled_at.duration_since(start);
            assert_eq!(
                delta,
                std::time::Duration::from_millis(20) * i as u32,
                "frame {i} scheduled_at drifted",
            );
        }

        // EndOfStream eventually arrives on the broadcast.
        let mut saw_eos = false;
        while let Ok(ev) = events.try_recv() {
            if matches!(ev, PipelineEvent::EndOfStream) {
                saw_eos = true;
            }
        }
        assert!(saw_eos, "EndOfStream should be broadcast at clean EOF");

        pipeline.shutdown().await;
    }

    /// Drop the frame receiver mid-stream → worker cleanly exits, no panic.
    #[tokio::test]
    async fn cancel_on_consumer_drop() {
        let cfg = PipelineConfig::default();
        let mut pipeline = AudioPipeline::spawn(
            AudioSourceSpec::SyntheticTone {
                hz: 440.0,
                amplitude: 0.5,
                duration_ms: None, // infinite
            },
            cfg,
        )
        .await
        .expect("spawn");
        let mut frames = pipeline.take_frames();
        // Drain a couple of frames…
        let _ = frames.recv().await.unwrap();
        let _ = frames.recv().await.unwrap();
        // …then drop. Worker should exit shortly.
        drop(frames);
        // Give it a tick to notice.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        pipeline.shutdown().await;
    }
}
