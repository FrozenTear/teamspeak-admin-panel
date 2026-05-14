//! `VideoPlayer` — Dioxus component that subscribes to a `ts6-media-sidecar`
//! source over WebTransport (moq-lite-04), decodes VP8 to a `<canvas>` and
//! plays Opus through the page's `AudioContext` (PURA-143 WS-5).
//!
//! Ported from the WS-0 reference player at `moq-spike/player/player.js`.
//! The wire format details (QUIC varint, SUBSCRIBE frame, GROUP stream
//! framing) live in [ADR-0007](../../../../../../docs/adr/0007-moq-flavor-and-draft-pin.md).
//!
//! ## Hooks
//!
//! The session loop is owned by a [`SessionGuard`] stashed in `use_hook`.
//! When the component unmounts the guard drops, flips the shared stop flag,
//! and closes the WebTransport — same pattern as `music_bots/detail.rs`
//! parking an `EventSource` (PURA-124).
//!
//! `use_effect` is **not** used to drive side effects here: per PURA-132 a
//! `read()`+`set` of the same signal inside an effect deadlocks the headless
//! probe. `use_hook` fires once per mount, isolates the spawn from the
//! reactive graph, and lets the loop write `state` via `set` without
//! re-arming the effect.

#![allow(dead_code)]

use dioxus::prelude::*;

/// High-level connection state surfaced to the in-page status line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlayerState {
    /// Component mounted but `autoplay=false` — waiting for an explicit
    /// "start" action. The component does not render a start button yet
    /// (WS-7 owns operator chrome); the dev route always passes
    /// `autoplay=true`.
    Idle,
    /// `window.WebTransport` is missing — Safari < 18 / older iOS. The
    /// surface renders the fallback copy instead of the canvas.
    Unsupported,
    /// Awaiting `WebTransport.ready` + `SUBSCRIBE_OK`.
    Connecting,
    /// At least one decoded frame has been painted to the canvas.
    Playing,
    /// Session loop exited cleanly (stop flag set or stream EOF).
    Stopped,
    /// Session loop exited with an error. The string is surfaced verbatim.
    Error(String),
}

impl PlayerState {
    /// Human-readable status line. Stable enough for an aria-live region.
    pub fn describe(&self) -> String {
        match self {
            PlayerState::Idle => "Waiting…".into(),
            PlayerState::Unsupported => "WebTransport not supported".into(),
            PlayerState::Connecting => "Connecting…".into(),
            PlayerState::Playing => "Playing".into(),
            PlayerState::Stopped => "Stopped".into(),
            PlayerState::Error(msg) => format!("Error: {msg}"),
        }
    }
}

#[component]
pub fn VideoPlayer(
    relay_url: String,
    namespace: String,
    #[props(default = false)] autoplay: bool,
    /// Start muted — public widget viewer (PURA-146 WS-8) needs this so
    /// browser autoplay policy doesn't block the canvas paint. The audio
    /// pipeline is configured at mount time, so flipping `muted` after
    /// the fact only takes effect on the next mount. The public viewer
    /// remounts the component when the user taps to unmute.
    #[props(default = false)]
    muted: bool,
) -> Element {
    let state: Signal<PlayerState> = use_signal(|| PlayerState::Idle);

    // Stable per-mount canvas id so the wasm session loop can grab the
    // element via `getElementById` once the first render has flushed to the
    // DOM. A Math.random()-derived suffix avoids collisions on routes that
    // render multiple players (WS-7 mosaic view).
    let canvas_id: String = use_hook(|| {
        #[cfg(target_arch = "wasm32")]
        {
            let r = (js_sys::Math::random() * 0xffff_ffffu32 as f64) as u32;
            format!("ts6-video-player-{r:08x}")
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            "ts6-video-player-ssr".to_string()
        }
    });

    #[cfg(target_arch = "wasm32")]
    {
        // One-shot mount hook. The guard returned here is stored across
        // renders by `use_hook` and dropped only when the component
        // unmounts (route change, parent re-render that removes us).
        let canvas_id = canvas_id.clone();
        let relay_url = relay_url.clone();
        let namespace = namespace.clone();
        let state_for_hook = state;
        let _guard = use_hook(move || {
            wt::start(
                relay_url,
                namespace,
                canvas_id,
                autoplay,
                muted,
                state_for_hook,
            )
        });
    }

    let _ = autoplay; // silence unused on non-wasm
    let _ = muted;

    let status_text = state.read().describe();
    let unsupported = matches!(*state.read(), PlayerState::Unsupported);

    rsx! {
        div { class: "video-player",
            if unsupported {
                div {
                    class: "video-player__fallback",
                    role: "status",
                    "aria-live": "polite",
                    "This browser does not support WebTransport. Use Chrome/Edge/Firefox 117+."
                }
            } else {
                canvas {
                    id: "{canvas_id}",
                    width: "1280",
                    height: "720",
                    style: "width: 100%; max-width: 1280px; aspect-ratio: 16/9; background: #000; display: block;",
                    "aria-label": "video output",
                }
                div {
                    class: "video-player__status",
                    role: "status",
                    "aria-live": "polite",
                    style: "color: #8a929c; font-size: 12px; font-family: ui-monospace, monospace; padding: 4px 0;",
                    "{status_text}"
                }
            }
        }
    }
}

