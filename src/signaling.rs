//! WebSocket signaling. Holds state for one client connection.

use std::sync::Arc;

use axum::{
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::peer::Peer;
use crate::pipeline::SharedPipeline;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignalingMessage {
    Start,
    Offer {
        sdp: String,
    },
    Answer {
        sdp: String,
    },
    IceCandidate {
        candidate: String,
        sdp_mline_index: u32,
    },
}

pub async fn ws_handler(
    State(shared): State<Arc<SharedPipeline>>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, shared))
}

async fn handle_socket(socket: WebSocket, shared: Arc<SharedPipeline>) {
    info!("WebSocket client connected");

    let (mut ws_sink, mut ws_stream) = socket.split();
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<SignalingMessage>();

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
                break;
            }
        }
    });

    let mut peer: Option<Peer> = None;

    while let Some(Ok(frame)) = ws_stream.next().await {
        let text = match frame {
            Message::Text(t) => t,
            Message::Close(_) => break,
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
            SignalingMessage::Start => match Peer::new(&shared, outbound_tx.clone()) {
                Ok(p) => {
                    info!("Peer branch attached to shared pipeline");
                    peer = Some(p);
                }
                Err(e) => error!("Failed to attach peer branch: {e:?}"),
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
                warn!("Received unexpected `offer` from client; ignoring");
            }
        }
    }

    drop(peer);
    forward_task.abort();
    info!("WebSocket client disconnected; peer branch removed");
}
