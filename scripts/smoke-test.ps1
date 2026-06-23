# Smoke test for the Docker image. Runs the container with the synthetic
# videotestsrc source (WCS_TEST_SOURCE=1) so it boots fully WITHOUT a camera —
# works on any host, including Docker Desktop on Windows. Then probes /healthz.
# Exits non-zero if the container dies or the endpoint doesn't answer.
param(
    [string]$Image = "webrtc-camera-server",
    [string]$Name = "wcs-smoke",
    [int]$Port = 18090
)

# Don't let a native command's stderr abort the script.
$ErrorActionPreference = "Continue"

# Best-effort pre-clean of a leftover container from a prior run.
try { docker rm -f $Name *> $null } catch {}

Write-Host "Starting container '$Name' (test source) on port $Port..."
docker run -d --name $Name -p "${Port}:8090" -e WCS_TEST_SOURCE=1 $Image | Out-Null
Start-Sleep -Seconds 8

$running = [bool](docker ps -q -f "name=$Name")
$health = ""
try {
    $health = (Invoke-WebRequest "http://localhost:$Port/healthz" -UseBasicParsing -TimeoutSec 5).Content
} catch {
    $health = "<no response>"
}

$ready = ""
try {
    $ready = (Invoke-WebRequest "http://localhost:$Port/readyz" -UseBasicParsing -TimeoutSec 5).StatusCode
} catch {
    $ready = "<no response>"
}

Write-Host "--- container log (stdout) ---"
$errFile = New-TemporaryFile
Write-Host ((docker logs $Name 2>$errFile.FullName | Out-String).Trim())
Remove-Item $errFile.FullName -ErrorAction SilentlyContinue
Write-Host "------------------------------"
Write-Host "running=$running  /healthz=$health  /readyz=$ready"

try { docker rm -f $Name *> $null } catch {}

if ($running -and $health -eq "ok") {
    Write-Host "PASS: container booted with the test source and /healthz responded (HTTP/WS server is up)."
    exit 0
} else {
    Write-Host "FAIL: container is not serving (running=$running, health=$health). Image or server is broken."
    exit 1
}