// =============================================================================
// WASM session loop — WebTransport + moq-lite-04 + VP8/Opus → canvas/audio
// =============================================================================
//
// Everything below is `cfg(target_arch = "wasm32")` because:
//   * the SSR + native test build must stay free of `web-sys::WebTransport`
//     types, and
//   * `wasm_bindgen_futures::spawn_local` does not exist on native.
//
// A non-wasm stub keeps the [`use_hook`] return type unifiable at the call
// site (it is `()` on the SSR build).

#[cfg(target_arch = "wasm32")]
mod wt {
    use super::PlayerState;
    use dioxus::prelude::{ReadableExt, Signal, WritableExt};
    use js_sys::{Array, Float32Array, Reflect, Uint8Array};
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use wasm_bindgen::{JsCast, JsValue, closure::Closure};
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{
        AudioBuffer, AudioBufferSourceNode, AudioContext, AudioData, AudioDataCopyToOptions,
        AudioDecoder, AudioDecoderConfig, AudioDecoderInit, AudioSampleFormat,
        CanvasRenderingContext2d, EncodedAudioChunk, EncodedAudioChunkInit, EncodedAudioChunkType,
        EncodedVideoChunk, EncodedVideoChunkInit, EncodedVideoChunkType, HtmlCanvasElement,
        ReadableStream, ReadableStreamDefaultReader, VideoDecoder, VideoDecoderConfig,
        VideoDecoderInit, VideoFrame, WebTransport, WebTransportBidirectionalStream,
        WebTransportHash, WebTransportOptions, WritableStreamDefaultWriter,
    };

    // Subscribe ids — one per track in this broadcast. The values are
    // local to the writer (the relay echoes them in GROUP messages so the
    // subscriber can dispatch frames to the right decoder).
    const SUBSCRIBE_VIDEO: u64 = 0;
    const SUBSCRIBE_AUDIO: u64 = 1;

    // moq-lite-04 stream-type prefix for SUBSCRIBE (bidi) and GROUP (uni).
    const STREAM_SUBSCRIBE: u64 = 0x02;
    const STREAM_GROUP: u64 = 0x00;

    /// `RAII` wrapper stashed in [`use_hook`]. Drop flips the stop flag
    /// and closes the live WebTransport handle, freeing the QUIC session
    /// as soon as Dioxus tears down the component. Wrapped in [`Rc`] at
    /// the call site so the `Clone` bound on `use_hook` is satisfied
    /// (only the final `Rc` drop runs the destructor).
    pub struct SessionGuard {
        stop: Rc<Cell<bool>>,
        wt: Rc<RefCell<Option<WebTransport>>>,
    }

    impl Drop for SessionGuard {
        fn drop(&mut self) {
            self.stop.set(true);
            if let Some(wt) = self.wt.borrow_mut().take() {
                wt.close();
            }
        }
    }

    /// Public entry called from the component's `use_hook`. Spawns the
    /// session loop on `wasm_bindgen_futures::spawn_local` and returns a
    /// guard that owns the teardown.
    pub fn start(
        relay_url: String,
        namespace: String,
        canvas_id: String,
        autoplay: bool,
        muted: bool,
        mut state: Signal<PlayerState>,
    ) -> Rc<SessionGuard> {
        let stop = Rc::new(Cell::new(false));
        let wt_slot: Rc<RefCell<Option<WebTransport>>> = Rc::new(RefCell::new(None));

        if !webtransport_supported() {
            state.set(PlayerState::Unsupported);
            return Rc::new(SessionGuard { stop, wt: wt_slot });
        }
        if !autoplay {
            return Rc::new(SessionGuard { stop, wt: wt_slot });
        }

        state.set(PlayerState::Connecting);
        let stop_for_task = stop.clone();
        let wt_for_task = wt_slot.clone();
        let mut state_for_task = state;
        wasm_bindgen_futures::spawn_local(async move {
            let res = run_session(
                &relay_url,
                &namespace,
                &canvas_id,
                muted,
                state_for_task,
                stop_for_task.clone(),
                wt_for_task.clone(),
            )
            .await;
            if stop_for_task.get() {
                state_for_task.set(PlayerState::Stopped);
            } else if let Err(err) = res {
                tracing::warn!(error = %err, "video_player session failed");
                state_for_task.set(PlayerState::Error(err));
            } else {
                state_for_task.set(PlayerState::Stopped);
            }
            // Make sure the JS handle is released even on the happy path.
            if let Some(wt) = wt_for_task.borrow_mut().take() {
                wt.close();
            }
        });

        Rc::new(SessionGuard { stop, wt: wt_slot })
    }

