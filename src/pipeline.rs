//! Shared GStreamer pipeline with dynamic per-peer webrtcbin branches.
//!
//! One capture + encode chain runs from server boot. A `tee` element fans
//! the encoded RTP stream out to one `webrtcbin` per connected client.
//!
//!   <capture+encode head> -> h264parse -> rtph264pay -> tee
//!                                                         |
//!                            +----------+-----------------+
//!                            v          v
//!                     queue+webrtc  queue+webrtc   ...
//!                     (peer 1)      (peer 2)
//!
//! The capture+encode head is platform-specific (see `capture_encode_head`):
//!   Linux:   v4l2src (MJPEG) -> jpegdec -> videoconvert -> x264enc
//!   Windows: mfvideosrc -> videoconvert -> videoscale -> mfh264enc
//!
//! Video params (resolution / fps / bitrate / raw format) live in
//! `VideoConfig` and can be changed at runtime via `reconfigure`. Reconfigure
//! rebuilds the whole pipeline, which invalidates every existing `PeerBranch`;
//! callers must drop their peers and have clients reconnect.

use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use gstreamer as gst;
use gstreamer::prelude::*;
use serde::{Deserialize, Serialize};

/// Mutable video parameters for the shared capture+encode chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoConfig {
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub bitrate_kbps: u32,
    /// Raw pixel format fed to the encoder, e.g. "I420" or "NV12".
    pub format: String,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            framerate: 30,
            bitrate_kbps: 6000,
            // mfh264enc (Windows) only accepts NV12 input; x264enc (Linux) is
            // happy with I420. Pick the encoder's native format per platform.
            format: default_format().to_string(),
        }
    }
}

/// The encoder's preferred raw input format for this platform.
const fn default_format() -> &'static str {
    if cfg!(target_os = "windows") {
        "NV12"
    } else {
        "I420"
    }
}

impl VideoConfig {
    /// Reject obviously-invalid params before we hand them to GStreamer.
    fn validate(&self) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            return Err(anyhow!("width/height must be non-zero"));
        }
        if self.width > 7680 || self.height > 4320 {
            return Err(anyhow!("resolution above 8K is not allowed"));
        }
        if self.framerate == 0 || self.framerate > 240 {
            return Err(anyhow!("framerate must be 1..=240"));
        }
        if self.bitrate_kbps == 0 || self.bitrate_kbps > 100_000 {
            return Err(anyhow!("bitrate_kbps must be 1..=100000"));
        }
        // GStreamer caps are picky about format strings; allow only known-good
        // raw formats so a typo can't wedge the pipeline rebuild.
        const ALLOWED: [&str; 4] = ["I420", "NV12", "YUY2", "BGRA"];
        if !ALLOWED.contains(&self.format.as_str()) {
            return Err(anyhow!(
                "format must be one of {ALLOWED:?}, got {:?}",
                self.format
            ));
        }
        Ok(())
    }
}

/// Platform-specific capture + H.264-encode chain, ending right before
/// `h264parse`. The rest of the pipeline (parse, payload, tee) is shared.
///
/// Linux: V4L2 camera delivering MJPEG, software-encoded with x264.
/// Windows: Media Foundation camera, hardware-encoded with mfh264enc.
fn capture_encode_head(cfg: &VideoConfig) -> String {
    let (w, h, fps, br, fmt) = (
        cfg.width,
        cfg.height,
        cfg.framerate,
        cfg.bitrate_kbps,
        &cfg.format,
    );

    #[cfg(target_os = "windows")]
    {
        // mfh264enc bitrate is in kbit/s, same unit as VideoConfig::bitrate_kbps.
        // No explicit source caps: let mfvideosrc negotiate its native format,
        // then videoconvert normalizes to the requested raw format. `videoscale`
        // handles the resolution change. `low-latency=true` ~ zerolatency.
        format!(
            "mfvideosrc ! \
             videoconvert ! \
             videoscale ! \
             video/x-raw,format={fmt},width={w},height={h},framerate={fps}/1 ! \
             mfh264enc bitrate={br} low-latency=true"
        )
    }

    #[cfg(not(target_os = "windows"))]
    {
        // x264enc bitrate is in kbit/s.
        format!(
            "v4l2src device=/dev/video0 ! \
             image/jpeg,width={w},height={h},framerate={fps}/1 ! \
             jpegdec ! \
             videoconvert ! \
             video/x-raw,format={fmt} ! \
             x264enc bitrate={br} tune=zerolatency speed-preset=ultrafast"
        )
    }
}

/// Build a fully-formed (capture -> encode -> parse -> pay -> tee) pipeline for
/// the given config, returning the pipeline and its tee element.
fn build_pipeline(cfg: &VideoConfig) -> Result<(gst::Pipeline, gst::Element)> {
    let pipeline_str = format!(
        "{head} ! \
         h264parse config-interval=-1 ! \
         rtph264pay pt=96 mtu=1200 aggregate-mode=zero-latency ! \
         application/x-rtp,media=video,encoding-name=H264,payload=96 ! \
         tee name=videotee allow-not-linked=true",
        head = capture_encode_head(cfg),
    );

    let pipeline = gst::parse::launch(&pipeline_str)
        .context("Failed to build shared pipeline")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("Parsed element is not a Pipeline"))?;

    let tee = pipeline
        .by_name("videotee")
        .context("tee element missing from pipeline")?;

    Ok((pipeline, tee))
}

/// The swappable inner state. Held behind a `Mutex` so `reconfigure` can tear
/// the old pipeline down and stand a new one up while other handlers wait.
struct Inner {
    pipeline: gst::Pipeline,
    tee: gst::Element,
    config: VideoConfig,
}

