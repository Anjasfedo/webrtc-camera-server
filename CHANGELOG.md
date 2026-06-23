# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Production-readiness and configurability work. Suggested release: **0.2.0**.

### Added

- **Runtime video configuration** — resolution, frame rate, bitrate, and raw
  format are changeable live via the `set_config` WebSocket message; the shared
  pipeline is rebuilt in place (replacement built and started before the old one
  is torn down, so a bad config leaves the live stream intact). A no-op guard
  skips the rebuild when the requested config is unchanged.
- **Camera selection** — connected cameras are enumerated at startup
  (`GstDeviceMonitor`); the client lists them via `get_devices` and can switch
  cameras live. On Windows only `mediafoundation` devices are listed to avoid
  duplicate legacy entries.
- **Real capability probing** — resolution / frame-rate / format options are
  built from each camera's actual advertised capture modes (`get_devices` →
  per-device caps), not hardcoded guesses. Requests are validated against the
  selected camera's real modes.
- **Platform-adaptive pipeline** — Linux uses `v4l2src` + `x264enc`; Windows
  uses `mfvideosrc` + `mfh264enc`, selected at compile time.
- **Structured logging** — daily-rotated JSONL logs at
  `logs/server.<YYYY-MM-DD>.jsonl` (`tracing` + `tracing-appender`).
- **Environment configuration** — `WCS_BIND`, `WCS_PORT`, `WCS_STUN`,
  `WCS_EMULATOR_LAN_IP`, `WCS_STATIC_DIR`, `WCS_TEST_SOURCE`, `RUST_LOG`. A local
  `.env` is auto-loaded (`dotenvy`); see `.env.example`. Unparseable values fail
  fast at startup.
- **Health endpoints** — `GET /healthz` (liveness) and `GET /readyz` (200 when
  the pipeline is live, else 503).
- **Graceful shutdown** — drains on SIGTERM (Unix) / Ctrl-C, then tears the
  pipeline down cleanly and flushes logs.
- **Synthetic test source** — `WCS_TEST_SOURCE=1` swaps the camera for
  `videotestsrc`, so the server boots with no camera (containers, CI, Docker
  Desktop on Windows/Mac).
- **Docker support** — multi-stage `Dockerfile` (GStreamer runtime plugins),
  `docker-compose.yml` (camera passthrough on Linux), `.dockerignore`, and a
  `justfile` (`build`/`run`/`run-test`/`up`/`logs`/`stop`/`clean`/`test`) with a
  PowerShell smoke test.

### Changed

- The emulator LAN-IP ICE workaround is now opt-in via `WCS_EMULATOR_LAN_IP`
  instead of a hardcoded constant; disabled by default.
- Server bind address and port are configurable (previously hardcoded
  `0.0.0.0:8090`).
- Process CPU/RAM/thread metrics are emitted as structured log fields.

### Fixed

- Startup no longer panics on a failed PID fetch or signal-handler install
  (handled gracefully instead of `.expect()`).
- Container can write its log directory: the image creates `/app/logs` owned by
  the non-root runtime user.

## [0.1.0] - 2026-05-22

### Added

- Initial WebRTC camera streaming server built on GStreamer: one shared
  capture + encode pipeline fanned out to many browser peers via a `tee` and
  per-peer `webrtcbin` branches.
- WebSocket signaling (server is the offerer; browser answers) with trickle ICE.
- Multi-peer support — each client taps the shared encoded stream.
- Self-contained browser test client with a live stats overlay (FPS, bitrate,
  resolution, latency).
- Periodic system metrics logging.

[Unreleased]: https://example.com/compare/v0.1.0...HEAD
[0.1.0]: https://example.com/releases/tag/v0.1.0