    fn webtransport_supported() -> bool {
        let Some(window) = web_sys::window() else {
            return false;
        };
        Reflect::get(window.as_ref(), &JsValue::from_str("WebTransport"))
            .map(|v| !v.is_undefined() && !v.is_null())
            .unwrap_or(false)
    }

    // ---------------------------------------------------------------------
    // Main session driver
    // ---------------------------------------------------------------------

    async fn run_session(
        relay_url: &str,
        namespace: &str,
        canvas_id: &str,
        muted: bool,
        state: Signal<PlayerState>,
        stop: Rc<Cell<bool>>,
        wt_slot: Rc<RefCell<Option<WebTransport>>>,
    ) -> Result<(), String> {
        // Build the WebTransport init dict: ALPN-pinned moq-lite-04 +
        // optional self-signed cert hash for dev relays (sidecar serves
        // it at HTTP /certificate.sha256).
        //
        // `protocols` is a vendor extension exposed by Chrome's WebTransport
        // implementation (PURA-138 WS-0 GO note: the moq-rs relay only
        // dispatches to the moq-lite-04 handler when this field is set).
        // web-sys's `WebTransportOptions` does not yet bind it, so we drop
        // to `Reflect::set` to attach the field.
        let opts = WebTransportOptions::new();
        let protocols = Array::new();
        protocols.push(&JsValue::from_str("moq-lite-04"));
        Reflect::set(
            opts.as_ref(),
            &JsValue::from_str("protocols"),
            protocols.as_ref(),
        )
        .map_err(|e| format!("set protocols: {}", js_err(&e)))?;

        if let Some(hash_bytes) = fetch_cert_hash(relay_url).await {
            let hash = WebTransportHash::new();
            hash.set_algorithm("sha-256");
            let buf = Uint8Array::new_with_length(hash_bytes.len() as u32);
            buf.copy_from(&hash_bytes);
            hash.set_value(&buf.buffer());
            let hashes = [hash];
            opts.set_server_certificate_hashes(&hashes);
        }

        let wt = WebTransport::new_with_options(relay_url, &opts)
            .map_err(|e| format!("WebTransport::new: {}", js_err(&e)))?;
        *wt_slot.borrow_mut() = Some(wt.clone());

        JsFuture::from(wt.ready())
            .await
            .map_err(|e| format!("WebTransport.ready: {}", js_err(&e)))?;

        // ── Subscribe to video; subscribe to audio only when not muted.
        //    Skipping the audio SUBSCRIBE entirely on muted mounts saves a
        //    pointless round-trip on public embeds whose autoplay policy
        //    will block the AudioContext anyway. The public viewer
        //    remounts the component with `muted=false` once the user
        //    clicks the tap-to-unmute overlay.
        subscribe(&wt, SUBSCRIBE_VIDEO, namespace, "video").await?;
        if !muted {
            subscribe(&wt, SUBSCRIBE_AUDIO, namespace, "audio").await?;
        }

        // ── Build decoders + sinks.
        let canvas = get_canvas(canvas_id)?;
        let ctx = canvas
            .get_context("2d")
            .map_err(|e| format!("getContext: {}", js_err(&e)))?
            .ok_or("getContext(2d) returned null")?
            .dyn_into::<CanvasRenderingContext2d>()
            .map_err(|_| "canvas 2d context cast failed".to_string())?;
        let ctx = Rc::new(ctx);

        let video_decoder = build_video_decoder(ctx.clone(), state)?;
        // Build the audio decoder lazily; muted mounts never feed it.
        let audio_decoder: Option<Rc<AudioDecoderHandle>> = if muted {
            None
        } else {
            let audio_pipeline = AudioPipeline::new()?;
            Some(build_audio_decoder(audio_pipeline.clone())?)
        };

        // ── Drain incoming unidirectional streams (group data).
        let uni_streams = wt.incoming_unidirectional_streams();
        let reader: ReadableStreamDefaultReader = uni_streams
            .get_reader()
            .dyn_into()
            .map_err(|_| "incomingUnidirectionalStreams reader cast".to_string())?;

        loop {
            if stop.get() {
                break;
            }
            let result = match JsFuture::from(reader.read()).await {
                Ok(v) => v,
                Err(e) => {
                    if stop.get() {
                        break;
                    }
                    return Err(format!("uni-stream reader read: {}", js_err(&e)));
                }
            };
            let done = Reflect::get(&result, &JsValue::from_str("done"))
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if done {
                break;
            }
            let stream_js = Reflect::get(&result, &JsValue::from_str("value"))
                .map_err(|e| format!("read result.value: {}", js_err(&e)))?;
            let stream: ReadableStream = stream_js
                .dyn_into()
                .map_err(|_| "uni-stream is not a ReadableStream".to_string())?;

            // Dispatch each group on its own task — keyframe arrival on
            // video must not block audio (and vice versa).
            let video_decoder = video_decoder.clone();
            let audio_decoder = audio_decoder.clone();
            let state_for_group = state;
            let stop_for_group = stop.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(err) = drain_group(
                    stream,
                    video_decoder,
                    audio_decoder,
                    state_for_group,
                    stop_for_group,
                )
                .await
                {
                    tracing::debug!(error = %err, "video_player group drain error");
                }
            });
        }

        let _ = state; // keep moveable into the future logic above
        Ok(())
    }

    // ---------------------------------------------------------------------
    // moq-lite SUBSCRIBE
    // ---------------------------------------------------------------------

    async fn subscribe(
        wt: &WebTransport,
        id: u64,
        broadcast: &str,
        track: &str,
    ) -> Result<(), String> {
        let bidi: WebTransportBidirectionalStream =
            JsFuture::from(wt.create_bidirectional_stream())
                .await
                .map_err(|e| format!("createBidirectionalStream: {}", js_err(&e)))?
                .dyn_into()
                .map_err(|_| "bidi stream cast".to_string())?;

        let writer: WritableStreamDefaultWriter = bidi
            .writable()
            .get_writer()
            .map_err(|e| format!("bidi.writable.getWriter: {}", js_err(&e)))?;

        // Prelude: stream type (raw varint, NOT length-prefixed).
        let mut prelude = Vec::with_capacity(1);
        write_varint(&mut prelude, STREAM_SUBSCRIBE);
        write_chunk(&writer, &prelude).await?;

        // SUBSCRIBE body (length-prefixed). Mirrors `sendSubscribe` in
        // `moq-spike/player/player.js` line ~190.
        let mut body = Vec::with_capacity(64);
        write_varint(&mut body, id);
        write_lp_string(&mut body, broadcast);
        write_lp_string(&mut body, track);
        body.push(0); // priority = 0 (default)
        body.push(1); // ordered = true
        write_varint(&mut body, 0); // maxLatency = 0
        write_varint(&mut body, 0); // startGroup = 0 (latest)
        write_varint(&mut body, 0); // endGroup = 0 (unbounded)

        let mut framed = Vec::with_capacity(body.len() + 8);
        write_varint(&mut framed, body.len() as u64);
        framed.extend_from_slice(&body);
        write_chunk(&writer, &framed).await?;

        // Release the writer so other code paths could re-acquire later.
        let _ = writer.release_lock();

        // Read SUBSCRIBE_OK. `value` is a JsValue (length-prefixed body
        // we don't currently consume — draining the type+body keeps the
        // stream cursor honest for any future fields).
        let reader: ReadableStreamDefaultReader = bidi
            .readable()
            .get_reader()
            .dyn_into()
            .map_err(|_| "bidi.readable.getReader cast".to_string())?;
        let mut sr = StreamReader::new(reader);
        let typ = sr.uvarint().await?;
        if typ != 0 {
            return Err(format!("SUBSCRIBE response type {typ} != 0 (OK)"));
        }
        let body_len = sr.uvarint().await? as usize;
        if body_len > 0 {
            // Tolerate any body bytes; we don't decode them.
            sr.read_bytes(body_len).await?;
        }
        // Hold the reader open: the relay may push status updates over
        // this stream later in moq-lite-04. Dropping `sr` here releases
        // it; if a later draft requires us to keep reading, expand here.
        drop(sr);
        Ok(())
    }

    // ---------------------------------------------------------------------
    // GROUP draining
    // ---------------------------------------------------------------------

    async fn drain_group(
        stream: ReadableStream,
        video_decoder: Rc<VideoDecoderHandle>,
        audio_decoder: Option<Rc<AudioDecoderHandle>>,
        mut state: Signal<PlayerState>,
        stop: Rc<Cell<bool>>,
    ) -> Result<(), String> {
        let reader: ReadableStreamDefaultReader = stream
            .get_reader()
            .dyn_into()
            .map_err(|_| "uni reader cast".to_string())?;
        let mut sr = StreamReader::new(reader);

        let stream_type = sr.uvarint().await?;
        if stream_type != STREAM_GROUP {
            // Skip unknown unidirectional stream types. Future drafts may
            // introduce new types we don't decode.
            return Ok(());
        }

        let _msg_len = sr.uvarint().await?;
        let subscribe_id = sr.uvarint().await?;
        let _sequence = sr.uvarint().await?;

        loop {
            if stop.get() {
                break;
            }
            let frame_len = match sr.try_uvarint().await {
                Some(n) => n as usize,
                None => break, // clean stream EOF
            };
            let bytes = sr.read_bytes(frame_len).await?;
            match subscribe_id {
                SUBSCRIBE_VIDEO => {
                    let is_key = vp8_is_keyframe(&bytes);
                    video_decoder.feed(bytes, is_key)?;
                    // Promote to "Playing" on the first decoded keyframe.
                    if is_key && matches!(*state.peek(), PlayerState::Connecting) {
                        state.set(PlayerState::Playing);
                    }
                }
                SUBSCRIBE_AUDIO => {
                    // `None` when the component is muted (WS-8 public
                    // viewer pre-unmute); drop the frame silently.
                    if let Some(decoder) = audio_decoder.as_ref() {
                        decoder.feed(bytes)?;
                    }
                }
                other => {
                    tracing::debug!(subscribe_id = other, "unknown subscribe id; dropping frame");
                }
            }
        }
        Ok(())
    }

    fn vp8_is_keyframe(data: &[u8]) -> bool {
        !data.is_empty() && (data[0] & 0x01) == 0
    }

    // ---------------------------------------------------------------------
    // VideoDecoder wrapper — paints decoded `VideoFrame`s onto the canvas.
    // ---------------------------------------------------------------------

    pub struct VideoDecoderHandle {
        decoder: VideoDecoder,
        frame_seq: Cell<u64>,
        // Closures must outlive every `decode()` call. Holding them in
        // the handle keeps them alive until the component unmounts.
        _output_closure: Closure<dyn FnMut(JsValue)>,
        _error_closure: Closure<dyn FnMut(JsValue)>,
    }

    fn build_video_decoder(
        ctx: Rc<CanvasRenderingContext2d>,
        state: Signal<PlayerState>,
    ) -> Result<Rc<VideoDecoderHandle>, String> {
        // Audio clock baseline for "drop late video frames". Without an
        // AudioContext handle here we use performance.now()-based timing;
        // good enough for the WS-5 ship — tighter A/V is WS-7's job.
        let last_paint_ms: Rc<Cell<f64>> = Rc::new(Cell::new(0.0));
        // ~30 fps target — same as the fixture (`fixture/build.sh`).
        let target_frame_interval_ms: f64 = 33.0;

        let ctx_for_output = ctx.clone();
        let last_paint_for_output = last_paint_ms.clone();
        let output = Closure::wrap(Box::new(move |frame_js: JsValue| {
            let Ok(frame) = frame_js.dyn_into::<VideoFrame>() else {
                return;
            };
            let now = performance_now();
            // Drop "stale" frames whose decode lagged more than ~2× the
            // frame interval behind the latest paint. Without an audio
            // clock this is a coarse heuristic, but it matches the JS
            // reference player's intent: keep the canvas current.
            let last = last_paint_for_output.get();
            if last > 0.0 && now - last < -target_frame_interval_ms * 2.0 {
                frame.close();
                return;
            }
            let _ = ctx_for_output.draw_image_with_video_frame(&frame, 0.0, 0.0);
            frame.close();
            last_paint_for_output.set(now);
        }) as Box<dyn FnMut(JsValue)>);

        let state_for_err = state;
        let error = Closure::wrap(Box::new(move |e: JsValue| {
            let mut state = state_for_err;
            let msg = format!("VideoDecoder: {}", js_err(&e));
            tracing::warn!("{msg}");
            state.set(PlayerState::Error(msg));
        }) as Box<dyn FnMut(JsValue)>);

        let init = VideoDecoderInit::new(
            error.as_ref().unchecked_ref(),
            output.as_ref().unchecked_ref(),
        );
        let decoder =
            VideoDecoder::new(&init).map_err(|e| format!("VideoDecoder::new: {}", js_err(&e)))?;
        // VP8 codec string — same as the reference player. The fixture
        // emits 1280×720 30fps VP8 keyframed groups (PURA-140 WS-2).
        let config = VideoDecoderConfig::new("vp8");
        decoder
            .configure(&config)
            .map_err(|e| format!("VideoDecoder.configure: {}", js_err(&e)))?;

        let _ = ctx; // closure already owns its own clone

        Ok(Rc::new(VideoDecoderHandle {
            decoder,
            frame_seq: Cell::new(0),
            _output_closure: output,
            _error_closure: error,
        }))
    }

    impl VideoDecoderHandle {
        fn feed(&self, data: Vec<u8>, is_key: bool) -> Result<(), String> {
            // Synthesise a monotonically-increasing timestamp; WebCodecs
            // needs SOMETHING for ordering — exact wall-clock value does
            // not matter for live playback. web-sys binds the
            // `timestamp` field as `i32`, so we emit ms-resolution
            // values (~30 fps → 33 ms/frame) — covers 18+ hours of
            // wall time before the counter wraps.
            let seq = self.frame_seq.get();
            self.frame_seq.set(seq.wrapping_add(1));
            let ts_ms = ((seq as i64).wrapping_mul(33) & 0x7fff_ffff) as i32;

            let buf = Uint8Array::new_with_length(data.len() as u32);
            buf.copy_from(&data);

            let kind = if is_key {
                EncodedVideoChunkType::Key
            } else {
                EncodedVideoChunkType::Delta
            };
            let init = EncodedVideoChunkInit::new(buf.as_ref(), ts_ms, kind);
            let chunk = EncodedVideoChunk::new(&init)
                .map_err(|e| format!("EncodedVideoChunk: {}", js_err(&e)))?;
            self.decoder
                .decode(&chunk)
                .map_err(|e| format!("VideoDecoder.decode: {}", js_err(&e)))
        }
    }

    // ---------------------------------------------------------------------
    // AudioPipeline — schedules decoded `AudioData` against an AudioContext.
    // ---------------------------------------------------------------------

    #[derive(Clone)]
    struct AudioPipeline {
        inner: Rc<AudioPipelineInner>,
    }

    struct AudioPipelineInner {
        ctx: AudioContext,
        next_play: Cell<f64>,
    }

    impl AudioPipeline {
        fn new() -> Result<Self, String> {
            let ctx =
                AudioContext::new().map_err(|e| format!("AudioContext::new: {}", js_err(&e)))?;
            Ok(Self {
                inner: Rc::new(AudioPipelineInner {
                    ctx,
                    next_play: Cell::new(0.0),
                }),
            })
        }

        fn schedule(&self, data: AudioData) -> Result<(), String> {
            let frames = data.number_of_frames() as u32;
            let channels = data.number_of_channels() as u32;
            let sample_rate = data.sample_rate();
            if frames == 0 || channels == 0 || sample_rate <= 0.0 {
                data.close();
                return Ok(());
            }

            // AudioContext.createBuffer(channels, length, sampleRate)
            let buffer: AudioBuffer = self
                .inner
                .ctx
                .create_buffer(channels, frames, sample_rate as f32)
                .map_err(|e| format!("createBuffer: {}", js_err(&e)))?;

            // Copy each channel into its own Float32Array, then push it
            // into the AudioBuffer. `AudioData.copyTo(target, {planeIndex})`
            // pulls a planar slice — WebCodecs Opus output is planar f32
            // by default.
            for ch in 0..channels {
                let target = Float32Array::new_with_length(frames);
                let opts = AudioDataCopyToOptions::new(ch);
                opts.set_format(web_sys::AudioSampleFormat::F32Planar);
                if let Err(e) = data.copy_to_with_buffer_source(&target, &opts) {
                    data.close();
                    return Err(format!("AudioData.copyTo: {}", js_err(&e)));
                }
                // copy_to_channel takes a slice; allocate via to_vec.
                let samples: Vec<f32> = target.to_vec();
                if let Err(e) = buffer.copy_to_channel(&samples, ch as i32) {
                    data.close();
                    return Err(format!("AudioBuffer.copyToChannel: {}", js_err(&e)));
                }
            }
            data.close();

            let source: AudioBufferSourceNode = self
                .inner
                .ctx
                .create_buffer_source()
                .map_err(|e| format!("createBufferSource: {}", js_err(&e)))?;
            source.set_buffer(Some(&buffer));
            source
                .connect_with_audio_node(&self.inner.ctx.destination())
                .map_err(|e| format!("AudioNode.connect: {}", js_err(&e)))?;

            // Schedule against the audio clock. Catch up if we fell
            // behind (`next_play < now`).
            let now = self.inner.ctx.current_time();
            let mut next = self.inner.next_play.get();
            if next < now {
                next = now;
            }
            let duration = frames as f64 / sample_rate as f64;
            source
                .start_with_when(next)
                .map_err(|e| format!("BufferSource.start: {}", js_err(&e)))?;
            self.inner.next_play.set(next + duration);
            Ok(())
        }
    }

    pub struct AudioDecoderHandle {
        decoder: AudioDecoder,
        seq: Cell<u64>,
        configured: Cell<bool>,
        _output_closure: Closure<dyn FnMut(JsValue)>,
        _error_closure: Closure<dyn FnMut(JsValue)>,
    }

    fn build_audio_decoder(pipeline: AudioPipeline) -> Result<Rc<AudioDecoderHandle>, String> {
        let pipeline_for_output = pipeline.clone();
        let output = Closure::wrap(Box::new(move |data_js: JsValue| {
            let Ok(data) = data_js.dyn_into::<AudioData>() else {
                return;
            };
            if let Err(err) = pipeline_for_output.schedule(data) {
                tracing::debug!(error = %err, "audio schedule failed");
            }
        }) as Box<dyn FnMut(JsValue)>);

        let error = Closure::wrap(Box::new(move |e: JsValue| {
            tracing::warn!("AudioDecoder error: {}", js_err(&e));
        }) as Box<dyn FnMut(JsValue)>);

        let init = AudioDecoderInit::new(
            error.as_ref().unchecked_ref(),
            output.as_ref().unchecked_ref(),
        );
        let decoder =
            AudioDecoder::new(&init).map_err(|e| format!("AudioDecoder::new: {}", js_err(&e)))?;

        // ts6-media-sidecar/pipeline.rs encodes audio as mono Opus @ 48 kHz
        // with 20 ms framing (libopus, application=voip). Mirror those
        // params verbatim. (web-sys argument order: codec, channels,
        // sample_rate — both as `u32`.)
        let config = AudioDecoderConfig::new("opus", 1, 48_000);
        decoder
            .configure(&config)
            .map_err(|e| format!("AudioDecoder.configure: {}", js_err(&e)))?;

        let _ = pipeline; // closure owns its clone

        Ok(Rc::new(AudioDecoderHandle {
            decoder,
            seq: Cell::new(0),
            configured: Cell::new(true),
            _output_closure: output,
            _error_closure: error,
        }))
    }

    impl AudioDecoderHandle {
        fn feed(&self, data: Vec<u8>) -> Result<(), String> {
            let seq = self.seq.get();
            self.seq.set(seq.wrapping_add(1));
            // 20 ms framing — match the sidecar's libopus framing exactly.
            // web-sys timestamp is `i32` ms; 20 ms × i32::MAX ≈ 13.6 days
            // before the counter wraps, plenty for a streaming session.
            let ts_ms = ((seq as i64).wrapping_mul(20) & 0x7fff_ffff) as i32;

            let buf = Uint8Array::new_with_length(data.len() as u32);
            buf.copy_from(&data);

            let init = EncodedAudioChunkInit::new(buf.as_ref(), ts_ms, EncodedAudioChunkType::Key);
            let chunk = EncodedAudioChunk::new(&init)
                .map_err(|e| format!("EncodedAudioChunk: {}", js_err(&e)))?;
            self.decoder
                .decode(&chunk)
                .map_err(|e| format!("AudioDecoder.decode: {}", js_err(&e)))?;
            let _ = self.configured.get();
            Ok(())
        }
    }

    // ---------------------------------------------------------------------
    // moq-lite wire helpers (QUIC varint + length-prefixed string)
    // ---------------------------------------------------------------------

    fn write_varint(buf: &mut Vec<u8>, n: u64) {
        if n < 0x40 {
            buf.push(n as u8);
        } else if n < 0x4000 {
            buf.push(0x40 | ((n >> 8) as u8));
            buf.push((n & 0xff) as u8);
        } else if n < 0x4000_0000 {
            buf.push(0x80 | ((n >> 24) as u8));
            buf.push(((n >> 16) & 0xff) as u8);
            buf.push(((n >> 8) & 0xff) as u8);
            buf.push((n & 0xff) as u8);
        } else {
            buf.push(0xc0 | (((n >> 56) & 0x3f) as u8));
            buf.push(((n >> 48) & 0xff) as u8);
            buf.push(((n >> 40) & 0xff) as u8);
            buf.push(((n >> 32) & 0xff) as u8);
            buf.push(((n >> 24) & 0xff) as u8);
            buf.push(((n >> 16) & 0xff) as u8);
            buf.push(((n >> 8) & 0xff) as u8);
            buf.push((n & 0xff) as u8);
        }
    }

    fn write_lp_string(buf: &mut Vec<u8>, s: &str) {
        let bytes = s.as_bytes();
        write_varint(buf, bytes.len() as u64);
        buf.extend_from_slice(bytes);
    }

    async fn write_chunk(writer: &WritableStreamDefaultWriter, bytes: &[u8]) -> Result<(), String> {
        let buf = Uint8Array::new_with_length(bytes.len() as u32);
        buf.copy_from(bytes);
        JsFuture::from(writer.write_with_chunk(buf.as_ref()))
            .await
            .map_err(|e| format!("writer.write: {}", js_err(&e)))?;
        Ok(())
    }

    // ---------------------------------------------------------------------
    // StreamReader — buffered, varint-aware view over a `ReadableStream`.
    // ---------------------------------------------------------------------

    struct StreamReader {
        reader: ReadableStreamDefaultReader,
        buf: Vec<u8>,
        done: bool,
    }

    impl StreamReader {
        fn new(reader: ReadableStreamDefaultReader) -> Self {
            Self {
                reader,
                buf: Vec::new(),
                done: false,
            }
        }

        async fn fill(&mut self) -> Result<bool, String> {
            if self.done {
                return Ok(false);
            }
            let result = JsFuture::from(self.reader.read())
                .await
                .map_err(|e| format!("stream.read: {}", js_err(&e)))?;
            let done = Reflect::get(&result, &JsValue::from_str("done"))
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if done {
                self.done = true;
                return Ok(false);
            }
            let value = Reflect::get(&result, &JsValue::from_str("value"))
                .map_err(|e| format!("stream.read value: {}", js_err(&e)))?;
            let chunk: Uint8Array = value
                .dyn_into()
                .map_err(|_| "stream chunk not a Uint8Array".to_string())?;
            if chunk.length() == 0 {
                return Err("empty stream chunk".to_string());
            }
            let start = self.buf.len();
            self.buf.resize(start + chunk.length() as usize, 0);
            chunk.copy_to(&mut self.buf[start..]);
            Ok(true)
        }

        async fn fill_to(&mut self, n: usize) -> Result<(), String> {
            while self.buf.len() < n {
                if !self.fill().await? {
                    return Err("unexpected end of stream".into());
                }
            }
            Ok(())
        }

        async fn read_bytes(&mut self, n: usize) -> Result<Vec<u8>, String> {
            self.fill_to(n).await?;
            let out = self.buf[..n].to_vec();
            self.buf.drain(..n);
            Ok(out)
        }

        async fn uvarint(&mut self) -> Result<u64, String> {
            self.fill_to(1).await?;
            let first = self.buf[0];
            self.buf.drain(..1);
            let prefix = first >> 6;
            let value = (first & 0x3f) as u64;
            match prefix {
                0 => Ok(value),
                1 => {
                    self.fill_to(1).await?;
                    let b = self.buf[0] as u64;
                    self.buf.drain(..1);
                    Ok((value << 8) | b)
                }
                2 => {
                    self.fill_to(3).await?;
                    let mut v = value;
                    for &b in &self.buf[..3] {
                        v = (v << 8) | b as u64;
                    }
                    self.buf.drain(..3);
                    Ok(v)
                }
                _ => {
                    self.fill_to(7).await?;
                    let mut v = value;
                    for &b in &self.buf[..7] {
                        v = (v << 8) | b as u64;
                    }
                    self.buf.drain(..7);
                    Ok(v)
                }
            }
        }

        /// Like [`Self::uvarint`] but returns `None` on a clean EOF
        /// (used to terminate the frame-decode loop without an error).
        async fn try_uvarint(&mut self) -> Option<u64> {
            if self.buf.is_empty() && !self.done {
                if !self.fill().await.ok()? {
                    return None;
                }
            }
            if self.buf.is_empty() {
                return None;
            }
            self.uvarint().await.ok()
        }
    }

    // ---------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------

    fn get_canvas(canvas_id: &str) -> Result<HtmlCanvasElement, String> {
        let document = web_sys::window()
            .and_then(|w| w.document())
            .ok_or("no document")?;
        document
            .get_element_by_id(canvas_id)
            .ok_or_else(|| format!("canvas #{canvas_id} not found"))?
            .dyn_into::<HtmlCanvasElement>()
            .map_err(|_| format!("#{canvas_id} is not a <canvas>"))
    }

    fn performance_now() -> f64 {
        web_sys::window()
            .and_then(|w| w.performance())
            .map(|p| p.now())
            .unwrap_or(0.0)
    }

    fn js_err(e: &JsValue) -> String {
        if let Some(s) = e.as_string() {
            return s;
        }
        if let Ok(msg) = Reflect::get(e, &JsValue::from_str("message")) {
            if let Some(s) = msg.as_string() {
                return s;
            }
        }
        format!("{e:?}")
    }

    async fn fetch_cert_hash(relay_url: &str) -> Option<Vec<u8>> {
        // The sidecar serves the SHA-256 hex of its self-signed cert at
        // `http://<host>:<port>/certificate.sha256`. In production behind
        // a real CA this 404s and we fall back to the OS trust store
        // (WebTransport allows that when the cert is publicly trusted).
        let url = web_sys::Url::new(relay_url).ok()?;
        let host = url.hostname();
        let port = url.port();
        let cert_url = if port.is_empty() {
            format!("http://{host}/certificate.sha256")
        } else {
            format!("http://{host}:{port}/certificate.sha256")
        };
        let window = web_sys::window()?;
        let resp_js = JsFuture::from(window.fetch_with_str(&cert_url))
            .await
            .ok()?;
        let resp: web_sys::Response = resp_js.dyn_into().ok()?;
        if !resp.ok() {
            return None;
        }
        let text_js = JsFuture::from(resp.text().ok()?).await.ok()?;
        let hex = text_js.as_string()?;
        let hex = hex.trim();
        if hex.len() != 64 {
            return None;
        }
        let mut out = Vec::with_capacity(32);
        for i in 0..32 {
            let byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
            out.push(byte);
        }
        Some(out)
    }
}