pub struct SharedPipeline {
    inner: Mutex<Inner>,
}

impl SharedPipeline {
    pub fn new() -> Result<Self> {
        let config = VideoConfig::default();
        let (pipeline, tee) = build_pipeline(&config)?;
        Ok(Self {
            inner: Mutex::new(Inner {
                pipeline,
                tee,
                config,
            }),
        })
    }

    /// Start the capture/encode chain. Runs from server boot until shutdown.
    pub fn start(&self) -> Result<()> {
        let inner = self.inner.lock().unwrap();
        inner
            .pipeline
            .set_state(gst::State::Playing)
            .context("Failed to start shared pipeline")?;
        Ok(())
    }

    /// The current video configuration.
    pub fn config(&self) -> VideoConfig {
        self.inner.lock().unwrap().config.clone()
    }

    /// Rebuild the shared pipeline with new video params. Validates first, then
    /// builds the replacement BEFORE tearing down the old one, so a bad config
    /// (e.g. resolution the camera can't deliver) leaves the live stream intact.
    ///
    /// On success every existing `PeerBranch` is now orphaned (it references the
    /// old, now-stopped pipeline). The caller must drop all peers and have
    /// clients reconnect to tap the new pipeline.
    pub fn reconfigure(&self, new_config: VideoConfig) -> Result<()> {
        new_config.validate()?;

        // Build + start the replacement first. If this fails, bail without
        // touching the running pipeline.
        let (new_pipeline, new_tee) = build_pipeline(&new_config)?;
        new_pipeline
            .set_state(gst::State::Playing)
            .context("Failed to start reconfigured pipeline")?;

        let mut inner = self.inner.lock().unwrap();
        let old = std::mem::replace(
            &mut *inner,
            Inner {
                pipeline: new_pipeline,
                tee: new_tee,
                config: new_config,
            },
        );
        // Drop the lock-held old pipeline explicitly to Null.
        let _ = old.pipeline.set_state(gst::State::Null);
        Ok(())
    }

    /// Add a new per-peer branch: `tee -> queue -> webrtcbin`. The `configure`
    /// closure runs after the branch is in the pipeline but before data flows,
    /// which is the right place to connect webrtcbin signal handlers.
    pub fn attach_peer(&self, configure: impl FnOnce(&gst::Element)) -> Result<PeerBranch> {
        let inner = self.inner.lock().unwrap();
        let pipeline = inner.pipeline.clone();
        let tee = inner.tee.clone();
        drop(inner);

        let tee_pad = tee
            .request_pad_simple("src_%u")
            .context("Failed to request tee src pad")?;

        let queue = gst::ElementFactory::make("queue")
            .property_from_str("leaky", "downstream")
            .property("max-size-buffers", 0u32)
            .property("max-size-bytes", 0u32)
            .property("max-size-time", 200_000_000u64) // 200 ms
            .build()
            .context("Failed to create queue")?;

        let webrtcbin = gst::ElementFactory::make("webrtcbin")
            .property_from_str("bundle-policy", "max-bundle")
            .property("stun-server", "stun://stun.l.google.com:19302")
            .build()
            .context("Failed to create webrtcbin")?;

        pipeline.add_many([&queue, &webrtcbin])?;

        let queue_src = queue.static_pad("src").context("queue has no src pad")?;
        let webrtc_sink = webrtcbin
            .request_pad_simple("sink_%u")
            .context("webrtcbin denied sink pad request")?;
        queue_src.link(&webrtc_sink)?;

        configure(&webrtcbin);

        // Bring the new branch to Playing BEFORE we connect the tee.
        queue.sync_state_with_parent()?;
        webrtcbin.sync_state_with_parent()?;

        // Now link the tee -> queue. Data starts flowing.
        let queue_sink = queue.static_pad("sink").context("queue has no sink pad")?;
        tee_pad.link(&queue_sink)?;

        // Ask the encoder for a keyframe so the new peer can start decoding
        // immediately instead of waiting for the next periodic IDR.
        let force_key = gst::event::CustomUpstream::new(
            gst::Structure::builder("GstForceKeyUnit")
                .field("all-headers", true)
                .build(),
        );
        let _ = tee.send_event(force_key);

        Ok(PeerBranch {
            pipeline,
            tee,
            tee_pad,
            queue,
            webrtcbin,
        })
    }
}

impl Drop for SharedPipeline {
    fn drop(&mut self) {
        let inner = self.inner.lock().unwrap();
        let _ = inner.pipeline.set_state(gst::State::Null);
    }
}

/// A per-peer branch in the shared pipeline. Dropping this releases the
/// branch's webrtcbin, queue, and tee request pad.
pub struct PeerBranch {
    pipeline: gst::Pipeline,
    tee: gst::Element,
    tee_pad: gst::Pad,
    queue: gst::Element,
    pub webrtcbin: gst::Element,
}

impl Drop for PeerBranch {
    fn drop(&mut self) {
        // Unlink and tear down. The tee has allow-not-linked=true so the
        // shared pipeline keeps flowing fine even while we clean up.
        if let Some(queue_sink) = self.queue.static_pad("sink") {
            let _ = self.tee_pad.unlink(&queue_sink);
        }
        let _ = self.queue.set_state(gst::State::Null);
        let _ = self.webrtcbin.set_state(gst::State::Null);
        let _ = self.pipeline.remove(&self.queue);
        let _ = self.pipeline.remove(&self.webrtcbin);
        self.tee.release_request_pad(&self.tee_pad);
    }
}
