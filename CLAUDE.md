# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Rust server that captures a single camera, H.264-encodes it once, and fans the
encoded stream out to many WebRTC browser clients. The camera and encoder run
continuously from server boot; clients tap into the live stream on connect.

## Commands

```bash
cargo run                 # build + run server, listens on 0.0.0.0:8090
cargo build --release     # optimized build (LTO thin, codegen-units=1)
cargo check               # fast type-check
RUST_LOG=webrtc_camera_server=debug cargo run   # verbose logging (default is info)
WCS_PORT=9000 cargo run                          # override config via env (see config.rs)
```

No tests exist. Manual testing: run the server, open `http://localhost:8090`
(served from `templates/`), click **Start Stream**. Health probes:
`GET /healthz` (liveness, always 200) and `GET /readyz` (200 when the pipeline
is live, else 503).

GStreamer must be installed on the host (with the plugins for the configured
source/encoder) and `gstreamer::init()` must succeed at startup, or the server
exits immediately.

### Configuration (env vars, all optional — see `config.rs`)

| Env var               | Default                          | Purpose                        |
|-----------------------|----------------------------------|--------------------------------|
| `WCS_BIND`            | `0.0.0.0`                        | bind address                   |
| `WCS_PORT`            | `8090`                           | listen port                    |
| `WCS_STUN`            | `stun://stun.l.google.com:19302` | STUN server for `webrtcbin`    |
| `WCS_EMULATOR_LAN_IP` | (unset → workaround off)         | Android-emulator ICE rewrite   |
| `WCS_STATIC_DIR`      | `templates`                      | static file dir                |
| `RUST_LOG`            | `webrtc_camera_server=info,...`  | log filter                     |

A present-but-unparseable value (e.g. non-numeric `WCS_PORT`) fails fast at
startup rather than being silently ignored. A local `.env` (gitignored) is
auto-loaded at startup via `dotenvy` — copy `.env.example` to `.env`; real
environment variables override it.

### Docker / deploy

The `Dockerfile` is a multi-stage build (Rust builder → `debian:bookworm-slim`
runtime with the GStreamer plugins the pipeline needs). It targets **Linux**, so
the pipeline uses the `v4l2src`/`x264enc` head — the Windows `mfvideosrc` head is
dev-only and never runs in the container.

```bash
docker build -t webrtc-camera-server .
# Linux host with a camera at /dev/video0:
docker run --rm -p 8090:8090 --device /dev/video0 --group-add video webrtc-camera-server
# or:
docker compose up --build
```

Camera passthrough (`--device /dev/video0`, `--group-add video`) needs a **Linux
Docker host** — it does not work on Docker Desktop for Windows/Mac (no
`/dev/videoN`). Build the image anywhere; run it where the camera is. The image
runs as non-root (uid 10001, in the `video` group). `getent group video` on the
host to confirm the gid if `--group-add video` by name doesn't resolve.

## Architecture

The core idea is **one shared pipeline, N dynamic per-peer branches**. This
avoids re-capturing/re-encoding per client.

- `config.rs` — `Config::from_env()` reads all runtime config (bind/port/STUN/
  emulator LAN IP/static dir) from env vars with defaults; lives in `AppState`.
  Parse errors fail fast at startup.

