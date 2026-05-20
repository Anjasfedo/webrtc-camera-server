//! WebSocket signaling between the browser/Flutter client and the server.
//!
//! Wire format is JSON. A `type` field discriminates the variant. The protocol
//! is intentionally tiny:
//!
//!   client -> server:  { "type": "start" }
//!   server -> client:  { "type": "offer", "sdp": "..." }
//!   client -> server:  { "type": "answer", "sdp": "..." }
//!   both ways:         { "type": "ice_candidate", "candidate": "...",
//!                        "sdp_mline_index": 0 }

use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::peer::Peer;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignalingMessage {
    /// Client -> Server: request the server to start streaming and begin
    /// negotiation. The server then sends an `Offer`.
    Start,

    /// Server -> Client: SDP offer.
    Offer { sdp: String },

    /// Client -> Server: SDP answer.
    Answer { sdp: String },

    /// Bi-directional: a single ICE candidate.
    IceCandidate {
        candidate: String,
        sdp_mline_index: u32,
    },
}

pub async fn ws_handler(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(handle_socket)
}

async fn handle_socket(socket: WebSocket) {
    info!("WebSocket client connected");

    let (mut ws_sink, mut ws_stream) = socket.split();

    // Channel used by GStreamer callbacks (running on the streaming thread)
    // to push signaling messages out to this WebSocket.
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<SignalingMessage>();

    // Forward outbound messages from the channel to the WebSocket.
    let forward_task = tokio::spawn(async move {
        while let Some(msg) = outbound_rx.recv().await {
            let text = match serde_json::to_string(&msg) {
                Ok(t) => t,
                Err(e) => {
                    error!("Failed to serialize signaling message: {e:?}");
                    continue;
                }
            };
            if ws_sink.send(Message::Text(text)).await.is_err() {
                break; // client disconnected
            }
        }
    });

    let mut peer: Option<Peer> = None;

    while let Some(Ok(frame)) = ws_stream.next().await {
        let text = match frame {
            Message::Text(t) => t,
            Message::Close(_) => break,
            // Ignore binary, ping, pong.
            _ => continue,
        };

        let msg: SignalingMessage = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                warn!("Malformed signaling message: {e:?}");
                continue;
            }
        };

        match msg {
            SignalingMessage::Start => match Peer::new(outbound_tx.clone()) {
                Ok(p) => {
                    if let Err(e) = p.start() {
                        error!("Pipeline failed to start: {e:?}");
                        continue;
                    }
                    info!("Pipeline started; awaiting SDP negotiation");
                    peer = Some(p);
                }
                Err(e) => error!("Failed to create peer: {e:?}"),
            },

            SignalingMessage::Answer { sdp } => match &peer {
                Some(p) => {
                    if let Err(e) = p.handle_answer(&sdp) {
                        error!("Failed to apply remote answer: {e:?}");
                    }
                }
                None => warn!("Got `answer` before `start`"),
            },

            SignalingMessage::IceCandidate {
                candidate,
                sdp_mline_index,
            } => match &peer {
                Some(p) => {
                    if let Err(e) = p.handle_ice_candidate(&candidate, sdp_mline_index) {
                        error!("Failed to add remote ICE candidate: {e:?}");
                    }
                }
                None => warn!("Got `ice_candidate` before `start`"),
            },

            SignalingMessage::Offer { .. } => {
                // The server is the offerer in this design.
                warn!("Received unexpected `offer` from client; ignoring");
            }
        }
    }

    // Dropping `peer` here tears down the pipeline via CameraPipeline::drop.
    drop(peer);
    forward_task.abort();
    info!("WebSocket client disconnected; pipeline torn down");
}
