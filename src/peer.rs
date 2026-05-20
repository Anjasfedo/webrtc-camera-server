//! Per-peer WebRTC signaling, layered on top of the shared pipeline.

use anyhow::{Result, anyhow};
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_sdp as gst_sdp;
use gstreamer_webrtc as gst_webrtc;
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use crate::pipeline::{PeerBranch, SharedPipeline};
use crate::signaling::SignalingMessage;

pub struct Peer {
    _branch: PeerBranch,
}

impl Peer {
    /// Attach a new branch to the shared pipeline and wire WebRTC signals
    /// into the outbound signaling channel.
    pub fn new(
        shared: &SharedPipeline,
        outbound: mpsc::UnboundedSender<SignalingMessage>,
    ) -> Result<Self> {
        let branch = shared.attach_peer(|webrtcbin| {
            wire_webrtc_signals(webrtcbin, outbound);
        })?;
        Ok(Self { _branch: branch })
    }

    pub fn handle_answer(&self, sdp: &str) -> Result<()> {
        let sdp_msg = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes())
            .map_err(|_| anyhow!("Failed to parse answer SDP"))?;
        let answer =
            gst_webrtc::WebRTCSessionDescription::new(gst_webrtc::WebRTCSDPType::Answer, sdp_msg);
        self._branch
            .webrtcbin
            .emit_by_name::<()>("set-remote-description", &[&answer, &None::<gst::Promise>]);
        Ok(())
    }

    pub fn handle_ice_candidate(&self, candidate: &str, sdp_mline_index: u32) -> Result<()> {
        self._branch
            .webrtcbin
            .emit_by_name::<()>("add-ice-candidate", &[&sdp_mline_index, &candidate]);
        Ok(())
    }
}

fn wire_webrtc_signals(
    webrtcbin: &gst::Element,
    outbound: mpsc::UnboundedSender<SignalingMessage>,
) {
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

    let tx = outbound;
    webrtcbin.connect_closure(
        "on-ice-candidate",
        false,
        glib::closure!(
            move |_webrtc: &gst::Element, mline_index: u32, candidate: &str| {
                // Original candidate, as webrtcbin emitted it.
                let original = SignalingMessage::IceCandidate {
                    candidate: candidate.to_string(),
                    sdp_mline_index: mline_index,
                };
                if tx.send(original).is_err() {
                    warn!("Signaling channel closed; ICE candidate dropped");
                    return;
                }

                // Emulator workaround: also broadcast a copy with the server's
                // LAN IP rewritten to 10.0.2.2 (qemu's host alias). Real
                // browsers and physical devices ignore this; Android emulator
                // peers can route to it.
                for rewritten in emulator_aliases(candidate) {
                    let alias = SignalingMessage::IceCandidate {
                        candidate: rewritten,
                        sdp_mline_index: mline_index,
                    };
                    let _ = tx.send(alias);
                }
            }
        ),
    );
}

/// If the candidate string contains the server's LAN IP, produce a copy with
/// that IP swapped for 10.0.2.2 so an Android emulator peer can reach us.
/// Returns an empty vec for candidates that don't match (loopback, IPv6,
/// public srflx) — we don't want to spam the client with garbage.
fn emulator_aliases(candidate: &str) -> Vec<String> {
    // Hardcoded for now. If you ever change networks, update this.
    // (Alternatively, detect the primary IPv4 at startup and pass it in.)
    const SERVER_LAN_IP: &str = "192.168.1.103";
    const EMULATOR_HOST_ALIAS: &str = "10.0.2.2";

    if !candidate.contains(SERVER_LAN_IP) {
        return Vec::new();
    }
    vec![candidate.replace(SERVER_LAN_IP, EMULATOR_HOST_ALIAS)]
}

fn create_and_send_offer(
    webrtcbin: &gst::Element,
    tx: mpsc::UnboundedSender<SignalingMessage>,
) -> Result<()> {
    let webrtcbin_inner = webrtcbin.clone();

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

        webrtcbin_inner
            .emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);

        let sdp = offer.sdp().as_text().unwrap_or_default();
        debug!("Sending offer SDP ({} bytes)", sdp.len());
        let _ = tx.send(SignalingMessage::Offer { sdp });
    });

    webrtcbin.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
    Ok(())
}
