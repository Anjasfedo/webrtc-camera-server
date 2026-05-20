//! Shared GStreamer pipeline with dynamic per-peer webrtcbin branches.
//!
//! One capture + encode chain runs from server boot. A `tee` element fans
//! the encoded RTP stream out to one `webrtcbin` per connected client.
//!
//!   mfvideosrc -> videoconvert -> mfh264enc -> h264parse -> rtph264pay -> tee
//!                                                                          |
//!                                              +----------+----------------+
//!                                              v          v
//!                                       queue+webrtc  queue+webrtc   ...
//!                                       (peer 1)      (peer 2)

use anyhow::{Context, Result, anyhow};
use gstreamer as gst;
use gstreamer::prelude::*;

pub const VIDEO_WIDTH: u32 = 2048;
pub const VIDEO_HEIGHT: u32 = 1536;
pub const VIDEO_FRAMERATE: u32 = 20;
pub const VIDEO_BITRATE_KBPS: u32 = 6000;

pub struct SharedPipeline {
    pipeline: gst::Pipeline,
    tee: gst::Element,
}

impl SharedPipeline {
    pub fn new() -> Result<Self> {
        let pipeline_str = format!(
            "mfvideosrc device-index=0 ! \
             image/jpeg,width={w},height={h},framerate={fps}/1 ! \
             jpegdec ! \
             videoconvert ! \
             video/x-raw,format=NV12 ! \
             mfh264enc bitrate={br} low-latency=true ! \
             h264parse config-interval=-1 ! \
             rtph264pay pt=96 mtu=1200 aggregate-mode=zero-latency ! \
             application/x-rtp,media=video,encoding-name=H264,payload=96 ! \
             tee name=videotee allow-not-linked=true",
            w = VIDEO_WIDTH,
            h = VIDEO_HEIGHT,
            fps = VIDEO_FRAMERATE,
            br = VIDEO_BITRATE_KBPS,
        );

        let pipeline = gst::parse::launch(&pipeline_str)
            .context("Failed to build shared pipeline")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("Parsed element is not a Pipeline"))?;

        let tee = pipeline
            .by_name("videotee")
            .context("tee element missing from pipeline")?;

        Ok(Self { pipeline, tee })
    }

    /// Start the capture/encode chain. Runs from server boot until shutdown.
    pub fn start(&self) -> Result<()> {
        self.pipeline
            .set_state(gst::State::Playing)
            .context("Failed to start shared pipeline")?;
        Ok(())
    }

    /// Add a new per-peer branch: `tee -> queue -> webrtcbin`. The `configure`
    /// closure runs after the branch is in the pipeline but before data flows,
    /// which is the right place to connect webrtcbin signal handlers.
    pub fn attach_peer(&self, configure: impl FnOnce(&gst::Element)) -> Result<PeerBranch> {
        let tee_pad = self
            .tee
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

        self.pipeline.add_many([&queue, &webrtcbin])?;

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
        let _ = self.tee.send_event(force_key);

        Ok(PeerBranch {
            pipeline: self.pipeline.clone(),
            tee: self.tee.clone(),
            tee_pad,
            queue,
            webrtcbin,
        })
    }
}

impl Drop for SharedPipeline {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
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
