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

use crate::camera::{CameraCap, CameraDevice};

/// Mutable video parameters for the shared capture+encode chain.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VideoConfig {
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub bitrate_kbps: u32,
    /// Raw pixel format fed to the encoder, e.g. "I420" or "NV12".
    pub format: String,
    /// Which camera to capture from (`CameraDevice::id`). `None` / empty = the
    /// pipeline's default source (no explicit device selected).
    #[serde(default)]
    pub device_id: Option<String>,
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
            device_id: None,
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
    /// Reject params the camera can't deliver before handing them to GStreamer.
    ///
    /// `caps` is the probed capability list. When non-empty we require the
    /// requested resolution+framerate to match a real capture mode (the camera
    /// ties these together). When empty (probe found nothing) we fall back to
    /// generic sanity bounds so the server still works.
    fn validate(&self, caps: &[CameraCap]) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            return Err(anyhow!("width/height must be non-zero"));
        }
        if self.framerate == 0 || self.framerate > 240 {
            return Err(anyhow!("framerate must be 1..=240"));
        }
        if self.bitrate_kbps == 0 || self.bitrate_kbps > 100_000 {
            return Err(anyhow!("bitrate_kbps must be 1..=100000"));
        }
        // `format` is the encoder's INPUT format (videoconvert bridges from the
        // camera's native format), so validate it against the encoder, not the
        // camera. mfh264enc needs NV12; x264enc takes I420.
        const ALLOWED: [&str; 4] = ["I420", "NV12", "YUY2", "BGRA"];
        if !ALLOWED.contains(&self.format.as_str()) {
            return Err(anyhow!(
                "format must be one of {ALLOWED:?}, got {:?}",
                self.format
            ));
        }

        if !caps.is_empty() {
            let (w, h, fps) = (self.width as i32, self.height as i32, self.framerate);
            let supported = caps
                .iter()
                .any(|c| c.width == w && c.height == h && c.framerate == fps);
            if !supported {
                return Err(anyhow!(
                    "camera does not support {}x{} @ {}fps; pick a listed mode",
                    self.width,
                    self.height,
                    self.framerate
                ));
            }
        } else if self.width > 7680 || self.height > 4320 {
            return Err(anyhow!("resolution above 8K is not allowed"));
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
        // Pin the source to a NATIVE camera mode (width/height/framerate the
        // device actually advertises — validated against probed caps), letting
        // mfvideosrc choose its native pixel format for that mode. videoconvert
        // then bridges to the encoder's required input format. No videoscale:
        // we run the camera's real resolution, not a scaled approximation.
        // `low-latency=true` ~ zerolatency.
        let device = device_path_arg(cfg.device_id.as_deref());
        format!(
            "mfvideosrc{device} ! \
             video/x-raw,width={w},height={h},framerate={fps}/1 ! \
             videoconvert ! \
             video/x-raw,format={fmt} ! \
             mfh264enc bitrate={br} low-latency=true"
        )
    }

    #[cfg(not(target_os = "windows"))]
    {
        // x264enc bitrate is in kbit/s. On v4l2 the device id is the /dev path.
        let device = cfg
            .device_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("/dev/video0");
        format!(
            "v4l2src device={device} ! \
             image/jpeg,width={w},height={h},framerate={fps}/1 ! \
             jpegdec ! \
             videoconvert ! \
             video/x-raw,format={fmt} ! \
             x264enc bitrate={br} tune=zerolatency speed-preset=ultrafast"
        )
    }
}

/// Build the ` device-path="..."` argument for `mfvideosrc`, or an empty string
/// for the default device. The MF device path contains backslashes and `#`/`{}`
/// characters that `gst::parse::launch` would mis-tokenize, so we wrap it in
/// quotes and escape backslashes and embedded quotes.
#[cfg(target_os = "windows")]
fn device_path_arg(device_id: Option<&str>) -> String {
    match device_id.filter(|s| !s.is_empty()) {
        Some(path) => {
            let escaped = path.replace('\\', "\\\\").replace('"', "\\\"");
            format!(" device-path=\"{escaped}\"")
        }
        None => String::new(),
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

/// Choose a sensible default config from real camera caps: prefer the base
/// config's resolution+fps if the camera supports it, else the highest-res mode
/// at the base framerate, else the highest-res mode overall. Keeps the base
/// format/bitrate (format is encoder-input, not camera-native).
fn pick_default_config(caps: &[CameraCap], base: &VideoConfig) -> VideoConfig {
    let (bw, bh, bfps) = (base.width as i32, base.height as i32, base.framerate);

    let exact = caps
        .iter()
        .find(|c| c.width == bw && c.height == bh && c.framerate == bfps);
    let at_fps = || caps.iter().filter(|c| c.framerate == bfps).max_by_key(|c| c.width * c.height);
    let any = || caps.iter().max_by_key(|c| (c.framerate, c.width * c.height));

    let chosen = exact.or_else(at_fps).or_else(any);
    match chosen {
        Some(c) => VideoConfig {
            width: c.width as u32,
            height: c.height as u32,
            framerate: c.framerate,
            ..base.clone()
        },
        None => base.clone(),
    }
}

/// The capture modes for the device with `device_id`, or an empty slice if no
/// device matches (no device selected, or it was unplugged since probing).
fn caps_for<'a>(devices: &'a [CameraDevice], device_id: Option<&str>) -> &'a [CameraCap] {
    let id = device_id.unwrap_or("");
    devices
        .iter()
        .find(|d| d.id == id)
        // When no id is set, fall back to the first device's caps.
        .or_else(|| if id.is_empty() { devices.first() } else { None })
        .map(|d| d.caps.as_slice())
        .unwrap_or(&[])
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
    /// All connected cameras + their real capture modes, probed at startup.
    /// Empty if probing found nothing (then `validate` falls back to bounds).
    devices: Vec<CameraDevice>,
}

