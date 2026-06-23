# Justfile for building / running / testing the Docker image.
# Run `just` or `just --list` to see recipes.
#
# Windows note: Docker Desktop on Windows has NO /dev/video0, so the container
# CANNOT capture a camera — the server exits at pipeline start. The image still
# builds and the binary + GStreamer plugins still load; `just test` verifies
# exactly that (GStreamer inits, then the expected v4l2 "no camera" failure).
# For an actual live stream, run the native Windows binary (`cargo run`) or
# deploy the image on a Linux host with a camera.

set windows-shell := ["powershell.exe", "-NoLogo", "-Command"]

image := "webrtc-camera-server"
container := "wcs"
port := "8090"

# Default: list recipes.
default:
    @just --list

# Build the Docker image.
build:
    docker build -t {{image}} .

# Run in foreground (Ctrl-C to stop). Windows: exits on v4l2 error (no camera).
run:
    docker run --rm --name {{container}} -p {{port}}:{{port}} {{image}}

# Run detached (background).
up:
    docker run -d --rm --name {{container}} -p {{port}}:{{port}} {{image}}

# Run with the synthetic videotestsrc source (no camera needed). Boots fully on
# Windows Docker — open http://localhost:8090 and Start to see the test pattern.
run-test:
    docker run --rm --name {{container}} -p {{port}}:{{port}} -e WCS_TEST_SOURCE=1 {{image}}

# Tail container logs.
logs:
    docker logs -f {{container}}

# Stop the running container.
stop:
    -docker stop {{container}}

# Remove the container (if it lingers) — `run`/`up` use --rm so usually a no-op.
rm:
    -docker rm -f {{container}}

# Remove the image (and any leftover container first).
clean: rm
    -docker rmi {{image}}

# Smoke test: build + boot the container, confirm GStreamer + plugins load.
# (Windows has no camera, so a v4l2 pipeline failure still counts as PASS.)
test: build
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/smoke-test.ps1 -Image {{image}}
