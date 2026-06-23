# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Rust server that captures a single camera, H.264-encodes it once, and fans the
encoded stream out to many WebRTC browser clients. The camera and encoder run
continuously from server boot; clients tap into the live stream on connect.

## Commands

```bash
cargo run                 # build + run server, listens on 0.0.0.0:8080
cargo build --release     # optimized build (LTO thin, codegen-units=1)
cargo check               # fast type-check
RUST_LOG=webrtc_camera_server=debug cargo run   # verbose logging (default is info)
```

No tests exist. Manual testing: run the server, open `http://localhost:8080`
(served from `test-client/`), click **Start Stream**.

GStreamer must be installed on the host (with the plugins for the configured
source/encoder) and `gstreamer::init()` must succeed at startup, or the server
exits immediately.

## Architecture

The core idea is **one shared pipeline, N dynamic per-peer branches**. This
avoids re-capturing/re-encoding per client.

- `pipeline.rs` — `SharedPipeline` owns the always-on capture+encode chain
  ending in a `tee`. `attach_peer()` requests a new tee src pad and builds a
  `tee -> queue -> webrtcbin` branch, returning a `PeerBranch`. The tee uses
  `allow-not-linked=true` so the shared chain keeps running with zero peers.
  Branch lifecycle is RAII: `PeerBranch::drop` unlinks the pad, nulls the
  elements, removes them, and releases the tee request pad. On attach it sends
  a `GstForceKeyUnit` event so the new peer gets an immediate IDR keyframe
  instead of waiting for the next periodic one.

- `signaling.rs` — Axum WebSocket handler. One `handle_socket` per client
  connection holds an `Option<Peer>`. JSON messages are the `SignalingMessage`
  enum (`start`, `offer`, `answer`, `ice_candidate`, snake_case tagged). A
  per-connection mpsc channel carries outbound messages from GStreamer signal
  callbacks to the WebSocket sink via a spawned forward task. `start` creates
  the peer (attaches a branch); dropping the socket drops the peer (removes the
  branch).

- `peer.rs` — `Peer` wraps a `PeerBranch` and wires `webrtcbin` signals
  (`on-negotiation-needed` → create/send offer, `on-ice-candidate` → forward).
  **The server is the offerer**; the browser answers. `handle_answer` /
  `handle_ice_candidate` apply the browser's responses to webrtcbin.

- `main.rs` — init tracing + GStreamer, build+start the shared pipeline, mount
  the Axum router (`/ws` + static `test-client/`, permissive CORS), and spawn a
  background task that logs this process's CPU/RAM/thread count every 5s via
  `sysinfo`.

- `test-client/index.html` — self-contained browser client (no build step). It
  answers the server's offer and renders a live stats overlay (FPS, bitrate,
  resolution, RTT) from `RTCPeerConnection.getStats()`.

### Signaling flow

```
browser: WS connect -> {type:start}
server:  attach_peer -> webrtcbin fires on-negotiation-needed
server:  -> {type:offer, sdp}
browser: setRemoteDescription(offer) -> createAnswer -> {type:answer, sdp}
both:    trickle {type:ice_candidate} as candidates appear
```

## Gotchas

- **Doc comment vs reality**: the module doc in `pipeline.rs` describes an
  `mfvideosrc/mfh264enc` (Media Foundation, Windows) chain, but the actual
  `gst::parse::launch` string uses `v4l2src device=/dev/video0` + `x264enc`
  (Linux/V4L2). If you change the capture source or encoder, edit the launch
  string in `SharedPipeline::new` — and fix the stale comment.

- **Hardcoded LAN IP**: `peer.rs::emulator_aliases` rewrites the server's LAN IP
  (`192.168.1.103`) to `10.0.2.2` so an Android emulator peer can route to the
  host. This is a hardcoded constant — update it on network change, or it
  silently does nothing.

- **Video params** are `const`s at the top of `pipeline.rs`
  (`VIDEO_WIDTH/HEIGHT/FRAMERATE/BITRATE_KBPS`: 1920x1080, 30fps, 6000kbps).

- Branch elements must be brought to `Playing` (`sync_state_with_parent`)
  **before** linking the tee pad, or data races the state change.
