//! Pipeline glue — wires `PcmSource → OpusFrameEncoder → WallClockPacer →
//! mpsc<OpusFrame>`. The worker task lives until the source EOFs or the
//! frame consumer drops.

use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::encoder::OpusFrameEncoder;
use crate::pacer::WallClockPacer;
use crate::source::{
    AudioSourceSpec, FfmpegSource, IcyRadioSource, PcmSource, SyntheticToneSource, YtDlpSource,
};
use crate::types::{OpusFrame, PipelineConfig, PipelineError, PipelineEvent};
use crate::volume::{VolumeHandle, apply_gain};

/// The handle WS-1 talks to. Drop = cancel.
pub struct AudioPipeline {
    frames_rx: Option<mpsc::Receiver<OpusFrame>>,
    events_tx: broadcast::Sender<PipelineEvent>,
    worker: Option<JoinHandle<()>>,
}

impl AudioPipeline {
    /// Spawn the pipeline. The frame receiver is taken once via
    /// [`take_frames`]; events can be subscribed to repeatedly.
    ///
    /// `volume` is the shared output-gain handle (PURA-351) — the caller
    /// keeps a clone and `set`s it from the REST / chat surfaces; the
    /// worker reads it once per frame and scales the PCM before encode.
    /// Pass [`VolumeHandle::default`] for unity-gain pass-through.
    pub async fn spawn(
        spec: AudioSourceSpec,
        cfg: PipelineConfig,
        volume: VolumeHandle,
    ) -> Result<Self, PipelineError> {
        // PURA-358 — wrap the spec in a `LazySource` instead of resolving
        // it here. Source bring-up (yt-dlp resolution of a YouTube URL is
        // ~5 s) used to run on this `await`; the music bot's connected
        // loop spawns the pipeline inline from its event/command arm, so
        // every `!play` / queue-advance froze the audio-drain arm for the
        // whole resolve — a multi-second mid-song gap on the wire. The
        // `LazySource` defers bring-up to the worker task's first
        // `read_samples`, which already runs off the connected loop, so
        // this call now returns in microseconds.
        let source: Box<dyn PcmSource> = Box::new(LazySource::new(spec, cfg.clone()));
        Self::spawn_with_source(source, cfg, volume)
    }

