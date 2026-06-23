# syntax=docker/dockerfile:1

# ── Build stage ───────────────────────────────────────────────────────────────
# edition = "2024" needs Rust >= 1.85, so pull the latest 1.x toolchain.
FROM rust:1-bookworm AS builder

# GStreamer DEVELOPMENT headers for the -sys crates to compile against.
# plugins-bad provides the gstreamer-webrtc / gstreamer-sdp pkg-config files.
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
        libglib2.0-dev \
        libgstreamer1.0-dev \
        libgstreamer-plugins-base1.0-dev \
        libgstreamer-plugins-bad1.0-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Cache dependency builds: copy manifests, build a stub, then the real source.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
# Touch so cargo rebuilds the real main.rs over the cached stub.
RUN touch src/main.rs && cargo build --release

# ── Runtime stage ─────────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

# GStreamer RUNTIME plugins the pipeline needs:
#   base       — videoconvert, tee, queue, capsfilter
#   good       — v4l2src, jpegdec, rtph264pay, h264parse
#   bad        — webrtcbin (WebRTC transport)
#   ugly       — x264enc (H.264 software encoder)
#   libnice    — ICE for webrtcbin
RUN apt-get update && apt-get install -y --no-install-recommends \
        libgstreamer1.0-0 \
        libgstreamer-plugins-base1.0-0 \
        gstreamer1.0-plugins-base \
        gstreamer1.0-plugins-good \
        gstreamer1.0-plugins-bad \
        gstreamer1.0-plugins-ugly \
        gstreamer1.0-libav \
        gstreamer1.0-nice \
        wget \
    && rm -rf /var/lib/apt/lists/*

# Run as non-root. The camera device (/dev/video0) is group-owned by `video`,
# so add the app user to it; pass `--group-add video` at run time too.
RUN useradd --create-home --uid 10001 app \
    && usermod -aG video app

WORKDIR /app
COPY --from=builder /app/target/release/webrtc-camera-server /usr/local/bin/
COPY templates ./templates

# The server writes JSONL logs to ./logs at runtime. Create it and hand /app to
# the non-root user so it can write there (and rotate daily) without root.
RUN mkdir -p /app/logs && chown -R app:app /app

USER app

ENV WCS_BIND=0.0.0.0 \
    WCS_PORT=8090 \
    RUST_LOG=webrtc_camera_server=info,tower_http=info

EXPOSE 8090

# Liveness can be wired to /healthz by the orchestrator; this is a basic check.
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD wget -qO- http://127.0.0.1:8090/healthz || exit 1

ENTRYPOINT ["webrtc-camera-server"]
