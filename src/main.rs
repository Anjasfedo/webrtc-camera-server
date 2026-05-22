//! WebRTC camera streaming server with a shared, always-on pipeline.

use std::net::SocketAddr;
use std::sync::Arc;

use std::time::Duration;
use sysinfo::System;

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

    tokio::spawn(async move {
        let mut sys = System::new_all();
        let pid = sysinfo::get_current_pid().expect("Failed to get current PID");

        loop {
            // Refresh system and process data
            sys.refresh_all();

            let hostname = System::host_name().unwrap_or_else(|| "unknown-host".to_string());

            let mut process_cpu = 0.0;
            let mut process_ram_mb = 0;
            let mut threads = 0;

            if let Some(process) = sys.process(pid) {
                // Get CPU usage specifically for this Rust program
                process_cpu = process.cpu_usage();

                // process.memory() returns bytes, divide to get MB
                process_ram_mb = process.memory() / 1024 / 1024;

                threads = process.tasks().map(|t| t.len()).unwrap_or(0);
            }

            tracing::info!(
                host = %hostname,
                cpu = format!("{:.1}%", process_cpu),
                ram = format!("{} MB", process_ram_mb),
                threads = threads,
                "Server Metrics"
            );

            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("Listening on http://{}", addr);

    axum::serve(listener, app).await?;
    Ok(())
}
