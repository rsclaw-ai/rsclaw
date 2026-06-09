#
# build-ui.ps1 — Build RsClaw desktop app (Tauri) on Windows
#
# Usage:
#   .\scripts\build-ui.ps1                       # release, host arch (default)
#   .\scripts\build-ui.ps1 -Debug                # debug build (shows console)
#   .\scripts\build-ui.ps1 -DualArch             # x64 + arm64 (both Windows MSVC)
#   .\scripts\build-ui.ps1 -Targets x86_64-pc-windows-msvc,aarch64-pc-windows-msvc
#
# -DualArch / -Targets build each architecture in turn (own sidecar + own bundle),
# so an ARM Windows box can ship the x64 build too (and vice-versa).
#

param(
    [switch]$Debug,
    [string]$Target = "",         # single explicit target (back-compat)
    [string[]]$Targets = @(),     # multiple targets (overrides -Target)
    [switch]$DualArch             # shortcut for x64 + arm64 Windows MSVC
)

$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RootDir = Split-Path -Parent $ScriptDir
$UiDir = Join-Path $RootDir "ui"
$TauriDir = Join-Path $UiDir "src-tauri"
$BinDir = Join-Path $TauriDir "binaries"

function Log($msg) { Write-Host "[build-ui] $msg" -ForegroundColor Green }
function Warn($msg) { Write-Host "[build-ui] $msg" -ForegroundColor Red }

# Resolve the list of targets to build.
if ($DualArch) {
    $BuildTargets = @("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc")
} elseif ($Targets.Count -gt 0) {
    $BuildTargets = $Targets
} elseif ($Target) {
    $BuildTargets = @($Target)
} else {
    # Auto-detect host target (original default behaviour).
    $hostTarget = (rustc -vV | Select-String "host:").ToString().Replace("host: ", "").Trim()
    $BuildTargets = @($hostTarget)
}
Log "Targets: $($BuildTargets -join ', ')"

# Default = release. -Debug keeps the console window for stdout/stderr.
if ($Debug) {
    $Profile = "debug"; $CargoFlags = @(); $TauriFlags = @("--debug")
} else {
    $Profile = "release"; $CargoFlags = @("--release"); $TauriFlags = @()
}

# Version baked into the build banner — read from Cargo.toml, not git tags.
function Get-CargoVersion {
    $cargoToml = Join-Path $RootDir "Cargo.toml"
    if (-not (Test-Path $cargoToml)) { return "dev" }
    $content = Get-Content -Raw $cargoToml
    if ($content -match '(?s)\[package\].*?(?:^|\n)\s*version\s*=\s*"([^"]+)"') { return $matches[1] }
    return "dev"
}
$env:RSCLAW_BUILD_VERSION = "v$(Get-CargoVersion)"
$env:RSCLAW_BUILD_DATE = Get-Date -Format "yyyy-MM-dd"

# Frontend deps once (shared across targets).
if (-not (Test-Path (Join-Path $UiDir "node_modules"))) {
    Log "Installing frontend dependencies..."
    Push-Location $UiDir
    try { yarn install } finally { Pop-Location }
}

New-Item -ItemType Directory -Force -Path $BinDir | Out-Null

function Build-Target($t) {
    Log "==== Building $t ($Profile) ===="

    # Ensure the Rust target is installed (no-op if already present).
    rustup target add $t 2>$null

    # Step 1: rsclaw CLI for this target.
    Push-Location $RootDir
    try {
        cargo build @CargoFlags --target $t
    } finally { Pop-Location }

    $CargoOut = Join-Path $RootDir "target\$t\$Profile\rsclaw.exe"
    if (-not (Test-Path $CargoOut)) {
        Warn "rsclaw.exe not found at: $CargoOut"
        exit 1
    }
    $Size = (Get-Item $CargoOut).Length / 1MB
    Log "CLI binary: $([math]::Round($Size, 1))MB ($CargoOut)"

    # Step 2: copy as the Tauri sidecar (name carries the target triple,
    # which is the externalBin convention Tauri expects).
    Copy-Item $CargoOut (Join-Path $BinDir "rsclaw-$t.exe") -Force
    Log "Sidecar: $BinDir\rsclaw-$t.exe"

    # Step 3: Tauri bundle for this target.
    Log "Building Tauri app ($t)..."
    Push-Location $UiDir
    try {
        npx tauri build --target $t @TauriFlags
    } finally { Pop-Location }

    # Show this target's output.
    $BundleDir = Join-Path $TauriDir "target\$t\$Profile\bundle"
    if (Test-Path $BundleDir) {
        Log "Output ($t):"
        Get-ChildItem $BundleDir -Recurse -Include "*.msi", "*.exe" | ForEach-Object {
            $s = [math]::Round($_.Length / 1MB, 1)
            Write-Host "  ${s}MB  $($_.FullName)"
        }
    }
}

foreach ($t in $BuildTargets) { Build-Target $t }

Log "Build complete! ($($BuildTargets.Count) target(s))"
