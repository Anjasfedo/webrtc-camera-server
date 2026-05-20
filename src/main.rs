//! WebRTC camera streaming server.
//!
//! Pipeline: Windows camera (Media Foundation) -> H.264 hardware encoder
//!           -> RTP packetizer -> webrtcbin -> browser/Flutter client.

use std::net::SocketAddr;

use anyhow::Result;
use axum::{Router, routing::get};
use tower_http::{cors::CorsLayer, services::ServeDir};
use tracing::info;

mod peer;
mod pipeline;
mod signaling;

#[tokio::main]
async fn main() -> Result<()> {
    // Logging. Tweak with `RUST_LOG=webrtc_camera_server=debug` for more detail.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "webrtc_camera_server=info,tower_http=info".into()),
        )
        .init();

    // GStreamer must be initialized before any pipeline is built.
    gstreamer::init()?;
    info!(
        "GStreamer initialized (version {})",
        gstreamer::version_string()
    );

    // Routes:
    //   GET /ws  -> WebSocket signaling
    //   GET /*   -> static test client (test-client/index.html)
    let app = Router::new()
        .route("/ws", get(signaling::ws_handler))
        .fallback_service(ServeDir::new("test-client"))
        .layer(CorsLayer::permissive());

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("Listening on http://{} (open in browser to test)", addr);

    axum::serve(listener, app).await?;
    Ok(())
}
