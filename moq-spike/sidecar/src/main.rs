// moq-spike-sidecar: WS-0 publisher for Phase 5.
//
// Reads a VP8-in-IVF video fixture and publishes it as a moq-lite broadcast.
// Each VP8 keyframe opens a new moq-lite Group; inter-frames continue the same Group.
// This gives the subscriber a clean re-sync point at every keyframe boundary.
//
// See moq-spike/README.md and docs/adr/0007-moq-flavor-and-draft-pin.md.

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use clap::Parser;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::Instant;
use tracing::info;
use url::Url;

#[derive(Parser, Debug)]
#[command(name = "moq-spike-sidecar", version, about = "WS-0 MoQ publisher")]
struct Args {
    /// Relay URL, e.g. https://localhost:4443/anon
    #[arg(long)]
    relay: Url,

    /// Broadcast namespace, e.g. pura-spike/0
    #[arg(long, default_value = "pura-spike/0")]
    namespace: String,

    /// Path to VP8-in-IVF video fixture
    #[arg(long)]
    video: PathBuf,

    /// Loop the fixture indefinitely
    #[arg(long, default_value_t = true)]
    r#loop: bool,

    /// moq-native client options (TLS, iroh, etc.)
    #[command(flatten)]
    client: moq_native::ClientConfig,

    /// Logging options
    #[command(flatten)]
    log: moq_native::Log,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    args.log.init();

    info!(relay = %args.relay, namespace = %args.namespace, video = %args.video.display());

    let video_bytes = tokio::fs::read(&args.video)
        .await
        .with_context(|| format!("reading {:?}", args.video))?;

    let ivf = Ivf::parse(&video_bytes).context("parsing IVF file")?;
    info!(
        width = ivf.width,
        height = ivf.height,
        frame_rate = ivf.frame_rate,
        time_scale = ivf.time_scale,
        frames = ivf.frames.len(),
        "IVF parsed"
    );

    let frame_duration_ns =
        Duration::from_nanos((1_000_000_000u64 * ivf.time_scale as u64) / ivf.frame_rate as u64);

    let client = args.client.init().context("init moq-native client")?;
    let origin = moq_lite::Origin::random().produce();

    let mut broadcast = moq_lite::Broadcast::new().produce();
    let track_producer = broadcast
        .create_track(moq_lite::Track {
            name: "video".into(),
            priority: 0,
        })
        .context("create video track")?;

    origin.publish_broadcast(&args.namespace, broadcast.consume());

    let reconnect = client
        .with_publish(origin.consume())
        .reconnect(args.relay.clone());

    info!("publishing broadcast, waiting for subscribers…");

    let publisher = tokio::spawn(publish_loop(
        track_producer,
        ivf,
        frame_duration_ns,
        args.r#loop,
    ));

    tokio::select! {
        res = reconnect.closed() => res.context("relay connection closed"),
        res = publisher => res.context("publisher task panicked")?.context("publisher error"),
    }
}

async fn publish_loop(
    mut track: moq_lite::TrackProducer,
    ivf: Ivf,
    frame_duration: Duration,
    do_loop: bool,
) -> Result<()> {
    let mut sequence: u64 = 0;

    loop {
        // New loop pass: set a fresh timing baseline so inter-pass sleeps pace correctly.
        let loop_start = Instant::now();
        let mut loop_elapsed = Duration::ZERO;

        for vp8_frame in &ivf.frames {
            let is_key = vp8_frame.is_keyframe();

            if is_key {
                // New keyframe → new moq-lite Group, giving subscribers a clean re-sync.
                if let Ok(mut grp) = track.create_group(sequence.into()) {
                    grp.write_frame(Bytes::copy_from_slice(&vp8_frame.data))
                        .context("write keyframe")?;
                    grp.finish().context("finish group")?;
                }
                sequence += 1;
            } else {
                // Each inter-frame gets its own group so the relay can drop late ones.
                let mut grp = track
                    .create_group(sequence.into())
                    .context("create inter-frame group")?;
                grp.write_frame(Bytes::copy_from_slice(&vp8_frame.data))
                    .context("write inter-frame")?;
                grp.finish().context("finish inter-frame group")?;
                sequence += 1;
            }

            // Pace frames at the capture frame rate.
            loop_elapsed += frame_duration;
            let target = loop_start + loop_elapsed;
            let now = Instant::now();
            if target > now {
                tokio::time::sleep_until(target).await;
            }
        }

        if do_loop {
            info!("looping fixture (sequence={})", sequence);
            // loop_start / loop_elapsed are reset at top of loop.
        } else {
            break;
        }
    }

    info!("fixture finished, closing track");
    Ok(())
}

// Minimal IVF parser (https://wiki.multimedia.cx/index.php/IVF)

struct IvfFrame {
    data: Vec<u8>,
}

impl IvfFrame {
    fn is_keyframe(&self) -> bool {
        // VP8 keyframe: byte[0] bit 0 == 0
        self.data.first().map_or(false, |b| b & 0x01 == 0)
    }
}

struct Ivf {
    width: u16,
    height: u16,
    frame_rate: u32,
    time_scale: u32,
    frames: Vec<IvfFrame>,
}

impl Ivf {
    fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 32 {
            bail!("IVF file too short");
        }
        if &data[0..4] != b"DKIF" {
            bail!("not an IVF file (bad magic)");
        }
        let fourcc = &data[8..12];
        if fourcc != b"VP80" {
            bail!(
                "unsupported codec: {}",
                String::from_utf8_lossy(fourcc)
            );
        }

        let width = u16::from_le_bytes([data[12], data[13]]);
        let height = u16::from_le_bytes([data[14], data[15]]);
        let frame_rate = u32::from_le_bytes([data[16], data[17], data[18], data[19]]);
        let time_scale = u32::from_le_bytes([data[20], data[21], data[22], data[23]]);

        if frame_rate == 0 || time_scale == 0 {
            bail!("IVF has zero frame_rate or time_scale");
        }

        let header_size = u16::from_le_bytes([data[4], data[5]]) as usize;
        let mut pos = header_size.max(32);
        let mut frames = Vec::new();

        while pos + 12 <= data.len() {
            let frame_size =
                u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                    as usize;
            pos += 12; // skip size (4) + timestamp (8)
            if pos + frame_size > data.len() {
                bail!("IVF frame extends past EOF at frame {}", frames.len());
            }
            frames.push(IvfFrame {
                data: data[pos..pos + frame_size].to_vec(),
            });
            pos += frame_size;
        }

        if frames.is_empty() {
            bail!("IVF file contains no frames");
        }

        Ok(Self {
            width,
            height,
            frame_rate,
            time_scale,
            frames,
        })
    }
}
