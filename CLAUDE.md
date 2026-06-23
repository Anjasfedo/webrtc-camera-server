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

- `pipeline.rs` — `SharedPipeline` holds the always-on capture+encode chain
  (ending in a `tee`) behind a `Mutex<Inner>`. `attach_peer()` requests a new
  tee src pad and builds a `tee -> queue -> webrtcbin` branch, returning a
  `PeerBranch`. The tee uses `allow-not-linked=true` so the shared chain keeps
  running with zero peers. Branch lifecycle is RAII: `PeerBranch::drop` unlinks
  the pad, nulls the elements, removes them, and releases the tee request pad.
  On attach it sends a `GstForceKeyUnit` event so the new peer gets an immediate
  IDR keyframe instead of waiting for the next periodic one.
  `VideoConfig` (width/height/framerate/bitrate_kbps/format) is mutable at
  runtime: `reconfigure()` validates the new config, builds + starts a
  REPLACEMENT pipeline first, then swaps it in (so a bad config leaves the live
  stream intact). The swap orphans every existing `PeerBranch` — callers must
  drop peers and have clients reconnect.

- `signaling.rs` — Axum WebSocket handler over shared `AppState` (`SharedPipeline`
  + a `broadcast::Sender<()>` for reconfigure notifications). One `handle_socket`
  per client holds an `Option<Peer>`. JSON messages are the `SignalingMessage`
  enum (snake_case tagged): client→server `start`/`answer`/`ice_candidate`/
  `get_config`/`set_config`; server→client `offer`/`ice_candidate`/`config`/
  `reconfigured`/`error`. A per-connection mpsc channel carries outbound
  messages to the WS sink via a forward task; a second task bridges the
  reconfigure broadcast into that same channel. `start` attaches a branch;
  `set_config` rebuilds the pipeline, drops this peer, replies `config`, and
  broadcasts `reconfigured` so ALL clients reconnect.

- `peer.rs` — `Peer` wraps a `PeerBranch` and wires `webrtcbin` signals
  (`on-negotiation-needed` → create/send offer, `on-ice-candidate` → forward).
  **The server is the offerer**; the browser answers. `handle_answer` /
  `handle_ice_candidate` apply the browser's responses to webrtcbin.

- `main.rs` — init tracing + GStreamer, build+start the shared pipeline, wrap it
  in `AppState`, mount the Axum router (`/ws` + static `templates/`, permissive
  CORS), and spawn a background task that logs this process's CPU/RAM/thread
  count every 5s via `sysinfo`.

- `templates/index.html` — self-contained browser client (no build step). Opens
  the WS on load and syncs a config form (resolution / fps / bitrate / format)
  from the server via `get_config`. **Apply** sends `set_config`; on the
  `reconfigured` broadcast it tears down and reconnects the peer. Renders a live
  stats overlay (FPS, bitrate, resolution, RTT) from `getStats()`.

### Signaling flow

```
browser: WS connect -> {type:get_config}      (syncs the form)
browser: click Start -> {type:start}
server:  attach_peer -> webrtcbin fires on-negotiation-needed
server:  -> {type:offer, sdp}
browser: setRemoteDescription(offer) -> createAnswer -> {type:answer, sdp}
both:    trickle {type:ice_candidate} as candidates appear

reconfigure (any client):
browser: {type:set_config, config} -> server rebuilds pipeline
server:  -> {type:config} to requester; {type:reconfigured} broadcast to all
browsers: drop peer, reconnect with {type:start}
```

## Gotchas

- **Platform-specific capture head**: `pipeline.rs::capture_encode_head` is
  `#[cfg]`-split — Linux `v4l2src` (MJPEG) → `jpegdec` → `x264enc`; Windows
  `mfvideosrc` → `videoconvert`/`videoscale` → `mfh264enc`. Edit there to change
  source/encoder; the parse/pay/tee tail is shared in `build_pipeline`.

- **mfh264enc only accepts NV12**: on Windows the MF H.264 encoder rejects
  `I420`/`BGRA` raw caps — it links only against `NV12`. `VideoConfig::default`
  picks the format per platform (`default_format()`: NV12 on Windows, I420 on
  Linux). The form's format dropdown still offers I420/YUY2/BGRA, but selecting
  one of those on Windows makes `reconfigure` fail (server replies `error`, live
  stream untouched) — don't widen the allowed list without a per-platform guard.

- **Hardcoded LAN IP**: `peer.rs::emulator_aliases` rewrites the server's LAN IP
  (`192.168.1.103`) to `10.0.2.2` so an Android emulator peer can route to the
  host. This is a hardcoded constant — update it on network change, or it
  silently does nothing.

- **Video params** live in `VideoConfig` (`pipeline.rs`), defaulting to
  1920x1080, 30fps, 6000kbps. Changeable at runtime via `set_config`; validated
  in `VideoConfig::validate` (≤8K, fps 1..=240, bitrate 1..=100000, format in a
  fixed allow-list).

- Branch elements must be brought to `Playing` (`sync_state_with_parent`)
  **before** linking the tee pad, or data races the state change.
