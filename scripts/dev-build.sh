#!/usr/bin/env bash
#
# Dev build wrapper for the gateway/CLI binary.
#
# Why this script exists
# ----------------------
# `cargo build` produces a binary signed with cargo's default
# `linker-signed adhoc` signature. The codesign Identifier is derived
# from the binary hash, so every rebuild changes the identifier and
# macOS TCC (Privacy & Security) treats it as a fresh, untrusted
# binary. Even when "Accessibility" / "Input Monitoring" toggles still
# look enabled in System Settings, TCC silently denies because the
# identifier in its database no longer matches the on-disk binary.
#
# This wrapper runs `cargo build` and then re-signs the binary with a
# stable ad-hoc identifier (`rsclaw.dev`). After authorising the binary
# once in System Settings, subsequent rebuilds keep the same identifier
# and the grant survives.
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

# Re-sign with a stable identifier. Only meaningful on macOS — on Linux
# / Windows codesign is a no-op so we just skip.
if [ "$(uname)" = "Darwin" ]; then
    if ! command -v codesign >/dev/null 2>&1; then
        echo "[dev-build] WARNING: codesign not found; binary will use cargo's hash-based ad-hoc signature, TCC permissions will not survive rebuilds" >&2
    else
        codesign --force --sign - --identifier "$IDENTIFIER" "$binary" 2>&1 |
            grep -v "replacing existing signature" || true
        echo "[dev-build] $binary signed with identifier=$IDENTIFIER"
    fi
fi