- `camera.rs` — `probe_cameras(only_api)` enumerates ALL connected cameras via
  `GstDeviceMonitor` (`Video/Source` class), keeping only those whose
  `device.api` matches (e.g. "mediafoundation" on Windows, to match `mfvideosrc`,
  so the same physical camera isn't listed twice under different backends).
  Returns `Vec<CameraDevice>`, each with `id` (the `device.path`, fed to
  `mfvideosrc device-path=`), `name`, `api`, and `caps: Vec<CameraCap>` — one
  concrete `(format, media, width, height, framerate)` per advertised mode.
  width/height/framerate may be fixed, a range, or a list; ranges flatten to
  their max. This is the ONLY source of truth for the form's device/resolution/
  fps options — no hardcoded guesses. Returns an empty vec (not an error) if no
  camera is found, so the server still boots.

- `pipeline.rs` — `SharedPipeline` holds the always-on capture+encode chain
  (ending in a `tee`) behind a `Mutex<Inner>`, plus the probed `devices`.
  `attach_peer()` requests a new tee src pad and builds a `tee -> queue ->
  webrtcbin` branch, returning a `PeerBranch`. The tee uses
  `allow-not-linked=true` so the shared chain keeps running with zero peers.
  Branch lifecycle is RAII: `PeerBranch::drop` unlinks the pad, nulls the
  elements, removes them, and releases the tee request pad. On attach it sends a
  `GstForceKeyUnit` event so the new peer gets an immediate IDR keyframe.
  `VideoConfig` (width/height/framerate/bitrate_kbps/format/device_id) is mutable
  at runtime: `reconfigure()` rejects an unknown `device_id`, then validates the
  config **against the SELECTED device's caps** (`caps_for`; the requested
  resolution+framerate must be a real mode of that camera); it returns
  `Ok(false)` (a no-op, pipeline untouched) when the config is unchanged, else
  builds + starts a REPLACEMENT pipeline before swapping it in (so a bad config
  leaves the live stream intact) and returns `Ok(true)`. `device_id` is the
  camera to capture from (the chosen `mfvideosrc device-path=`, escaped by
  `device_path_arg`; on Linux it's the v4l2 device path). `new()` defaults to the
  first device + one of its real modes (`pick_default_config`). A real swap
  orphans every existing `PeerBranch` — callers must drop peers and reconnect.

- `signaling.rs` — Axum WebSocket handler over shared `AppState` (`SharedPipeline`
  + a `broadcast::Sender<()>` for reconfigure notifications). One `handle_socket`
  per client holds an `Option<Peer>`. JSON messages are the `SignalingMessage`
  enum (snake_case tagged): client→server `start`/`answer`/`ice_candidate`/
  `get_config`/`set_config`/`get_devices`; server→client `offer`/
  `ice_candidate`/`config`/`devices`/`reconfigured`/`error`. A
  per-connection mpsc channel carries outbound messages to the WS sink via a
  forward task; a second task bridges the reconfigure broadcast into that same
  channel. `start` attaches a branch; `set_config` rebuilds the pipeline, drops
  this peer, replies `config`, and broadcasts `reconfigured` so ALL clients
  reconnect — UNLESS the config is unchanged, in which case it just echoes
  `config` and leaves peers streaming. `get_devices` replies with the connected
  cameras and each one's modes.

- `peer.rs` — `Peer` wraps a `PeerBranch` and wires `webrtcbin` signals
  (`on-negotiation-needed` → create/send offer, `on-ice-candidate` → forward).
  **The server is the offerer**; the browser answers. `handle_answer` /
  `handle_ice_candidate` apply the browser's responses to webrtcbin.

- `logging.rs` — `init()` sets up the global `tracing` subscriber to write
  **JSONL** (one JSON object per line) to `logs/server.<YYYY-MM-DD>.jsonl`,
  rotated DAILY at UTC midnight (`tracing-appender`). Console is intentionally
  quiet (file-only); a single `println!` at startup points to the log dir. It
  returns a `WorkerGuard` that `main` MUST keep alive (`_log_guard`) — dropping
  it stops the background writer and loses buffered lines. Filter via `RUST_LOG`
  (default `webrtc_camera_server=info,tower_http=info`). `logs/` is gitignored.

- `main.rs` — call `logging::init()` (hold the returned guard), load `Config`,
  init GStreamer, build+start the shared pipeline, wrap it + config in
  `AppState`, mount the Axum router (`/ws` + `/healthz` + `/readyz` + static dir,
  permissive CORS), and spawn `metrics_loop` (CPU/RAM/threads every 5s via
  `sysinfo`). `axum::serve(...).with_graceful_shutdown(shutdown_signal())` serves
  until SIGTERM (Unix) / Ctrl-C, then drains and exits cleanly — dropping
  `AppState` nulls the pipeline (its `Drop`), and `_log_guard` flushes. Startup
  no longer panics: the PID fetch and signal-handler installs are handled, not
  `.expect()`ed.

- `templates/index.html` — self-contained browser client (no build step). On
  load it opens the WS, requests `get_devices` (to build the camera + resolution
  + frame-rate dropdowns from REAL device caps) then `get_config` (to select the
  live values). The camera, resolution, and fps `<select>`s start empty and are
  populated at runtime; selecting a camera rebuilds resolution from THAT device's
  caps, and selecting a resolution filters fps to what that camera supports at
  that resolution. **Apply** sends `set_config`; on the
  `reconfigured` broadcast it tears down and reconnects the peer. Renders a live
  stats overlay (FPS, bitrate, resolution, RTT) from `getStats()`.

### Signaling flow

```
browser: WS connect -> {type:get_devices}       (builds camera/res/fps dropdowns)
server:  -> {type:devices, devices:[...]}       (cameras + per-device modes)
browser: -> {type:get_config}                   (selects live values)
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
  `mfvideosrc` (pinned to a native res@fps) → `videoconvert` → `mfh264enc`. Edit
  there to change source/encoder; the parse/pay/tee tail is shared in
  `build_pipeline`. There is NO `videoscale` — the camera runs its real
  resolution (validated against probed caps), not a scaled approximation.

- **mfh264enc only accepts NV12**: on Windows the MF H.264 encoder rejects
  `I420`/`BGRA` raw caps — it links only against `NV12`. `VideoConfig::default`
  picks the format per platform (`default_format()`: NV12 on Windows, I420 on
  Linux). The form's format dropdown still offers I420/YUY2/BGRA, but selecting
  one of those on Windows makes `reconfigure` fail (server replies `error`, live
  stream untouched) — don't widen the allowed list without a per-platform guard.

- **Emulator LAN-IP workaround** (`peer.rs::emulator_aliases`): when
  `WCS_EMULATOR_LAN_IP` is set, ICE candidates containing that IP are duplicated
  with it rewritten to `10.0.2.2` (the qemu host alias) so an Android emulator
  peer can route to the host. Unset by default → disabled (no rewrite). It is no
  longer a hardcoded constant; set the env var to your dev box's LAN IP to use it.

- **Video params** live in `VideoConfig` (`pipeline.rs`). The default mode is
  chosen from real caps at boot (`pick_default_config`, prefers 1080p30).
  Changeable at runtime via `set_config`; `VideoConfig::validate` requires the
  resolution+framerate to be a real probed camera mode (when caps are non-empty),
  plus fps 1..=240, bitrate 1..=100000, format in a fixed allow-list. `format`
  is the ENCODER-INPUT format (videoconvert bridges from the camera's native
  format) — not necessarily a format the camera advertises.

- **Device selection** is via `VideoConfig::device_id` = `CameraDevice::id` (the
  `device.path`). The MF path has backslashes + `#`/`{}` that `gst::parse::launch`
  mis-tokenizes, so `device_path_arg` quotes + escapes it before interpolating
  into `mfvideosrc device-path="..."`. `caps_for` resolves the selected device's
  caps for validation, so each camera is validated against its OWN modes. Only
  `device.api == "mediafoundation"` devices are listed (Windows) to avoid the
  duplicate ksvideosrc legacy entry.

- **Probing is at boot only**: cameras are enumerated once in
  `SharedPipeline::new`. Hot-plugging / unplugging a camera at runtime won't
  update the list — restart the server. A `device_id` that vanished is rejected
  by `reconfigure` ("unknown camera device"). If probing returns empty (no camera
  / monitor failed), `validate` falls back to generic bounds and the client
  keeps whatever options it last had.

- Branch elements must be brought to `Playing` (`sync_state_with_parent`)
  **before** linking the tee pad, or data races the state change.
