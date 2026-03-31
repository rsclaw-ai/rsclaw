# rsclaw installer for Windows
# Usage:
#   irm https://raw.githubusercontent.com/rsclaw-ai/rsclaw/main/scripts/install.ps1 | iex
#   .\install.ps1 -Version v0.1.0 -Prefix C:\tools\rsclaw

param(
    [string]$Version = "",
    [string]$Prefix = "$env:LOCALAPPDATA\rsclaw\bin",
    [switch]$Help
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"  # Speed up Invoke-WebRequest

$Repo = "rsclaw-ai/rsclaw"
$Binary = "rsclaw.exe"

if ($Help) {
    Write-Host "Usage: install.ps1 [-Version VERSION] [-Prefix DIR]"
    Write-Host "  -Version   Install specific version (e.g. v0.1.0). Default: latest"
    Write-Host "  -Prefix    Installation directory. Default: $env:LOCALAPPDATA\rsclaw\bin"
    exit 0
}

# --- Detect platform ---
function Get-Target {
    $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
    switch ($arch) {
        "X64"   { return "x86_64-pc-windows-msvc" }
        "Arm64" { return "aarch64-pc-windows-msvc" }
        default {
            Write-Error "Unsupported architecture: $arch"
            exit 1
        }
    }
}

# --- Resolve version ---
function Get-LatestVersion {
    if ($Version -ne "") {
        return $Version
    }
    try {
        $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest"
        return $release.tag_name
    }
    catch {
        Write-Error "Failed to resolve latest version: $_"
        exit 1
    }
}

# --- Verify checksum ---
function Test-Checksum {
    param([string]$File, [string]$Expected)

    $actual = (Get-FileHash -Path $File -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $Expected.ToLower()) {
        Write-Error "Checksum mismatch!`n  Expected: $Expected`n  Actual:   $actual"
        exit 1
    }
}

# --- Add to PATH ---
function Add-ToPath {
    param([string]$Dir)

    $userPath = [Environment]::GetEnvironmentVariable("PATH", "User")
    if ($userPath -notlike "*$Dir*") {
        [Environment]::SetEnvironmentVariable("PATH", "$Dir;$userPath", "User")
        $env:PATH = "$Dir;$env:PATH"
        Write-Host "Added $Dir to user PATH"
    }
}

# --- Main ---
function Main {
    $target = Get-Target
    Write-Host "Detected platform: $target"

    $ver = Get-LatestVersion
    Write-Host "Installing rsclaw $ver ..."

    $archiveName = "rsclaw-$ver-$target.zip"
    $downloadUrl = "https://github.com/$Repo/releases/download/$ver/$archiveName"
    $checksumsUrl = "https://github.com/$Repo/releases/download/$ver/SHA256SUMS.txt"

    $tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) "rsclaw-install-$(Get-Random)"
    New-Item -ItemType Directory -Path $tmpDir -Force | Out-Null

    try {
        Write-Host "Downloading $archiveName ..."
        try {
            Invoke-WebRequest -Uri $downloadUrl -OutFile (Join-Path $tmpDir $archiveName) -UseBasicParsing
        }
        catch {
            Write-Error "Download failed. Check version and platform.`n  URL: $downloadUrl`n  Error: $_"
            exit 1
        }

        # Verify checksum
        try {
            $checksums = Invoke-WebRequest -Uri $checksumsUrl -UseBasicParsing
            $lines = $checksums.Content -split "`n"
            foreach ($line in $lines) {
                if ($line -like "*$archiveName*") {
                    $expected = ($line -split "\s+")[0]
                    Write-Host "Verifying checksum ..."
                    Test-Checksum -File (Join-Path $tmpDir $archiveName) -Expected $expected
                    Write-Host "Checksum OK"
                    break
                }
            }
        }
        catch {
            Write-Host "Warning: checksums not available, skipping verification"
        }

        Write-Host "Extracting ..."
        Expand-Archive -Path (Join-Path $tmpDir $archiveName) -DestinationPath $tmpDir -Force

        # Create prefix directory
        if (-not (Test-Path $Prefix)) {
            New-Item -ItemType Directory -Path $Prefix -Force | Out-Null
        }

        Write-Host "Installing to $Prefix\$Binary ..."
        Copy-Item -Path (Join-Path $tmpDir $Binary) -Destination (Join-Path $Prefix $Binary) -Force

        # Add to PATH
        Add-ToPath -Dir $Prefix

        Write-Host ""
        Write-Host "rsclaw $ver installed successfully!"
        Write-Host "  Location: $Prefix\$Binary"

        $exe = Join-Path $Prefix $Binary
        if (Test-Path $exe) {
            try {
                $versionOutput = & $exe --version 2>&1
                Write-Host "  Version:  $versionOutput"
            }
            catch {
                Write-Host "  Run ``rsclaw --version`` to verify"
            }
        }

        Write-Host ""
        Write-Host "Note: restart your terminal for PATH changes to take effect."
    }
    finally {
        Remove-Item -Path $tmpDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Main
