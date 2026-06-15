#!/usr/bin/env bash
#
# Dev build wrapper for the gateway/CLI binary.
#
# Why this script exists
# ----------------------
# `cargo build` produces a binary signed with cargo's default
# `linker-signed adhoc` signature, whose codesign Identifier is derived
# from the binary hash. macOS TCC (Privacy & Security) keys its grants
# on the binary's code-signing *designated requirement*.
#
# IMPORTANT: for an AD-HOC signature (`codesign --sign -`) that
# requirement is a pinned **cdhash** — and a stable `--identifier` does
# NOT change that. So every rebuild produces a new cdhash and the grant
# silently stops applying (Screen Recording / Accessibility look enabled
# in System Settings but capture fails with `Failed to copy data` and
# input no-ops). A stable identifier alone is NOT enough; the binary must
# be signed with a real (even self-signed) IDENTITY so the requirement
# becomes identity-based and survives rebuilds.
#
# This wrapper therefore signs with a real code-signing identity when one
# is available (auto-detected, or `RSCLAW_CODESIGN_IDENTITY`), and falls
# back to ad-hoc only with a loud warning. After authorising the binary
# once in System Settings, subsequent rebuilds keep the same identity and
# the grant survives. (Import a stable identity once via:
#   security import id.p12 -k ~/Library/Keychains/login.keychain-db \
#     -P <pw> -T /usr/bin/codesign -A )
#
# Usage
# -----
#   bash scripts/dev-build.sh          # debug build (target/debug/rsclaw)
#   bash scripts/dev-build.sh --release # release build (target/release/rsclaw)
#
# Anything you'd pass to cargo build can be appended verbatim.
#
# First-time setup on macOS
# -------------------------
#   1. bash scripts/dev-build.sh
#   2. ./target/debug/rsclaw gateway run    # triggers TCC prompt on first
#                                           # input-simulation attempt
#   3. System Settings -> Privacy & Security ->
#        Accessibility    : enable rsclaw
#        Input Monitoring : enable rsclaw
#        (Screen Recording: usually auto-prompted on first capture)
#   4. Restart the gateway. Permissions are now stable across rebuilds.
#
# CAVEAT — DO NOT use `cargo run -- gateway restart` after this script.
# `cargo run` re-runs the linker, which re-applies cargo's default
# `linker-signed adhoc` signature using a hash-derived identifier,
# clobbering the stable `rsclaw.dev` identifier this script set. To
# restart, kill the old process and exec the binary directly:
#
#   pkill -f "target/debug/rsclaw"
#   ./target/debug/rsclaw gateway run
#
# Or, if you trust your edits compiled cleanly:
#
#   bash scripts/dev-build.sh && pkill -f "target/debug/rsclaw" && \
#     ./target/debug/rsclaw gateway run &

set -euo pipefail

# Stable identifier the binary is signed with. Picked so System Settings
# shows a recognisable name and TCC has something to key on.
IDENTIFIER="rsclaw.dev"

# Pass everything through to cargo so callers can use --release, -p, etc.
cargo build "$@"

# Determine the produced binary path. Mirrors cargo's profile -> target dir
# convention; only --release matters for our purposes.
profile_dir="debug"
for arg in "$@"; do
    if [ "$arg" = "--release" ] || [ "$arg" = "-r" ]; then
        profile_dir="release"
    fi
done
binary="target/${profile_dir}/rsclaw"

if [ ! -f "$binary" ]; then
    echo "[dev-build] expected binary at $binary not found" >&2
    exit 1
fi

# Re-sign with a stable identity + identifier so macOS TCC grants survive
# rebuilds. Only meaningful on macOS — on Linux / Windows codesign is a
# no-op so we just skip.
if [ "$(uname)" = "Darwin" ]; then
    if ! command -v codesign >/dev/null 2>&1; then
        echo "[dev-build] WARNING: codesign not found; binary will use cargo's hash-based ad-hoc signature, TCC permissions will not survive rebuilds" >&2
    else
        # Pick a stable signing identity: explicit override wins, else the
        # first valid code-signing identity in the keychain (by SHA-1, which
        # is unambiguous), else none.
        identity="${RSCLAW_CODESIGN_IDENTITY:-}"
        if [ -z "$identity" ]; then
            identity="$(security find-identity -v -p codesigning 2>/dev/null \
                | awk '/^[[:space:]]*[0-9]+\)/ {print $2; exit}')"
        fi
        if [ -n "$identity" ]; then
            codesign --force --sign "$identity" --identifier "$IDENTIFIER" "$binary" 2>&1 |
                grep -v "replacing existing signature" || true
            echo "[dev-build] $binary signed with stable identity=$identity identifier=$IDENTIFIER (TCC grants survive rebuilds)"
        else
            codesign --force --sign - --identifier "$IDENTIFIER" "$binary" 2>&1 |
                grep -v "replacing existing signature" || true
            echo "[dev-build] WARNING: no code-signing identity in keychain — signed AD-HOC (identifier=$IDENTIFIER)." >&2
            echo "[dev-build]          macOS TCC grants (Screen Recording / Accessibility) will NOT survive rebuilds — the" >&2
            echo "[dev-build]          designated requirement is a cdhash that changes every build. Import a stable identity" >&2
            echo "[dev-build]          (security import id.p12 -T /usr/bin/codesign -A) or set RSCLAW_CODESIGN_IDENTITY." >&2
        fi
    fi
fi