impl SharedPipeline {
    pub fn new() -> Result<Self> {
        // Probe cameras that match our pipeline's source. On Windows the
        // pipeline uses mfvideosrc, so list only mediafoundation devices.
        let only_api = if cfg!(target_os = "windows") {
            Some("mediafoundation")
        } else {
            None
        };
        let devices = crate::camera::probe_cameras(only_api).unwrap_or_else(|e| {
            tracing::warn!("Camera probe failed: {e}; continuing without devices");
            Vec::new()
        });

        // Default to the first device and one of its real capture modes (prefer
        // 1080p30, else highest-res at 30fps, else highest-res). Fall back to
        // the static config when no device was found.
        let mut config = VideoConfig::default();
        if let Some(first) = devices.first() {
            config.device_id = Some(first.id.clone()).filter(|s| !s.is_empty());
            if !first.caps.is_empty() {
                config = pick_default_config(&first.caps, &config);
            }
        }
        config.validate(caps_for(&devices, config.device_id.as_deref()))?;

        let (pipeline, tee) = build_pipeline(&config)?;
        Ok(Self {
            inner: Mutex::new(Inner {
                pipeline,
                tee,
                config,
            }),
            devices,
        })
    }

    /// All connected cameras + their capture modes (for the client's dropdowns).
    pub fn devices(&self) -> Vec<CameraDevice> {
        self.devices.clone()
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

    /// Whether the shared pipeline is currently in the Playing state (used by
    /// the readiness probe). A short timeout avoids blocking on a state change.
    pub fn is_playing(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        let (_, current, _) = inner.pipeline.state(gst::ClockTime::from_mseconds(0));
        current == gst::State::Playing
    }

    /// Rebuild the shared pipeline with new video params. Validates first, then
    /// builds the replacement BEFORE tearing down the old one, so a bad config
    /// (e.g. resolution the camera can't deliver) leaves the live stream intact.
    ///
    /// Returns `Ok(true)` if the pipeline was rebuilt, `Ok(false)` if the
    /// requested config is identical to the current one (no-op — we skip the
    /// expensive teardown/rebuild and existing peers keep streaming).
    ///
    /// On a real rebuild every existing `PeerBranch` is now orphaned (it
    /// references the old, now-stopped pipeline). The caller must drop all peers
    /// and have clients reconnect to tap the new pipeline.
    pub fn reconfigure(&self, new_config: VideoConfig) -> Result<bool> {
        // Reject a device id we don't know about (unplugged, or never existed).
        if let Some(id) = new_config.device_id.as_deref().filter(|s| !s.is_empty()) {
            if !self.devices.is_empty() && !self.devices.iter().any(|d| d.id == id) {
                return Err(anyhow!("unknown camera device: {id}"));
            }
        }
        new_config.validate(caps_for(&self.devices, new_config.device_id.as_deref()))?;

        // No-op if nothing changed: avoid churning the live pipeline (and
        // bouncing every connected peer) on an Apply of identical settings.
        if self.inner.lock().unwrap().config == new_config {
            return Ok(false);
        }

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
        Ok(true)
    }

    /// Add a new per-peer branch: `tee -> queue -> webrtcbin`. The `configure`
    /// closure runs after the branch is in the pipeline but before data flows,
    /// which is the right place to connect webrtcbin signal handlers. `stun` is
    /// the STUN server URI for this peer's `webrtcbin`.
    pub fn attach_peer(
        &self,
        stun: &str,
        configure: impl FnOnce(&gst::Element),
    ) -> Result<PeerBranch> {
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
            .property("stun-server", stun)
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
