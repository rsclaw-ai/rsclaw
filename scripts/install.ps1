# rsclaw installer for Windows
# Usage:
#   irm https://raw.githubusercontent.com/rsclaw-ai/rsclaw/main/scripts/install.ps1 | iex
#   .\install.ps1 -Version v0.1.0 -Prefix C:\tools\rsclaw
#
# China mirror:
#   $env:GITHUB_PROXY="https://gitfast.run"; irm https://gitfast.run/https://raw.githubusercontent.com/rsclaw-ai/rsclaw/main/scripts/install.ps1 | iex

param(
    [string]$Version = "",
    [string]$Prefix = "",
    [switch]$Help
)

# --- Ensure TLS 1.2 (required for GitHub API, older PowerShell defaults to TLS 1.0) ---
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 -bor [Net.SecurityProtocolType]::Tls13
} catch {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
}

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"  # Speed up Invoke-WebRequest

$Repo = "rsclaw-ai/rsclaw"
$Binary = "rsclaw.exe"

# Default install prefix
if (-not $Prefix) {
    $Prefix = Join-Path $env:LOCALAPPDATA "rsclaw\bin"
}

# GitHub proxy for regions where github.com is blocked (e.g. China).
$GhProxy = if ($env:GITHUB_PROXY) { $env:GITHUB_PROXY } else { "" }
$GhUrl = if ($GhProxy) { "$GhProxy/https://github.com" } else { "https://github.com" }
$GhApi = if ($GhProxy) { "$GhProxy/https://api.github.com" } else { "https://api.github.com" }

if ($Help) {
    Write-Host "Usage: install.ps1 [-Version VERSION] [-Prefix DIR]"
    Write-Host "  -Version   Install specific version (e.g. v0.1.0). Default: latest"
    Write-Host "  -Prefix    Installation directory. Default: %LOCALAPPDATA%\rsclaw\bin"
    exit 0
}

# --- Detect platform ---
function Get-Target {
    # PROCESSOR_ARCHITEW6432 is set when 32-bit process runs on 64-bit OS
    $arch = $env:PROCESSOR_ARCHITEW6432
    if (-not $arch) {
        $arch = $env:PROCESSOR_ARCHITECTURE
    }
    if (-not $arch) {
        # Fallback: WMI query
        try {
            $arch = (Get-WmiObject Win32_OperatingSystem).OSArchitecture
        } catch {
            $arch = ""
        }
    }

    switch -Wildcard ($arch) {
        "AMD64"    { return "x86_64-pc-windows-msvc" }
        "x86_64"   { return "x86_64-pc-windows-msvc" }
        "64*"      { return "x86_64-pc-windows-msvc" }  # "64-bit" from WMI
        "ARM64"    { return "aarch64-pc-windows-msvc" }
        "x86"      { return "x86_64-pc-windows-msvc" }  # 32-bit PS on 64-bit OS
        "EM64T"    { return "x86_64-pc-windows-msvc" }  # Old Intel name
        default {
            Write-Host "Error: unsupported architecture: $arch" -ForegroundColor Red
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
        $response = Invoke-WebRequest -Uri "$GhApi/repos/$Repo/releases/latest" -UseBasicParsing
        $json = $response.Content | ConvertFrom-Json
        return $json.tag_name
    }
    catch {
        # Fallback: try Invoke-RestMethod
        try {
            $release = Invoke-RestMethod -Uri "$GhApi/repos/$Repo/releases/latest"
            return $release.tag_name
        }
        catch {
            Write-Host "Error: failed to resolve latest version: $_" -ForegroundColor Red
            exit 1
        }
    }
}

# --- Verify checksum ---
function Test-Checksum {
    param([string]$File, [string]$Expected)

    $actual = (Get-FileHash -Path $File -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $Expected.ToLower()) {
        Write-Host "Error: checksum mismatch!" -ForegroundColor Red
        Write-Host "  Expected: $Expected"
        Write-Host "  Actual:   $actual"
        exit 1
    }
}

# --- Add to PATH ---
function Add-ToPath {
    param([string]$Dir)

    $userPath = [Environment]::GetEnvironmentVariable("PATH", "User")
    if (-not $userPath) { $userPath = "" }
    if ($userPath -notlike "*$Dir*") {
        [Environment]::SetEnvironmentVariable("PATH", "$Dir;$userPath", "User")
        $env:PATH = "$Dir;$env:PATH"
        Write-Host "Added $Dir to user PATH"
    }
}

# --- Extract archive (compatible with older PowerShell) ---
function Expand-Zip {
    param([string]$ZipPath, [string]$DestPath)

    if (Get-Command Expand-Archive -ErrorAction SilentlyContinue) {
        Expand-Archive -Path $ZipPath -DestinationPath $DestPath -Force
    } else {
        # Fallback for PowerShell < 5.0
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        [System.IO.Compression.ZipFile]::ExtractToDirectory($ZipPath, $DestPath)
    }
}

# --- Main ---
function Main {
    $target = Get-Target
    Write-Host "Detected platform: $target"

    $ver = Get-LatestVersion
    Write-Host "Installing rsclaw $ver ..."

    $archiveName = "rsclaw-$ver-$target.zip"
    $downloadUrl = "$GhUrl/$Repo/releases/download/$ver/$archiveName"
    $checksumsUrl = "$GhUrl/$Repo/releases/download/$ver/SHA256SUMS.txt"

    $tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) "rsclaw-install-$(Get-Random)"
    New-Item -ItemType Directory -Path $tmpDir -Force | Out-Null

    try {
        Write-Host "Downloading $archiveName ..."
        try {
            Invoke-WebRequest -Uri $downloadUrl -OutFile (Join-Path $tmpDir $archiveName) -UseBasicParsing
        }
        catch {
            Write-Host "Error: download failed." -ForegroundColor Red
            Write-Host "  URL: $downloadUrl"
            Write-Host "  Error: $_"
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
        Expand-Zip -ZipPath (Join-Path $tmpDir $archiveName) -DestPath $tmpDir

        # Create prefix directory
        if (-not (Test-Path $Prefix)) {
            New-Item -ItemType Directory -Path $Prefix -Force | Out-Null
        }

        Write-Host "Installing to $Prefix\$Binary ..."
        Copy-Item -Path (Join-Path $tmpDir $Binary) -Destination (Join-Path $Prefix $Binary) -Force

        # Add to PATH
        Add-ToPath -Dir $Prefix

        Write-Host ""
        Write-Host "rsclaw $ver installed successfully!" -ForegroundColor Green
        Write-Host "  Location: $Prefix\$Binary"

        $exe = Join-Path $Prefix $Binary
        if (Test-Path $exe) {
            try {
                $versionOutput = & $exe --version 2>&1
                Write-Host "  Version:  $versionOutput"
            }
            catch {
                Write-Host "  Run 'rsclaw --version' to verify"
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
