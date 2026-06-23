//! WebRTC camera streaming server with a shared, always-on pipeline.

use std::net::SocketAddr;
use std::sync::Arc;

use std::time::Duration;
use sysinfo::System;

use anyhow::Result;
use axum::{Router, response::IntoResponse, routing::get};
use tower_http::{cors::CorsLayer, services::ServeDir};
use tracing::{error, info};

mod camera;
mod config;
mod logging;
mod peer;
mod pipeline;
mod signaling;

use crate::config::Config;
use crate::pipeline::SharedPipeline;
use crate::signaling::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    // Load a local `.env` if present (real env vars still win). Absent file is
    // fine — config has defaults. Must run before logging/config read env.
    match dotenvy::dotenv() {
        Ok(path) => eprintln!("Loaded env from {}", path.display()),
        Err(e) if e.not_found() => {}
        Err(e) => eprintln!("Warning: failed to load .env: {e}"),
    }

    // Hold the guard until main returns so the background log writer keeps
    // flushing. Dropping it early would silence/lose buffered log lines.
    let _log_guard = logging::init()?;

    let cfg = Config::from_env()?;
    info!(?cfg, "Configuration loaded");

    gstreamer::init()?;
    info!(
        "GStreamer initialized (version {})",
        gstreamer::version_string()
    );

    // Build and start the shared capture + encode pipeline. From here on the
    // camera is on and the encoder is running. New WebRTC clients tap in.
    let pipeline = SharedPipeline::new()?;
    pipeline.start()?;
    info!("Shared pipeline running; camera is live");

    let addr = SocketAddr::new(cfg.bind, cfg.port);
    let static_dir = cfg.static_dir.clone();
    let state = Arc::new(AppState::new(pipeline, cfg));

    let app = Router::new()
        .route("/ws", get(signaling::ws_handler))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(state.clone())
        .fallback_service(ServeDir::new(static_dir))
        .layer(CorsLayer::permissive());

    let metrics_handle = tokio::spawn(metrics_loop());

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("Listening on http://{}", addr);

    // Serve until SIGTERM / Ctrl-C, then shut down gracefully: stop accepting,
    // let in-flight requests finish. Dropping `state` afterwards tears down the
    // pipeline (its `Drop` nulls GStreamer), and `_log_guard` flushes the logs.
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("Shutdown signal received; stopping");
    metrics_handle.abort();
    Ok(())
}

/// Liveness probe: the process is up and serving. Always 200.
async fn healthz() -> impl IntoResponse {
    "ok"
}

/// Readiness probe: the shared pipeline is live (in Playing). Returns 200 when
/// ready, 503 otherwise so a load balancer holds traffic until the camera is up.
async fn readyz(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse {
    if state.pipeline.is_playing() {
        (axum::http::StatusCode::OK, "ready")
    } else {
        (axum::http::StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}

/// Resolves when the process receives SIGTERM (Unix) or Ctrl-C (any platform).
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            error!("Failed to install Ctrl-C handler: {e}");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => error!("Failed to install SIGTERM handler: {e}"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

/// Periodically log this process's CPU / RAM / thread count as structured fields.
async fn metrics_loop() {
    let mut sys = System::new_all();
    let pid = match sysinfo::get_current_pid() {
        Ok(p) => p,
        Err(e) => {
            // Non-fatal: skip process metrics rather than crashing the server.
            error!("Cannot get current PID; process metrics disabled: {e}");
            return;
        }
    };

    loop {
        sys.refresh_all();

        let hostname = System::host_name().unwrap_or_else(|| "unknown-host".to_string());

        let mut process_cpu = 0.0;
        let mut process_ram_mb = 0;
        let mut threads = 0;

        if let Some(process) = sys.process(pid) {
            process_cpu = process.cpu_usage();
            process_ram_mb = process.memory() / 1024 / 1024;
            threads = process.tasks().map(|t| t.len()).unwrap_or(0);
        }

        info!(
            host = %hostname,
            cpu = format!("{:.1}%", process_cpu),
            ram = format!("{} MB", process_ram_mb),
            threads = threads,
            "Server Metrics"
        );

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
