//! WebRTC camera streaming server with a shared, always-on pipeline.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::{Router, routing::get};
use tower_http::{cors::CorsLayer, services::ServeDir};
use tracing::info;

mod peer;
mod pipeline;
mod signaling;

use crate::pipeline::SharedPipeline;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "webrtc_camera_server=info,tower_http=info".into()),
        )
        .init();

    gstreamer::init()?;
    info!(
        "GStreamer initialized (version {})",
        gstreamer::version_string()
    );

    // Build and start the shared capture + encode pipeline. From here on the
    // camera is on and the encoder is running. New WebRTC clients tap in.
    let shared = Arc::new(SharedPipeline::new()?);
    shared.start()?;
    info!("Shared pipeline running; camera is live");

    let app = Router::new()
        .route("/ws", get(signaling::ws_handler))
        .with_state(shared.clone())
        .fallback_service(ServeDir::new("test-client"))
        .layer(CorsLayer::permissive());

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("Listening on http://{}", addr);

    axum::serve(listener, app).await?;
    Ok(())
}
