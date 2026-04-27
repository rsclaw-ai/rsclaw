# Local build script for rsclaw on Windows
# Builds release binaries with optional cross-compilation.
#
# Usage:
#   .\scripts\build.ps1                           # build for current platform
#   .\scripts\build.ps1 -Targets all              # build all Windows targets
#   .\scripts\build.ps1 -Targets x86_64,arm64     # build specific targets
#   .\scripts\build.ps1 -Clean                    # remove dist/ directory
#
# Prerequisites:
#   - Rust toolchain 1.91+
#   - Visual Studio Build Tools (MSVC)
#   - For Linux/macOS targets: install `cross`
#     cargo install cross --git https://github.com/cross-rs/cross

param(
    [string[]]$Targets = @(),
    [switch]$All,
    [switch]$Clean,
    [switch]$Help
)

$ErrorActionPreference = "Stop"

$Binary = "rsclaw"
$DistDir = "dist"

$WindowsTargets = @(
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc"
)
$LinuxTargets = @(
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu"
)
$MacosTargets = @(
    "x86_64-apple-darwin",
    "aarch64-apple-darwin"
)
$AllTargets = $WindowsTargets + $LinuxTargets + $MacosTargets

# Read version from Cargo.toml [package] section. Matches the first
# `version = "..."` that appears under `[package]` so it's not confused
# by versions of dependencies declared elsewhere in the file.
function Get-CargoVersion {
    if (-not (Test-Path "Cargo.toml")) { return "dev" }
    $content = Get-Content -Raw "Cargo.toml"
    if ($content -match '(?s)\[package\].*?(?:^|\n)\s*version\s*=\s*"([^"]+)"') {
        return $matches[1]
    }
    return "dev"
}
# Match release-cli.yml + build-ui.ps1: prefix the cargo version with "v"
# so the binary's --version output, the artifact filename, and what the
# desktop bundle's sidecar reports all align.
$Version = "v$(Get-CargoVersion)"
$env:RSCLAW_BUILD_VERSION = $Version
$env:RSCLAW_BUILD_DATE = (Get-Date -Format "yyyy-MM-dd")

# Detect host architecture
function Get-HostTarget {
    $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
    switch ($arch) {
        "X64"   { return "x86_64-pc-windows-msvc" }
        "Arm64" { return "aarch64-pc-windows-msvc" }
        default { return "unknown" }
    }
}

$HostTarget = Get-HostTarget

function Write-Log   { param($Msg) Write-Host "[build] $Msg" -ForegroundColor Cyan }
function Write-Ok    { param($Msg) Write-Host "[  ok ] $Msg" -ForegroundColor Green }
function Write-Warn  { param($Msg) Write-Host "[ warn] $Msg" -ForegroundColor Yellow }
function Write-Err   { param($Msg) Write-Host "[ fail] $Msg" -ForegroundColor Red }

function Test-Cross {
    return (Get-Command "cross" -ErrorAction SilentlyContinue) -ne $null
}

function Install-RustTarget {
    param([string]$Target)
    $installed = rustup target list --installed
    if ($installed -notcontains $Target) {
        Write-Log "Installing rustup target: $Target"
        rustup target add $Target
    }
}

function Build-Target {
    param([string]$Target)

    Write-Log "Building $Target ..."
    Install-RustTarget $Target

    $isNative = ($Target -eq $HostTarget) -or ($Target -match "pc-windows-msvc")
    $buildCmd = "cargo"

    if (-not $isNative) {
        if (Test-Cross) {
            $buildCmd = "cross"
        } else {
            Write-Err "Cannot cross-compile $Target without 'cross'. Install: cargo install cross"
            return $false
        }
    }

    & $buildCmd build --release --target $Target
    if ($LASTEXITCODE -ne 0) {
        Write-Err "Build failed: $Target"
        return $false
    }

    Write-Ok "Built: $Target"
    return (Package-Target $Target)
}

