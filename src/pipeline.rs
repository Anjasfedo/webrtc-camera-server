//! GStreamer pipeline definition.
//!
//! Windows-optimized pipeline:
//!
//!   mfvideosrc        -> Media Foundation camera source (Windows 10+).
//!   videoconvert      -> Format negotiation glue.
//!   mfh264enc         -> Hardware H.264 encoder via Media Foundation.
//!   h264parse         -> Parse the H.264 stream for the RTP payloader.
//!   rtph264pay        -> Packetize H.264 into RTP. zero-latency aggregation
//!                        gives the lowest possible glass-to-glass delay.
//!   webrtcbin         -> Handles ICE, DTLS-SRTP, and SDP negotiation.

use anyhow::{Context, Result, anyhow};
use gstreamer as gst;
use gstreamer::prelude::*;

/// Width/height/framerate of the camera stream.
/// Adjust if your camera doesn't support 1280x720@30.
pub const VIDEO_WIDTH: u32 = 1280;
pub const VIDEO_HEIGHT: u32 = 720;
pub const VIDEO_FRAMERATE: u32 = 30;

/// Encoder target bitrate in kbps.
pub const VIDEO_BITRATE_KBPS: u32 = 2500;

pub struct CameraPipeline {
    pub pipeline: gst::Pipeline,
    pub webrtcbin: gst::Element,
}

impl CameraPipeline {
    pub fn new() -> Result<Self> {
        let pipeline_str = format!(
            "mfvideosrc device-index=0 ! \
             videoconvert ! \
             video/x-raw,width={width},height={height},framerate={fps}/1 ! \
             mfh264enc bitrate={bitrate} low-latency=true ! \
             h264parse config-interval=-1 ! \
             rtph264pay pt=96 mtu=1200 aggregate-mode=zero-latency ! \
             application/x-rtp,media=video,encoding-name=H264,payload=96 ! \
             webrtcbin name=webrtcbin bundle-policy=max-bundle \
                       stun-server=stun://stun.l.google.com:19302",
            width = VIDEO_WIDTH,
            height = VIDEO_HEIGHT,
            fps = VIDEO_FRAMERATE,
            bitrate = VIDEO_BITRATE_KBPS,
        );

        let pipeline = gst::parse::launch(&pipeline_str)
            .context(
                "Failed to build GStreamer pipeline. \
                 Verify GStreamer is installed and mfvideosrc / mfh264enc are available.",
            )?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("Parsed element is not a Pipeline"))?;

        let webrtcbin = pipeline
            .by_name("webrtcbin")
            .context("webrtcbin element missing from pipeline")?;

        Ok(Self {
            pipeline,
            webrtcbin,
        })
    }

    /// Move the pipeline into the Playing state.
    pub fn start(&self) -> Result<()> {
        self.pipeline
            .set_state(gst::State::Playing)
            .context("Failed to start pipeline")?;
        Ok(())
    }
}

impl Drop for CameraPipeline {
    fn drop(&mut self) {
        // Best-effort teardown — releases the camera and encoder.
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
