#!/usr/bin/env bash
#
# build-ui.sh — Build RsClaw desktop app (Tauri)
#
# Usage:
#   ./scripts/build-ui.sh              # build for current platform (debug)
#   ./scripts/build-ui.sh --release    # build for current platform (release)
#   ./scripts/build-ui.sh --target x86_64-pc-windows-msvc --release
#
# Prerequisites:
#   - Rust toolchain (rustup)
#   - Node.js + yarn
#   - yarn install (in ui/)
#
# What this script does:
#   1. Build the rsclaw CLI binary for the target platform
#   2. Copy it into ui/src-tauri/binaries/ with the Tauri sidecar naming convention
#   3. Run `npx tauri build` (or `tauri dev` for debug)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
UI_DIR="$ROOT_DIR/ui"
TAURI_DIR="$UI_DIR/src-tauri"
BIN_DIR="$TAURI_DIR/binaries"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
CYAN='\033[0;36m'
DIM='\033[0;90m'
NC='\033[0m'

log()  { echo -e "${GREEN}[build-ui]${NC} $*"; }
warn() { echo -e "${RED}[build-ui]${NC} $*" >&2; }
dim()  { echo -e "${DIM}$*${NC}"; }

# Parse args
RELEASE=false
TARGET=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --release|-r) RELEASE=true; shift ;;
        --target|-t)  TARGET="$2"; shift 2 ;;
        --help|-h)
            echo "Usage: $0 [--release] [--target <triple>]"
            echo ""
            echo "Options:"
            echo "  --release, -r    Build in release mode (default: debug)"
            echo "  --target, -t     Rust target triple (default: auto-detect)"
            echo ""
            echo "Examples:"
            echo "  $0                                          # debug, current platform"
            echo "  $0 --release                                # release, current platform"
            echo "  $0 --release --target x86_64-pc-windows-msvc"
            exit 0
            ;;
        *) warn "Unknown option: $1"; exit 1 ;;
    esac
done

# Auto-detect target triple
if [[ -z "$TARGET" ]]; then
    TARGET=$(rustc -vV | sed -n 's/host: //p')
fi
log "Target: ${CYAN}$TARGET${NC}"

# Determine binary name
case "$TARGET" in
    *-windows-*) EXE_EXT=".exe" ;;
    *)           EXE_EXT="" ;;
esac
SIDECAR_NAME="rsclaw-${TARGET}${EXE_EXT}"

# Determine cargo profile
if $RELEASE; then
    PROFILE="release"
    PROFILE_DIR="release"
    CARGO_FLAGS="--release"
    TAURI_FLAGS=""
else
    PROFILE="debug"
    PROFILE_DIR="debug"
    CARGO_FLAGS=""
    TAURI_FLAGS="--debug"
fi

# --- Step 1: Build rsclaw CLI ---
log "Building rsclaw CLI (${PROFILE})..."

VERSION="$(grep '^version' "$ROOT_DIR/Cargo.toml" | head -1 | sed 's/.*"\(.*\)"/\1/')"
BUILD_DATE="$(date +%Y-%m-%d)"

log "Version: ${CYAN}${VERSION}${NC}"

(
    cd "$ROOT_DIR"
    RSCLAW_BUILD_VERSION="${VERSION}" \
    RSCLAW_BUILD_DATE="$BUILD_DATE" \
    cargo build $CARGO_FLAGS --target "$TARGET" 2>&1
) || {
    # Fallback: try without --target (native build)
    if [[ "$TARGET" == "$(rustc -vV | sed -n 's/host: //p')" ]]; then
        log "Retrying without explicit --target..."
        (
            cd "$ROOT_DIR"
            RSCLAW_BUILD_VERSION="${VERSION}" \
            RSCLAW_BUILD_DATE="$BUILD_DATE" \
            cargo build $CARGO_FLAGS 2>&1
        )
        # Native build puts binary in target/{profile}/ not target/{target}/{profile}/
        CARGO_OUT="$ROOT_DIR/target/${PROFILE_DIR}/rsclaw${EXE_EXT}"
    fi
}

# Locate the built binary
CARGO_OUT="${CARGO_OUT:-$ROOT_DIR/target/${TARGET}/${PROFILE_DIR}/rsclaw${EXE_EXT}}"
if [[ ! -f "$CARGO_OUT" ]]; then
    # Try native path
    CARGO_OUT="$ROOT_DIR/target/${PROFILE_DIR}/rsclaw${EXE_EXT}"
fi
if [[ ! -f "$CARGO_OUT" ]]; then
    warn "rsclaw binary not found at expected paths"
    warn "  Tried: $ROOT_DIR/target/${TARGET}/${PROFILE_DIR}/rsclaw${EXE_EXT}"
    warn "  Tried: $ROOT_DIR/target/${PROFILE_DIR}/rsclaw${EXE_EXT}"
    exit 1
fi
log "CLI binary: $(du -h "$CARGO_OUT" | cut -f1) ${DIM}($CARGO_OUT)${NC}"

# --- Step 2: Copy to Tauri binaries ---
mkdir -p "$BIN_DIR"
cp "$CARGO_OUT" "$BIN_DIR/$SIDECAR_NAME"
log "Sidecar: ${CYAN}$BIN_DIR/$SIDECAR_NAME${NC}"

# --- Step 3: Install frontend deps ---
if [[ ! -d "$UI_DIR/node_modules" ]]; then
    log "Installing frontend dependencies..."
    (cd "$UI_DIR" && yarn install --frozen-lockfile 2>&1) || \
    (cd "$UI_DIR" && yarn install 2>&1)
fi

# --- Step 4: Build Tauri app ---
log "Building Tauri app (${PROFILE})..."
(
    cd "$UI_DIR"
    npx tauri build $TAURI_FLAGS --target "$TARGET" 2>&1
) || {
    # Some setups need plain `npx tauri build` without --target
    log "Retrying without --target..."
    (cd "$UI_DIR" && npx tauri build $TAURI_FLAGS 2>&1)
}

# --- Done ---
echo ""
# Re-sign with stable identifier so macOS permissions persist across builds
APP_BUNDLE="$TAURI_DIR/target/${TARGET}/${PROFILE_DIR}/bundle/macos/RsClaw.app"
[[ ! -d "$APP_BUNDLE" ]] && APP_BUNDLE="$TAURI_DIR/target/${PROFILE_DIR}/bundle/macos/RsClaw.app"
if [[ -d "$APP_BUNDLE" ]]; then
    log "Re-signing with stable identifier (ai.rsclaw.app)..."
    codesign --force --deep --sign - --identifier "ai.rsclaw.app" "$APP_BUNDLE" 2>&1 || true
fi

log "Build complete!"
echo ""

# Show output files
if $RELEASE; then
    BUNDLE_DIR="$TAURI_DIR/target/${TARGET}/${PROFILE_DIR}/bundle"
    [[ ! -d "$BUNDLE_DIR" ]] && BUNDLE_DIR="$TAURI_DIR/target/${PROFILE_DIR}/bundle"
else
    BUNDLE_DIR="$TAURI_DIR/target/${PROFILE_DIR}/bundle"
fi

if [[ -d "$BUNDLE_DIR" ]]; then
    log "Output:"
    find "$BUNDLE_DIR" -maxdepth 2 -type f \( -name "*.dmg" -o -name "*.app" -o -name "*.msi" -o -name "*.deb" -o -name "*.AppImage" \) 2>/dev/null | while read f; do
        echo "  $(du -h "$f" | cut -f1)  $f"
    done
fi