    /// Construct a pipeline from a caller-built source. Useful for tests
    /// (synthetic source) and for sources WS-1 wants to inject directly.
    /// See [`spawn`](Self::spawn) for the `volume` handle contract.
    pub fn spawn_with_source(
        mut source: Box<dyn PcmSource>,
        cfg: PipelineConfig,
        volume: VolumeHandle,
    ) -> Result<Self, PipelineError> {
        let mut encoder = OpusFrameEncoder::new(&cfg)?;
        let (frames_tx, frames_rx) = mpsc::channel::<OpusFrame>(cfg.frame_buffer);
        let (events_tx, _) = broadcast::channel::<PipelineEvent>(cfg.event_buffer);
        let events_pub = events_tx.clone();

        let prebuffer_target = cfg.prebuffer_frames;
        let worker = tokio::spawn(async move {
            // PURA-330 — anchor for the per-stage latency log: time from
            // worker spawn to the first encoded Opus frame covers the whole
            // source bring-up (yt-dlp + ffmpeg) plus the first encode.
            let worker_t0 = Instant::now();
            let mut first_frame_logged = false;
            // The pacer is anchored on the *first forwarded frame*, not on
            // worker spawn. yt-dlp/ffmpeg startup latency is multi-second;
            // anchoring at spawn stamps every slot before the first frame in
            // the past, so the consumer's wall-clock wait fires instantly and
            // blasts a multi-second catch-up burst before settling into
            // real-time — audible as choppy playback at the start of every
            // track (PURA-314).
            //
            // PURA-329 — the worker also pre-buffers: it holds the first
            // `prebuffer_target` encoded frames before anchoring the pacer, so
            // the paced consumer starts draining against an already-filled
            // channel and a transient producer stall during start-up cannot
            // immediately underrun the wire.
            let mut pacer: Option<WallClockPacer> = None;
            let mut prebuffer: Vec<Bytes> = Vec::with_capacity(prebuffer_target);
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
                    // Source closed before the pre-buffer watermark — flush
                    // whatever we held so a short track still plays.
                    if !prebuffer.is_empty() {
                        let pacer =
                            pacer.get_or_insert_with(|| WallClockPacer::new(Instant::now()));
                        if !flush_prebuffer(&mut prebuffer, pacer, &frames_tx, channels).await {
                            break;
                        }
                    }
                    let _ = events_pub.send(PipelineEvent::EndOfStream);
                    break;
                }
                // Carve off one frame; pad with silence on EOF if short.
                if pcm.len() < samples_per_frame {
                    pcm.resize(samples_per_frame, 0);
                }
                let mut frame_pcm: Vec<i16> = pcm.drain(..samples_per_frame).collect();
                // PURA-351 — apply the operator's output gain to the raw
                // PCM before the Opus encode. Read once per frame so a
                // mid-track slider move is picked up on the next 20 ms
                // boundary; unity gain is a no-op fast path.
                apply_gain(&mut frame_pcm, volume.get());
                let opus = match encoder.encode_frame(&frame_pcm) {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = events_pub.send(PipelineEvent::Warning(format!("encode: {e}")));
                        continue;
                    }
                };
                if !first_frame_logged {
                    first_frame_logged = true;
                    tracing::info!(
                        target: "music_bot_latency",
                        stage = "pipeline_first_frame",
                        elapsed_ms = worker_t0.elapsed().as_millis() as u64,
                        "pipeline encoded first Opus frame (pre-buffering begins)",
                    );
                }
                let drained = eof && pcm.is_empty();
                match &mut pacer {
                    Some(pacer) => {
                        let (index, scheduled_at) = pacer.tick();
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
                    }
                    slot => {
                        // Still pre-buffering: hold the frame. Anchor the
                        // pacer and flush once the watermark is reached, or
                        // immediately if the source drained first (track
                        // shorter than the watermark).
                        prebuffer.push(opus);
                        if prebuffer.len() >= prebuffer_target || drained {
                            tracing::info!(
                                target: "music_bot_latency",
                                stage = "pipeline_prebuffer_full",
                                elapsed_ms = worker_t0.elapsed().as_millis() as u64,
                                frames = prebuffer.len(),
                                "pre-buffer watermark reached — handing frames to consumer",
                            );
                            let pacer = slot.insert(WallClockPacer::new(Instant::now()));
                            if !flush_prebuffer(&mut prebuffer, pacer, &frames_tx, channels).await {
                                break;
                            }
                        }
                    }
                }
                if drained {
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

/// Forward every held pre-buffer frame against the (already-anchored) pacer,
/// stamping each with its drift-free `scheduled_at`. Returns `false` if the
/// frame consumer has gone away — the worker treats that as its cancel
/// signal.
async fn flush_prebuffer(
    prebuffer: &mut Vec<Bytes>,
    pacer: &mut WallClockPacer,
    frames_tx: &mpsc::Sender<OpusFrame>,
    channels: u8,
) -> bool {
    for bytes in prebuffer.drain(..) {
        let (index, scheduled_at) = pacer.tick();
        let frame = OpusFrame {
            bytes,
            index,
            scheduled_at,
            channels,
        };
        if frames_tx.send(frame).await.is_err() {
            return false;
        }
    }
    true
}

/// PURA-358 — a [`PcmSource`] that defers source bring-up (yt-dlp / ffmpeg
/// resolution) to its first [`read_samples`](PcmSource::read_samples) call.
///
/// Bring-up of a YouTube URL takes ~5 s. The music bot's connected loop
/// spawns the pipeline inline from its event/command arm, so running the
/// resolve there froze audio-frame delivery for the whole resolve — the
/// sporadic mid-song crackle PURA-358 chases. The pipeline worker is
/// already a separate task off the connected loop; deferring the resolve
/// into the worker's first `read_samples` moves the stall there.
///
/// A resolve failure surfaces as an `io::Error` from `read_samples`, which
/// the worker already turns into a `Warning` event + a 0-frame finish —
/// the path the bot reports as `last_error` (PURA-314). The spec is kept
/// until the resolve *succeeds*, so a `read_samples` future dropped
/// mid-resolve (consumer teardown) leaves the source uncorrupted, per the
/// [`PcmSource`] cancel-safety contract.
struct LazySource {
    spec: Option<AudioSourceSpec>,
    cfg: PipelineConfig,
    inner: Option<Box<dyn PcmSource>>,
}

impl LazySource {
    fn new(spec: AudioSourceSpec, cfg: PipelineConfig) -> Self {
        Self {
            spec: Some(spec),
            cfg,
            inner: None,
        }
    }
}

#[async_trait]
impl PcmSource for LazySource {
    async fn read_samples(&mut self, buf: &mut [i16]) -> std::io::Result<usize> {
        if self.inner.is_none() {
            let Some(spec) = self.spec.clone() else {
                return Err(std::io::Error::other(
                    "LazySource: source bring-up already failed",
                ));
            };
            let src = build_source(spec, &self.cfg)
                .await
                .map_err(std::io::Error::other)?;
            self.spec = None;
            self.inner = Some(src);
        }
        self.inner.as_mut().unwrap().read_samples(buf).await
    }

    fn try_drain_events(&mut self) -> Vec<PipelineEvent> {
        match self.inner.as_mut() {
            Some(src) => src.try_drain_events(),
            None => Vec::new(),
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
            let src = FfmpegSource::from_input(&input, cfg.channels, None)
                .await
                .map_err(|e| PipelineError::Source(format!("ffmpeg spawn: {e}")))?;
            Ok(Box::new(src))
        }
        // PURA-352 — seek: decode directly from the (already-resolved)
        // input at an offset. No yt-dlp resolution.
        AudioSourceSpec::FfmpegAt { input, start_secs } => {
            let src = FfmpegSource::from_input(&input, cfg.channels, Some(start_secs))
                .await
                .map_err(|e| PipelineError::Source(format!("ffmpeg seek spawn: {e}")))?;
            Ok(Box::new(src))
        }
        AudioSourceSpec::YtDlp { url } => {
            // PURA-359 — try the warm resolver service first: it returns a
            // direct `bestaudio` URL from a process that already imported
            // yt-dlp, so `ffmpeg` consumes the URL directly and the ~2 s
            // per-`!play` yt-dlp process startup (PURA-355) is skipped.
            // Every failure path below degrades to the proven `yt-dlp`
            // subprocess — a broken resolver slows `!play` but never breaks
            // it.
            // THE-932: when the warm resolver returns a video_id (always for
            // searches after two-phase instrumentation), prefer that direct
            // watch URL for the subprocess fallback. This avoids re-running
            // the expensive ytsearch query from scratch when the warm resolver
            // fails or when ffmpeg rejects the resolved URL.
            let mut fallback_url = url.clone();
            if let Some(resolver) = crate::resolver::shared() {
                let t0 = Instant::now();
                // PURA-368 — a `ytsearch<N>:` URL is a `!play yt:` search
                // query (PURA-353); a watch URL is a direct `!play`. Tag the
                // latency log so the two resolve paths can be measured apart.
                let is_search = url.starts_with("ytsearch");
                match resolver.resolve(&url, cfg.yt_cookie_file.as_deref()).await {
                    Ok(track) => {
                        // THE-932: emit per-phase timing alongside the total.
                        for phase in &track.phases {
                            tracing::info!(
                                target: "music_bot_latency",
                                stage = "resolver_phase",
                                phase = %phase.name,
                                phase_ms = phase.ms,
                                search = is_search,
                                "yt-dlp resolver phase",
                            );
                        }
                        tracing::info!(
                            target: "music_bot_latency",
                            stage = "resolver_resolved",
                            elapsed_ms = t0.elapsed().as_millis() as u64,
                            search = is_search,
                            title = track.title.as_deref().unwrap_or(""),
                            "warm yt-dlp resolver returned direct media URL",
                        );
                        match FfmpegSource::from_input(&track.direct_url, cfg.channels, None).await
                        {
                            Ok(src) => return Ok(Box::new(src)),
                            Err(e) => {
                                // Preserve the video_id for the subprocess fallback even
                                // when ffmpeg rejects the direct URL.
                                if let Some(vid) = &track.video_id {
                                    fallback_url = format!("https://www.youtube.com/watch?v={vid}");
                                }
                                tracing::warn!(
                                    error = %e,
                                    "ffmpeg rejected resolver direct URL — \
                                     falling back to yt-dlp subprocess",
                                );
                            }
                        }
                    }
                    Err(e) => tracing::warn!(
                        error = %e,
                        search = is_search,
                        "warm resolver did not resolve — falling back to yt-dlp subprocess",
                    ),
                }
            }
            let src = YtDlpSource::new(&fallback_url, cfg.channels, cfg.yt_cookie_file.as_deref())
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

    /// PURA-358 — `LazySource` must not bring its inner source up until the
    /// first `read_samples`. `try_drain_events` before any read sees no
    /// inner source and returns empty; the first read resolves it and from
    /// then on the source produces samples. This is the property that keeps
    /// the resolve off the connected loop: `AudioPipeline::spawn` only
    /// builds a `LazySource`, never resolves.
    #[tokio::test]
    async fn lazy_source_defers_bring_up_to_first_read() {
        let cfg = PipelineConfig::default();
        let mut lazy = LazySource::new(
            AudioSourceSpec::SyntheticTone {
                hz: 440.0,
                amplitude: 0.5,
                duration_ms: Some(200),
            },
            cfg,
        );
        // Not yet resolved: spec retained, inner absent, no events.
        assert!(lazy.inner.is_none(), "inner unresolved before first read");
        assert!(lazy.spec.is_some(), "spec retained before first read");
        assert!(lazy.try_drain_events().is_empty());

        // First read resolves the inner source and yields samples.
        let mut buf = [0i16; 960];
        let n = lazy.read_samples(&mut buf).await.expect("read");
        assert!(n > 0, "resolved source produces samples");
        assert!(lazy.inner.is_some(), "inner resolved after first read");
        assert!(lazy.spec.is_none(), "spec cleared once resolved");
    }

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
            VolumeHandle::default(),
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
            VolumeHandle::default(),
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

    /// A PCM source that delivers one 20 ms mono frame of silence per
    /// `read_samples` call and, on a chosen call, stalls for a fixed
    /// duration first — a deterministic stand-in for a network / ffmpeg
    /// hiccup mid-stream.
    struct StallingSource {
        reads_remaining: usize,
        read_count: usize,
        stall_on_read: usize,
        stall: std::time::Duration,
    }

    #[async_trait::async_trait]
    impl crate::source::PcmSource for StallingSource {
        async fn read_samples(&mut self, buf: &mut [i16]) -> std::io::Result<usize> {
            if self.reads_remaining == 0 {
                return Ok(0);
            }
            self.read_count += 1;
            if self.read_count == self.stall_on_read {
                tokio::time::sleep(self.stall).await;
            }
            self.reads_remaining -= 1;
            let n = crate::types::SAMPLES_PER_FRAME_MONO.min(buf.len());
            buf[..n].fill(0);
            Ok(n)
        }
    }

    /// PURA-329 regression — a paced 20 ms consumer must not starve when the
    /// producer stalls mid-stream. With the legacy `frame_buffer = 8` / no
    /// pre-buffer config a stall longer than 8 × 20 ms = 160 ms emptied the
    /// frame channel and gapped the wire (audible crackle). The enlarged
    /// `frame_buffer` plus the `prebuffer_frames` watermark give the consumer
    /// a multi-second runway, so a 300 ms stall is absorbed with the channel
    /// never running dry.
    #[tokio::test]
    async fn prebuffer_absorbs_producer_stall_no_underrun() {
        const TOTAL_FRAMES: usize = 70;
        const PREBUFFER: usize = 20;
        const FRAME_BUFFER: usize = 45;
        const STALL_ON_READ: usize = 50;
        let stall = std::time::Duration::from_millis(300);

        let source = StallingSource {
            reads_remaining: TOTAL_FRAMES,
            read_count: 0,
            stall_on_read: STALL_ON_READ,
            stall,
        };
        let cfg = PipelineConfig {
            frame_buffer: FRAME_BUFFER,
            prebuffer_frames: PREBUFFER,
            ..PipelineConfig::default()
        };
        let mut pipeline =
            AudioPipeline::spawn_with_source(Box::new(source), cfg, VolumeHandle::default())
                .expect("spawn");
        let mut frames = pipeline.take_frames();

        // First frame marks playback start; the pre-buffer is full by now.
        let first = frames.recv().await.expect("first frame");
        assert_eq!(first.index, 0, "first frame is index 0");

        // Drain at the real wire cadence — one frame per 20 ms — and measure
        // how long each `recv()` blocks. A buffered frame pops instantly; a
        // non-trivial block means the channel underran.
        let mut max_block = std::time::Duration::ZERO;
        let mut count = 1usize;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let t0 = Instant::now();
            match frames.recv().await {
                Some(_) => {
                    max_block = max_block.max(t0.elapsed());
                    count += 1;
                }
                None => break,
            }
        }

        assert_eq!(count, TOTAL_FRAMES, "every frame delivered");
        assert!(
            max_block < std::time::Duration::from_millis(80),
            "consumer starved for {max_block:?} — frame-buffer underrun: the \
             pre-buffer watermark / enlarged frame_buffer did not absorb the \
             {stall:?} producer stall",
        );

        pipeline.shutdown().await;
    }
}
