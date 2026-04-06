# rsclaw installer for Windows
# Usage:
#   irm https://app.rsclaw.ai/scripts/install.ps1 | iex
#   .\install.ps1 -Version v0.1.0 -Prefix C:\tools\rsclaw
#
# China mirror:
#   $env:GITHUB_PROXY="https://gitfast.run"; irm https://gitfast.run/https://app.rsclaw.ai/scripts/install.ps1 | iex

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
# Note: most proxies (ghfast.top, etc.) only support file downloads,
# not API requests, so we always call api.github.com directly.
$GhProxy = if ($env:GITHUB_PROXY) { $env:GITHUB_PROXY } else { "" }
$GhUrl = if ($GhProxy) { "$GhProxy/https://github.com" } else { "https://github.com" }
$GhApi = "https://api.github.com"

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

# --- Resolve version + cache release data ---
$script:ReleaseData = $null

function Get-LatestVersion {
    if ($Version -ne "") {
        return $Version
    }

    # Primary: app.rsclaw.ai/api/version (array of releases, find CLI tag v*)
    try {
        $script:ReleaseData = Invoke-RestMethod -Uri "https://app.rsclaw.ai/api/version" -TimeoutSec 5
        foreach ($r in $script:ReleaseData) {
            if ($r.tag_name -match '^v' -and $r.tag_name -notmatch '^app-') {
                return $r.tag_name
            }
        }
    } catch {}

    # Fallback: GitHub releases API
    try {
        $script:ReleaseData = Invoke-RestMethod -Uri "$GhApi/repos/$Repo/releases?per_page=10" -TimeoutSec 10
        foreach ($r in $script:ReleaseData) {
            if ($r.tag_name -match '^v' -and $r.tag_name -notmatch '^app-') {
                return $r.tag_name
            }
        }
    } catch {}

    Write-Host "Error: failed to resolve latest version" -ForegroundColor Red
    exit 1
}

# Extract browser_download_url for a given filename from cached release data
function Get-DownloadUrl {
    param([string]$FileName)
    if ($script:ReleaseData) {
        foreach ($r in $script:ReleaseData) {
            if ($r.assets) {
                foreach ($a in $r.assets) {
                    if ($a.name -like "*$FileName*") {
                        return $a.browser_download_url
                    }
                }
            }
        }
    }
    # Fallback
    return "$GhUrl/$Repo/releases/download/$Version/$FileName"
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
    $downloadUrl = Get-DownloadUrl $archiveName
    $checksumsUrl = Get-DownloadUrl "SHA256SUMS.txt"

    $tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) "rsclaw-install-$(Get-Random)"
    New-Item -ItemType Directory -Path $tmpDir -Force | Out-Null

    try {
        Write-Host "Downloading $archiveName ..."
        Write-Host "  URL: $downloadUrl"
        try {
            $ProgressPreference = 'SilentlyContinue'
            Invoke-WebRequest -Uri $downloadUrl -OutFile (Join-Path $tmpDir $archiveName) -UseBasicParsing -TimeoutSec 120
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

        # --- Install tray script ---
        $trayScript = "rsclaw-tray.ps1"
        $trayUrl = if ($GhProxy) {
            "$GhProxy/https://raw.githubusercontent.com/$Repo/main/scripts/$trayScript"
        } else {
            "https://raw.githubusercontent.com/$Repo/main/scripts/$trayScript"
        }
        try {
            Write-Host "Downloading tray controller ..."
            Invoke-WebRequest -Uri $trayUrl -OutFile (Join-Path $Prefix $trayScript) -UseBasicParsing
        } catch {
            Write-Host "Warning: tray script download failed, skipping" -ForegroundColor Yellow
        }

        # --- Create startup shortcut ---
        $trayPath = Join-Path $Prefix $trayScript
        if (Test-Path $trayPath) {
            try {
                $startupDir = [Environment]::GetFolderPath("Startup")
                $shortcutPath = Join-Path $startupDir "RsClaw Tray.lnk"
                $shell = New-Object -ComObject WScript.Shell
                $shortcut = $shell.CreateShortcut($shortcutPath)
                $shortcut.TargetPath = "powershell.exe"
                $shortcut.Arguments = "-WindowStyle Hidden -ExecutionPolicy Bypass -File `"$trayPath`""
                $shortcut.WorkingDirectory = $Prefix
                $shortcut.Description = "RsClaw Gateway Tray Controller"
                $shortcut.Save()
                Write-Host "Tray auto-start enabled (startup shortcut created)"
            } catch {
                Write-Host "Warning: could not create startup shortcut: $_" -ForegroundColor Yellow
            }
        }

        Write-Host ""
        Write-Host "rsclaw $ver installed successfully!" -ForegroundColor Green
        Write-Host "  Location: $Prefix\$Binary"
        Write-Host "  Tray:     $Prefix\$trayScript"

        Write-Host ""
        Write-Host "Note: restart your terminal for PATH changes to take effect."
    }
    finally {
        Remove-Item -Path $tmpDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Main
