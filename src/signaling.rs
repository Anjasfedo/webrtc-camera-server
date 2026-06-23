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
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info, warn};

use crate::camera::CameraDevice;
use crate::config::Config;
use crate::peer::Peer;
use crate::pipeline::{SharedPipeline, VideoConfig};

/// Shared application state handed to every WebSocket connection.
pub struct AppState {
    pub pipeline: SharedPipeline,
    pub config: Config,
    /// Fires whenever the pipeline is reconfigured, so every connected client
    /// can be told to tear down and reconnect to the rebuilt pipeline.
    pub reconfigured_tx: broadcast::Sender<()>,
}

impl AppState {
    pub fn new(pipeline: SharedPipeline, config: Config) -> Self {
        let (reconfigured_tx, _) = broadcast::channel(16);
        Self {
            pipeline,
            config,
            reconfigured_tx,
        }
    }
}

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
    /// Client asks for the current video config.
    GetConfig,
    /// Server's reply to `GetConfig`, also sent after a successful `SetConfig`.
    Config {
        config: VideoConfig,
    },
    /// Client asks which cameras are connected and what each supports.
    GetDevices,
    /// Server's reply to `GetDevices`: connected cameras, each with its modes.
    Devices {
        devices: Vec<CameraDevice>,
    },
    /// Client requests new video params; triggers a live pipeline rebuild.
    SetConfig {
        config: VideoConfig,
    },
    /// Broadcast to all clients after a rebuild: drop your peer and reconnect.
    Reconfigured,
    /// Server -> client error (bad config, rebuild failure, ...).
    Error {
        message: String,
    },
}

pub async fn ws_handler(State(state): State<Arc<AppState>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    info!("WebSocket client connected");

    let (mut ws_sink, mut ws_stream) = socket.split();
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<SignalingMessage>();

    // Bridge the broadcast (reconfigure notifications) into this connection's
    // outbound channel so the single writer task owns the sink.
    let mut reconfigured_rx = state.reconfigured_tx.subscribe();
    let reconfig_outbound = outbound_tx.clone();
    let reconfig_task = tokio::spawn(async move {
        while reconfigured_rx.recv().await.is_ok() {
            if reconfig_outbound.send(SignalingMessage::Reconfigured).is_err() {
                break;
            }
        }
    });

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
            SignalingMessage::Start => {
                match Peer::new(
                    &state.pipeline,
                    &state.config.stun_server,
                    state.config.emulator_lan_ip.clone(),
                    outbound_tx.clone(),
                ) {
                    Ok(p) => {
                        info!("Peer branch attached to shared pipeline");
                        peer = Some(p);
                    }
                    Err(e) => error!("Failed to attach peer branch: {e:?}"),
                }
            }
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
            SignalingMessage::GetConfig => {
                let config = state.pipeline.config();
                let _ = outbound_tx.send(SignalingMessage::Config { config });
            }
            SignalingMessage::GetDevices => {
                let devices = state.pipeline.devices();
                let _ = outbound_tx.send(SignalingMessage::Devices { devices });
            }
            SignalingMessage::SetConfig { config } => {
                info!("Reconfigure requested: {config:?}");
                match state.pipeline.reconfigure(config) {
                    Ok(true) => {
                        // Pipeline rebuilt: this peer's branch is now orphaned;
                        // drop it so the client gets a clean reconnect via the
                        // broadcast below.
                        peer = None;
                        let new_config = state.pipeline.config();
                        let _ = outbound_tx.send(SignalingMessage::Config {
                            config: new_config,
                        });
                        // Tell every client (including this one) to reconnect.
                        let _ = state.reconfigured_tx.send(());
                        info!("Pipeline reconfigured");
                    }
                    Ok(false) => {
                        // No change requested; pipeline untouched, peers keep
                        // streaming. Just echo the current config back.
                        let new_config = state.pipeline.config();
                        let _ = outbound_tx.send(SignalingMessage::Config {
                            config: new_config,
                        });
                        info!("Reconfigure skipped: config unchanged");
                    }
                    Err(e) => {
                        error!("Reconfigure failed: {e:?}");
                        let _ = outbound_tx.send(SignalingMessage::Error {
                            message: format!("Reconfigure failed: {e}"),
                        });
                    }
                }
            }
            SignalingMessage::Offer { .. } => {
                warn!("Received unexpected `offer` from client; ignoring");
            }
            SignalingMessage::Config { .. }
            | SignalingMessage::Devices { .. }
            | SignalingMessage::Reconfigured
            | SignalingMessage::Error { .. } => {
                warn!("Received server-only message from client; ignoring");
            }
        }
    }

    drop(peer);
    forward_task.abort();
    reconfig_task.abort();
    info!("WebSocket client disconnected; peer branch removed");
}
