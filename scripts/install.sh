#!/usr/bin/env bash
# rsclaw installer for macOS and Linux
# Usage:
#   curl -fsSL https://app.rsclaw.ai/scripts/install.sh | bash
#   curl -fsSL ... | bash -s -- --version v0.1.0 --prefix /opt/rsclaw

set -euo pipefail

REPO="rsclaw-ai/rsclaw"
BINARY="rsclaw"
DEFAULT_PREFIX="/usr/local/bin"

# GitHub proxy for regions where github.com is blocked (e.g. China).
# Usage: GITHUB_PROXY=https://ghfast.top curl -fsSL ... | bash
# Note: most proxies only support file downloads, not API requests,
# so we always call api.github.com directly.
GITHUB_PROXY="${GITHUB_PROXY:-}"
if [[ -n "$GITHUB_PROXY" ]]; then
    GITHUB_URL="${GITHUB_PROXY}/https://github.com"
else
    GITHUB_URL="https://github.com"
fi
GITHUB_API="https://api.github.com"

# --- Defaults ---
VERSION=""
PREFIX="$DEFAULT_PREFIX"

# --- Parse args ---
while [[ $# -gt 0 ]]; do
    case "$1" in
        --version|-v)  VERSION="$2";  shift 2 ;;
        --prefix|-p)   PREFIX="$2";   shift 2 ;;
        --help|-h)
            echo "Usage: install.sh [--version VERSION] [--prefix DIR]"
            echo "  --version, -v   Install specific version (e.g. v0.1.0). Default: latest"
            echo "  --prefix,  -p   Installation directory. Default: $DEFAULT_PREFIX"
            exit 0
            ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

# --- Detect platform ---
detect_target() {
    local os arch target

    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Linux)
            case "$arch" in
                x86_64)  target="x86_64-unknown-linux-gnu" ;;
                aarch64) target="aarch64-unknown-linux-gnu" ;;
                arm64)   target="aarch64-unknown-linux-gnu" ;;
                *) echo "Error: unsupported architecture: $arch"; exit 1 ;;
            esac
            ;;
        Darwin)
            case "$arch" in
                x86_64)  target="x86_64-apple-darwin" ;;
                arm64)   target="aarch64-apple-darwin" ;;
                aarch64) target="aarch64-apple-darwin" ;;
                *) echo "Error: unsupported architecture: $arch"; exit 1 ;;
            esac
            ;;
        *)
            echo "Error: unsupported OS: $os"
            echo "For Windows, use scripts/install.ps1"
            exit 1
            ;;
    esac

    echo "$target"
}

# --- Resolve version ---
# Global: release JSON cached for version + download URL resolution
RELEASE_JSON=""

# Fetch release data from API (call once in main, not in subshell)
fetch_release_data() {
    # Primary: app.rsclaw.ai/api/version
    RELEASE_JSON="$(curl -fsSL --max-time 5 "https://app.rsclaw.ai/api/version" 2>/dev/null)" || true

    # Fallback: GitHub releases API
    if [[ -z "$RELEASE_JSON" ]]; then
        RELEASE_JSON="$(curl -fsSL --max-time 10 "${GITHUB_API}/repos/${REPO}/releases?per_page=10" 2>/dev/null)" || true
    fi
}

resolve_version() {
    if [[ -n "$VERSION" ]]; then
        echo "$VERSION"
        return
    fi

    local latest=""
    if [[ -n "$RELEASE_JSON" ]]; then
        latest="$(echo "$RELEASE_JSON" | grep -o '"tag_name" *: *"[^"]*"' | grep -o '"v[^"]*"' | tr -d '"' | grep -v '^app-' | sort -rV | head -1)"
    fi

    if [[ -z "$latest" ]]; then
        echo "Error: failed to resolve latest version" >&2
        exit 1
    fi
    echo "$latest"
}

