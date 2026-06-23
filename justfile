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
test: build
    #!powershell.exe -NoLogo -Command
    Write-Host "Starting container..."
    docker run -d --rm --name {{container}}-test {{image}} | Out-Null
    Start-Sleep -Seconds 8
    $log = docker logs {{container}}-test 2>&1 | Out-String
    docker rm -f {{container}}-test 2>$null | Out-Null
    Write-Host "--- container log ---"
    Write-Host $log
    Write-Host "---------------------"
    if ($log -match "GStreamer initialized") {
        if ($log -match "camera is live") {
            Write-Host "PASS: server booted AND camera is live (Linux host with camera)."
        } elseif ($log -match "v4l2|/dev/video0|shared pipeline|Failed to build") {
            Write-Host "PASS: image OK. GStreamer loaded; pipeline failed on the missing camera (expected on Windows)."
        } else {
            Write-Host "PASS: GStreamer loaded. (No camera-related outcome detected.)"
        }
    } else {
        Write-Host "FAIL: GStreamer did not initialize — image or plugins broken."
        exit 1
    }
