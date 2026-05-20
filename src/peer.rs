//! Per-peer WebRTC state.
//!
//! One `Peer` per connected client. Owns the GStreamer pipeline and wires
//! `webrtcbin` callbacks (`on-negotiation-needed`, `on-ice-candidate`) into
//! the signaling channel back to the WebSocket.
//!
//! Negotiation flow:
//!   1. Pipeline starts -> webrtcbin emits `on-negotiation-needed`.
//!   2. We call `create-offer`, get back an SDP, send it to the client.
//!   3. Client responds with `Answer` -> we set it as the remote description.
//!   4. ICE candidates flow both ways in parallel.

use anyhow::{Result, anyhow};
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_sdp as gst_sdp;
use gstreamer_webrtc as gst_webrtc;
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use crate::pipeline::CameraPipeline;
use crate::signaling::SignalingMessage;

pub struct Peer {
    pipeline: CameraPipeline,
}

impl Peer {
    /// Build a new pipeline and connect the signaling callbacks.
    /// `outbound` is the channel used to send messages to the WebSocket client.
    pub fn new(outbound: mpsc::UnboundedSender<SignalingMessage>) -> Result<Self> {
        let pipeline = CameraPipeline::new()?;
        wire_webrtc_signals(&pipeline.webrtcbin, outbound);
        Ok(Self { pipeline })
    }

    pub fn start(&self) -> Result<()> {
        self.pipeline.start()
    }

    /// Apply an SDP answer received from the client.
    pub fn handle_answer(&self, sdp: &str) -> Result<()> {
        let sdp_msg = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes())
            .map_err(|_| anyhow!("Failed to parse answer SDP"))?;
        let answer =
            gst_webrtc::WebRTCSessionDescription::new(gst_webrtc::WebRTCSDPType::Answer, sdp_msg);
        self.pipeline
            .webrtcbin
            .emit_by_name::<()>("set-remote-description", &[&answer, &None::<gst::Promise>]);
        Ok(())
    }

    /// Add a remote ICE candidate received from the client.
    pub fn handle_ice_candidate(&self, candidate: &str, sdp_mline_index: u32) -> Result<()> {
        self.pipeline
            .webrtcbin
            .emit_by_name::<()>("add-ice-candidate", &[&sdp_mline_index, &candidate]);
        Ok(())
    }
}

/// Hook up webrtcbin's signals so the offer + local ICE candidates get pushed
/// out over the signaling channel.
fn wire_webrtc_signals(
    webrtcbin: &gst::Element,
    outbound: mpsc::UnboundedSender<SignalingMessage>,
) {
    // `on-negotiation-needed` fires once the pipeline is ready to create an SDP.
    let tx = outbound.clone();
    webrtcbin.connect_closure(
        "on-negotiation-needed",
        false,
        glib::closure!(move |webrtc: &gst::Element| {
            if let Err(e) = create_and_send_offer(webrtc, tx.clone()) {
                error!("create-offer failed: {e:?}");
            }
        }),
    );

    // Each locally-gathered ICE candidate is forwarded to the client.
    let tx = outbound.clone();
    webrtcbin.connect_closure(
        "on-ice-candidate",
        false,
        glib::closure!(
            move |_webrtc: &gst::Element, mline_index: u32, candidate: &str| {
                let msg = SignalingMessage::IceCandidate {
                    candidate: candidate.to_string(),
                    sdp_mline_index: mline_index,
                };
                if tx.send(msg).is_err() {
                    warn!("Signaling channel closed; ICE candidate dropped");
                }
            }
        ),
    );
}

/// Trigger `create-offer`, then `set-local-description` and forward the SDP
/// to the client over the signaling channel.
fn create_and_send_offer(
    webrtcbin: &gst::Element,
    tx: mpsc::UnboundedSender<SignalingMessage>,
) -> Result<()> {
    let webrtcbin_inner = webrtcbin.clone();

    // The promise fires when webrtcbin has produced the offer.
    let promise = gst::Promise::with_change_func(move |reply| {
        let reply = match reply {
            Ok(Some(r)) => r,
            Ok(None) => {
                error!("create-offer returned no reply");
                return;
            }
            Err(e) => {
                error!("create-offer error: {e:?}");
                return;
            }
        };

        let offer = match reply.get::<gst_webrtc::WebRTCSessionDescription>("offer") {
            Ok(o) => o,
            Err(e) => {
                error!("offer field missing: {e:?}");
                return;
            }
        };

        // Apply locally...
        webrtcbin_inner
            .emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);

        // ...then send to the client.
        let sdp = offer.sdp().as_text().unwrap_or_default();
        debug!("Sending offer SDP ({} bytes)", sdp.len());
        let _ = tx.send(SignalingMessage::Offer { sdp });
    });

    webrtcbin.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);

    Ok(())
}
