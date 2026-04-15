#
# build-ui.ps1 — Build RsClaw desktop app (Tauri) on Windows
#
# Usage:
#   .\scripts\build-ui.ps1              # debug build
#   .\scripts\build-ui.ps1 -Release     # release build
#

param(
    [switch]$Release,
    [string]$Target = ""
)

$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RootDir = Split-Path -Parent $ScriptDir
$UiDir = Join-Path $RootDir "ui"
$TauriDir = Join-Path $UiDir "src-tauri"
$BinDir = Join-Path $TauriDir "binaries"

function Log($msg) { Write-Host "[build-ui] $msg" -ForegroundColor Green }
function Warn($msg) { Write-Host "[build-ui] $msg" -ForegroundColor Red }

# Auto-detect target
if (-not $Target) {
    $Target = (rustc -vV | Select-String "host:").ToString().Replace("host: ", "").Trim()
}
Log "Target: $Target"

$SidecarName = "rsclaw-$Target.exe"

if ($Release) {
    $Profile = "release"
    $CargoFlags = "--release"
    $TauriFlags = ""
} else {
    $Profile = "debug"
    $CargoFlags = ""
    $TauriFlags = "--debug"
}

# Step 1: Build rsclaw CLI
Log "Building rsclaw CLI ($Profile)..."
$Version = git -C $RootDir describe --tags --always 2>$null
if (-not $Version) { $Version = "dev" }
$BuildDate = Get-Date -Format "yyyy-MM-dd"

$env:RSCLAW_BUILD_VERSION = "v$Version"
$env:RSCLAW_BUILD_DATE = $BuildDate

Push-Location $RootDir
try {
    cargo build $CargoFlags
} finally {
    Pop-Location
}

# Locate binary
$CargoOut = Join-Path $RootDir "target\$Profile\rsclaw.exe"
if (-not (Test-Path $CargoOut)) {
    Warn "rsclaw.exe not found at: $CargoOut"
    exit 1
}
$Size = (Get-Item $CargoOut).Length / 1MB
Log "CLI binary: $([math]::Round($Size, 1))MB ($CargoOut)"

# Step 2: Copy to Tauri binaries
New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
Copy-Item $CargoOut (Join-Path $BinDir $SidecarName) -Force
Log "Sidecar: $BinDir\$SidecarName"

# Step 3: Install frontend deps
if (-not (Test-Path (Join-Path $UiDir "node_modules"))) {
    Log "Installing frontend dependencies..."
    Push-Location $UiDir
    try { yarn install } finally { Pop-Location }
}

# Step 4: Build Tauri app
Log "Building Tauri app ($Profile)..."
Push-Location $UiDir
try {
    npx tauri build $TauriFlags
} finally {
    Pop-Location
}

Log "Build complete!"

# Show output
$BundleDir = Join-Path $TauriDir "target\$Profile\bundle"
if (Test-Path $BundleDir) {
    Log "Output:"
    Get-ChildItem $BundleDir -Recurse -Include "*.msi","*.exe" | ForEach-Object {
        $s = [math]::Round($_.Length / 1MB, 1)
        Write-Host "  ${s}MB  $($_.FullName)"
    }
}
