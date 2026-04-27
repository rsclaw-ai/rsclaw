#
# build-ui.ps1 — Build RsClaw desktop app (Tauri) on Windows
#
# Usage:
#   .\scripts\build-ui.ps1              # release build (default)
#   .\scripts\build-ui.ps1 -Debug       # debug build (opt-in, shows console)
#

param(
    [switch]$Debug,
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

# Default = release. Use -Debug to build a debug bundle (e.g. for stack
# traces or attaching a debugger). Release builds skip --debug-assertions
# so the windows_subsystem cfg_attr in main.rs hides the console window;
# debug builds intentionally keep it for stdout/stderr visibility.
if ($Debug) {
    $Profile = "debug"
    $CargoFlags = ""
    $TauriFlags = "--debug"
} else {
    $Profile = "release"
    $CargoFlags = "--release"
    $TauriFlags = ""
}

# Step 1: Build rsclaw CLI
Log "Building rsclaw CLI ($Profile)..."
# Read version from root Cargo.toml [package] block, not from git tags —
# `git describe` returns the commit hash (e.g. "1bbd5cd") when the working
# tree has no recent tag, which then ends up baked into the build banner.
function Get-CargoVersion {
    $cargoToml = Join-Path $RootDir "Cargo.toml"
    if (-not (Test-Path $cargoToml)) { return "dev" }
    $content = Get-Content -Raw $cargoToml
    if ($content -match '(?s)\[package\].*?(?:^|\n)\s*version\s*=\s*"([^"]+)"') {
        return $matches[1]
    }
    return "dev"
}
$Version = Get-CargoVersion
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