function Package-Target {
    param([string]$Target)

    if (-not (Test-Path $DistDir)) {
        New-Item -ItemType Directory -Path $DistDir -Force | Out-Null
    }

    $ext = if ($Target -match "windows") { ".exe" } else { "" }
    $binPath = "target/$Target/release/$Binary$ext"

    if (-not (Test-Path $binPath)) {
        Write-Err "Binary not found: $binPath"
        return $false
    }

    if ($Target -match "windows") {
        $archiveName = "$Binary-$Version-$Target.zip"
        Compress-Archive -Path $binPath -DestinationPath "$DistDir/$archiveName" -Force
    } else {
        $archiveName = "$Binary-$Version-$Target.tar.gz"
        tar czf "$DistDir/$archiveName" -C "target/$Target/release" "$Binary$ext"
    }

    Write-Ok "Packaged: $DistDir/$archiveName"
    return $true
}

function New-Checksums {
    Write-Log "Generating checksums ..."
    $files = Get-ChildItem "$DistDir/$Binary-*" -ErrorAction SilentlyContinue
    if ($files.Count -eq 0) {
        Write-Warn "No artifacts to checksum"
        return
    }

    $checksums = foreach ($f in $files) {
        $hash = (Get-FileHash -Path $f.FullName -Algorithm SHA256).Hash.ToLower()
        "$hash  $($f.Name)"
    }
    $checksums | Set-Content "$DistDir/SHA256SUMS.txt" -Encoding utf8
    Write-Ok "Checksums: $DistDir/SHA256SUMS.txt"
}

# --- Main ---

if ($Help) {
    Write-Host "Usage: build.ps1 [-Targets <target,...>] [-All] [-Clean]"
    Write-Host ""
    Write-Host "Options:"
    Write-Host "  -Targets   Comma-separated targets or groups: all, windows, linux, macos"
    Write-Host "  -All       Build all 6 targets"
    Write-Host "  -Clean     Remove dist/ directory"
    Write-Host ""
    Write-Host "Windows targets: $($WindowsTargets -join ', ')"
    Write-Host "Linux targets:   $($LinuxTargets -join ', ')"
    Write-Host "macOS targets:   $($MacosTargets -join ', ')"
    Write-Host ""
    Write-Host "Host: $HostTarget"
    Write-Host "cross: $(if (Test-Cross) {'available'} else {'not installed'})"
    exit 0
}

if ($Clean) {
    Write-Log "Cleaning dist/ ..."
    Remove-Item -Path $DistDir -Recurse -Force -ErrorAction SilentlyContinue
    Write-Ok "Cleaned"
    exit 0
}

# Resolve targets
$resolvedTargets = @()

if ($All) {
    $resolvedTargets = $AllTargets
} elseif ($Targets.Count -eq 0) {
    $resolvedTargets = @($HostTarget)
} else {
    foreach ($t in $Targets) {
        switch ($t) {
            "all"     { $resolvedTargets += $AllTargets }
            "windows" { $resolvedTargets += $WindowsTargets }
            "linux"   { $resolvedTargets += $LinuxTargets }
            "macos"   { $resolvedTargets += $MacosTargets }
            "x86_64"  { $resolvedTargets += "x86_64-pc-windows-msvc" }
            "arm64"   { $resolvedTargets += "aarch64-pc-windows-msvc" }
            default   { $resolvedTargets += $t }
        }
    }
}

$resolvedTargets = $resolvedTargets | Select-Object -Unique

Write-Log "rsclaw $Version -- building $($resolvedTargets.Count) target(s)"
Write-Log "Host: $HostTarget"
Write-Log "cross: $(if (Test-Cross) {'available'} else {'not installed'})"
Write-Host ""

$failed = 0
foreach ($target in $resolvedTargets) {
    $result = Build-Target $target
    if (-not $result) { $failed++ }
    Write-Host ""
}

New-Checksums

Write-Host ""
Write-Log "Artifacts in ${DistDir}/:"
Get-ChildItem $DistDir | Format-Table Name, @{N="Size";E={"{0:N2} MB" -f ($_.Length / 1MB)}} -AutoSize
Write-Host ""

if ($failed -gt 0) {
    Write-Err "$failed target(s) failed"
    exit 1
}

Write-Ok "All $($resolvedTargets.Count) target(s) built successfully"