# Extract browser_download_url for a given filename from cached release JSON
resolve_download_url() {
    local filename="$1"
    if [[ -n "$RELEASE_JSON" ]]; then
        local url
        url="$(echo "$RELEASE_JSON" | grep -o "\"browser_download_url\" *: *\"[^\"]*${filename}\"" | head -1 | sed 's/.*"\(http[^"]*\)".*/\1/')"
        if [[ -n "$url" ]]; then
            echo "$url"
            return
        fi
    fi
    # Fallback: construct URL from GITHUB_URL
    echo "${GITHUB_URL}/${REPO}/releases/download/${VERSION:-latest}/${filename}"
}

# --- Verify checksum ---
verify_checksum() {
    local file="$1" expected_hash="$2"

    local actual_hash
    if command -v sha256sum &>/dev/null; then
        actual_hash="$(sha256sum "$file" | awk '{print $1}')"
    elif command -v shasum &>/dev/null; then
        actual_hash="$(shasum -a 256 "$file" | awk '{print $1}')"
    else
        echo "Warning: no sha256sum or shasum found, skipping checksum verification"
        return 0
    fi

    if [[ "$actual_hash" != "$expected_hash" ]]; then
        echo "Error: checksum mismatch!"
        echo "  Expected: $expected_hash"
        echo "  Actual:   $actual_hash"
        return 1
    fi
}

# --- Main ---
CLEANUP_DIR=""
cleanup() { [[ -n "$CLEANUP_DIR" ]] && rm -rf "$CLEANUP_DIR"; }
trap cleanup EXIT

main() {
    local target version archive_name download_url checksums_url

    target="$(detect_target)"
    echo "Detected platform: $target"

    fetch_release_data
    version="$(resolve_version)"
    echo "Installing rsclaw $version ..."

    # Build candidate list: primary target + fallback variants
    local targets=("$target")
    case "$target" in
        *-linux-gnu)  targets+=("${target/linux-gnu/linux-musl}") ;;
        *-linux-musl) targets+=("${target/linux-musl/linux-gnu}") ;;
    esac

    local tmpdir
    tmpdir="$(mktemp -d)"
    CLEANUP_DIR="$tmpdir"

    local downloaded=false
    for try_target in "${targets[@]}"; do
        archive_name="rsclaw-${version}-${try_target}.tar.gz"
        download_url="$(resolve_download_url "$archive_name")"
        checksums_url="$(resolve_download_url "SHA256SUMS.txt")"

        echo "Trying ${archive_name} ..."
        if curl -fSL --progress-bar -o "${tmpdir}/${archive_name}" "$download_url" 2>/dev/null; then
            target="$try_target"
            downloaded=true
            break
        fi
    done

    if [[ "$downloaded" != "true" ]]; then
        echo "Error: no pre-built binary found for ${target}."
        echo "  Tried: ${targets[*]}"
        exit 1
    fi

    echo "Downloading checksums ..."
    if curl -fsSL -o "${tmpdir}/SHA256SUMS.txt" "$checksums_url"; then
        expected="$(grep "$archive_name" "${tmpdir}/SHA256SUMS.txt" | awk '{print $1}')"
        if [[ -n "$expected" ]]; then
            echo "Verifying checksum ..."
            verify_checksum "${tmpdir}/${archive_name}" "$expected"
            echo "Checksum OK"
        fi
    else
        echo "Warning: checksums not available, skipping verification"
    fi

    echo "Extracting ..."
    tar xzf "${tmpdir}/${archive_name}" -C "${tmpdir}"

    echo "Installing to ${PREFIX}/${BINARY} ..."
    if [[ -w "$PREFIX" ]]; then
        install -m 755 "${tmpdir}/${BINARY}" "${PREFIX}/${BINARY}"
    else
        echo "Need elevated permissions to install to ${PREFIX}"
        sudo install -m 755 "${tmpdir}/${BINARY}" "${PREFIX}/${BINARY}"
    fi

    echo ""
    echo "rsclaw $version installed successfully!"
    echo "  Location: ${PREFIX}/${BINARY}"

    if command -v "$BINARY" &>/dev/null; then
        echo "  Version:  $("$BINARY" --version 2>/dev/null || echo 'run `rsclaw --version` to verify')"
    else
        echo ""
        echo "Note: ${PREFIX} is not in your PATH."
        echo "Add it with: export PATH=\"${PREFIX}:\$PATH\""
    fi
}

main
