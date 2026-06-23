//! Structured file logging: one JSON object per line (JSONL), rotated daily.
//!
//! Logs are written to `logs/server.<YYYY-MM-DD>.jsonl` (a new file each day at
//! UTC midnight). Console output is intentionally quiet — everything goes to the
//! files — so a single `println!` at startup tells the operator where to look.

use std::path::Path;

use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;

/// Initialize the global tracing subscriber to write JSONL to `logs/`.
///
/// Returns a [`WorkerGuard`] that MUST be kept alive for the lifetime of the
/// program — dropping it flushes and stops the background writer thread, so
/// store it in a binding that lives until `main` returns.
pub fn init() -> Result<WorkerGuard> {
    let log_dir = Path::new("logs");
    std::fs::create_dir_all(log_dir).context("Failed to create logs/ directory")?;

    // Daily rotation -> logs/server.YYYY-MM-DD.jsonl
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("server")
        .filename_suffix("jsonl")
        .build(log_dir)
        .context("Failed to build rolling file appender")?;

    let (non_blocking, guard) = tracing_appender::non_blocking(appender);

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "webrtc_camera_server=info,tower_http=info".into());

    tracing_subscriber::fmt()
        .json()
        .with_current_span(true)
        .with_span_list(false)
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_env_filter(filter)
        .init();

    println!("Logging to {}/server.<date>.jsonl (JSONL)", log_dir.display());
    Ok(guard)
}
