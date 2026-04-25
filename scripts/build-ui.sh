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
# Tauri v2 doesn't sign debug builds from tauri.conf.json — resign explicitly
# with the Developer ID identity so macOS TCC permissions persist across builds.
SIGN_IDENTITY="${APPLE_SIGNING_IDENTITY:-Developer ID Application: Hua Lan (K87X8CQ78Y)}"
APP_BUNDLE="$TAURI_DIR/target/${TARGET}/${PROFILE_DIR}/bundle/macos/RsClaw.app"
[[ ! -d "$APP_BUNDLE" ]] && APP_BUNDLE="$TAURI_DIR/target/${PROFILE_DIR}/bundle/macos/RsClaw.app"
if [[ -d "$APP_BUNDLE" ]] && [[ "$(uname -s)" == "Darwin" ]]; then
    # Inject TCC usage descriptions — macOS silently denies Automation/Screen
    # Recording without these keys (no prompt, no error). Must be done BEFORE
    # codesign so they're covered by the signature.
    PLIST="$APP_BUNDLE/Contents/Info.plist"
    log "Injecting TCC usage descriptions into Info.plist..."
    plutil -replace NSAppleEventsUsageDescription -string \
        "RsClaw needs to automate other apps (WeChat, System Events, etc.) to perform tasks on your behalf." \
        "$PLIST" 2>&1 || true
    plutil -replace NSScreenCaptureUsageDescription -string \
        "RsClaw needs to capture the screen to see UI state for agentic tasks." \
        "$PLIST" 2>&1 || true

    # Entitlements file: required for hardened runtime to allow Automation,
    # child process launching, and library loading. Without this, macOS blocks
    # osascript/screencapture even though TCC is granted.
    ENTITLEMENTS="$TAURI_DIR/entitlements.plist"
    if [[ -f "$ENTITLEMENTS" ]]; then
        log "Using entitlements: $ENTITLEMENTS"
    fi

    # Sign inside-out: all nested executables first, then the app bundle.
    # Each binary must be signed individually with entitlements because
    # --deep does not propagate entitlements, and signing the bundle
    # after --deep strips entitlements from nested binaries.
    SIGN_BASE=(--force --sign "$SIGN_IDENTITY" --options runtime --timestamp)
    if [[ -f "$ENTITLEMENTS" ]]; then
        SIGN_BASE+=(--entitlements "$ENTITLEMENTS")
    fi

    # Sign all Mach-O binaries inside the bundle individually.
    log "Signing all binaries inside app bundle..."
    while IFS= read -r -d '' bin; do
        if file "$bin" | grep -q "Mach-O"; then
            log "  signing: $(basename "$bin")"
            codesign "${SIGN_BASE[@]}" "$bin" 2>&1 || true
        fi
    done < <(find "$APP_BUNDLE/Contents/MacOS" -type f -print0)

    # Sign any frameworks/dylibs
    while IFS= read -r -d '' fw; do
        codesign "${SIGN_BASE[@]}" "$fw" 2>&1 || true
    done < <(find "$APP_BUNDLE/Contents/Frameworks" -type f -name "*.dylib" -print0 2>/dev/null)

    SIGN_ARGS=(--force --sign "$SIGN_IDENTITY" --options runtime --timestamp)
    if [[ -f "$ENTITLEMENTS" ]]; then
        SIGN_ARGS+=(--entitlements "$ENTITLEMENTS")
    fi

    log "Signing app bundle..."
    if codesign "${SIGN_ARGS[@]}" "$APP_BUNDLE" 2>&1; then
        # Re-sign sidecar AFTER bundle — codesign on an app bundle strips
        # entitlements from nested binaries. The sidecar needs allow-jit for
        # wasmtime JIT; without it macOS kills the process (SIGKILL Invalid Page).
        SIDECAR_BIN="$APP_BUNDLE/Contents/MacOS/rsclaw"
        if [[ -f "$SIDECAR_BIN" ]] && [[ -f "$ENTITLEMENTS" ]]; then
            log "Re-signing sidecar with entitlements (post-bundle)..."
            codesign --force --sign "$SIGN_IDENTITY" --options runtime --timestamp \
                --entitlements "$ENTITLEMENTS" "$SIDECAR_BIN" 2>&1 || true
        fi
        log "App signed successfully"

        # Notarize with Apple (requires APPLE_ID + app-specific password in keychain
        # or env vars). Without notarization, Gatekeeper shows "cannot verify" warning.
        APPLE_ID="${APPLE_ID:-}"
        APPLE_TEAM_ID="${APPLE_TEAM_ID:-K87X8CQ78Y}"
        # Try keychain profile first, then env var password
        if xcrun notarytool history --keychain-profile "notarytool-profile" >/dev/null 2>&1; then
            log "Notarizing with keychain profile..."
            ZIP_PATH="/tmp/RsClaw-notarize.zip"
            ditto -c -k --keepParent "$APP_BUNDLE" "$ZIP_PATH"
            if xcrun notarytool submit "$ZIP_PATH" \
                --keychain-profile "notarytool-profile" \
                --wait 2>&1; then
                log "Notarization succeeded"
                xcrun stapler staple "$APP_BUNDLE" 2>&1 || true
            else
                warn "Notarization failed — app will trigger Gatekeeper warning"
            fi
            rm -f "$ZIP_PATH"
        elif [[ -n "$APPLE_ID" && -n "${APPLE_PASSWORD:-}" ]]; then
            log "Notarizing with APPLE_ID..."
            ZIP_PATH="/tmp/RsClaw-notarize.zip"
            ditto -c -k --keepParent "$APP_BUNDLE" "$ZIP_PATH"
            if xcrun notarytool submit "$ZIP_PATH" \
                --apple-id "$APPLE_ID" \
                --password "$APPLE_PASSWORD" \
                --team-id "$APPLE_TEAM_ID" \
                --wait 2>&1; then
                log "Notarization succeeded"
                xcrun stapler staple "$APP_BUNDLE" 2>&1 || true
            else
                warn "Notarization failed — app will trigger Gatekeeper warning"
            fi
            rm -f "$ZIP_PATH"
        else
            dim "Skipping notarization (no keychain profile or APPLE_ID set)"
            dim "To enable: xcrun notarytool store-credentials notarytool-profile"
        fi
    else
        warn "Signing failed — app will use ad-hoc signature"
    fi
fi

# Rebuild DMG after re-signing so it contains the correctly entitled binaries.
# Tauri generates the DMG before our post-build signing steps.
if $RELEASE && [[ "$(uname -s)" == "Darwin" ]] && [[ -d "$APP_BUNDLE" ]]; then
    BUNDLE_DIR="$TAURI_DIR/target/${TARGET}/${PROFILE_DIR}/bundle"
    [[ ! -d "$BUNDLE_DIR" ]] && BUNDLE_DIR="$TAURI_DIR/target/${PROFILE_DIR}/bundle"
    DMG_DIR="$BUNDLE_DIR/dmg"
    if [[ -d "$DMG_DIR" ]]; then
        DMG_NAME=$(ls "$DMG_DIR"/*.dmg 2>/dev/null | head -1)
        if [[ -n "$DMG_NAME" ]]; then
            log "Rebuilding DMG with re-signed app..."
            rm -f "$DMG_NAME"
            hdiutil create -volname "RsClaw" -srcfolder "$APP_BUNDLE" \
                -ov -format UDZO "$DMG_NAME" 2>&1 || warn "DMG rebuild failed"
        fi
    fi
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
