#!/usr/bin/env bash
# Local cross-compilation build script for rsclaw
# Builds release binaries for macOS, Linux, and Windows targets.
#
# Usage:
#   ./scripts/build.sh                  # build for current platform only
#   ./scripts/build.sh all              # build all 6 targets
#   ./scripts/build.sh linux            # build all linux targets
#   ./scripts/build.sh macos            # build all macos targets
#   ./scripts/build.sh windows          # build all windows targets
#   ./scripts/build.sh x86_64-apple-darwin aarch64-unknown-linux-musl
#                                       # build specific targets
#
# Cross-compilation toolchains (no Docker needed):
#   macOS host:
#     brew install filosottile/musl-cross/musl-cross   # linux (musl)
#     cargo install cargo-xwin                          # windows (MSVC)
#     rustup target add <target>
#   Linux host:
#     cargo install cargo-xwin                          # windows (MSVC)
#     rustup target add <target>

set -euo pipefail

BINARY="rsclaw"
VERSION="$(git describe --tags --always 2>/dev/null || echo "dev")"
DIST_DIR="dist"

# All supported targets
TARGETS_MACOS=(
    x86_64-apple-darwin
    aarch64-apple-darwin
)
TARGETS_LINUX=(
    x86_64-unknown-linux-musl
    aarch64-unknown-linux-musl
)
TARGETS_WINDOWS=(
    x86_64-pc-windows-msvc
    aarch64-pc-windows-msvc
)
ALL_TARGETS=("${TARGETS_MACOS[@]}" "${TARGETS_LINUX[@]}" "${TARGETS_WINDOWS[@]}")

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
NC='\033[0m'

log()  { echo -e "${CYAN}[build]${NC} $*"; }
ok()   { echo -e "${GREEN}[  ok ]${NC} $*"; }
warn() { echo -e "${YELLOW}[warn]${NC} $*"; }
err()  { echo -e "${RED}[fail]${NC} $*"; }

# Detect current host target triple
detect_host() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Darwin)
            case "$arch" in
                x86_64)  echo "x86_64-apple-darwin" ;;
                arm64)   echo "aarch64-apple-darwin" ;;
                *)       echo "unknown" ;;
            esac ;;
        Linux)
            case "$arch" in
                x86_64)  echo "x86_64-unknown-linux-musl" ;;
                aarch64) echo "aarch64-unknown-linux-musl" ;;
                *)       echo "unknown" ;;
            esac ;;
        *)  echo "unknown" ;;
    esac
}

HOST_TARGET="$(detect_host)"

# Resolve the C cross-compiler for musl-cross Linux targets
musl_cc() {
    local target="$1"
    case "$target" in
        x86_64-unknown-linux-musl)
            echo "x86_64-linux-musl-gcc" ;;
        aarch64-unknown-linux-musl)
            echo "aarch64-linux-musl-gcc" ;;
    esac
}

# Create a CXX wrapper that strips -stdlib=libc++ (unsupported by musl-g++)
# esaxx-rs build.rs hardcodes this flag when host is macOS
musl_cxx_wrapper() {
    local real_cxx="$1"
    local wrapper="/tmp/musl-cxx-wrapper-$$"
    cat > "$wrapper" <<WRAPPER
#!/bin/sh
args=""
for arg in "\$@"; do
    case "\$arg" in
        -stdlib=*) ;;
        *) args="\$args \$arg" ;;
    esac
done
exec $real_cxx \$args
WRAPPER
    chmod +x "$wrapper"
    echo "$wrapper"
}

# Ensure rustup target is installed
ensure_target() {
    local target="$1"
    if ! rustup target list --installed | grep -q "^${target}$"; then
        log "Installing rustup target: $target"
        rustup target add "$target"
    fi
}

# Build a single target
build_target() {
    local target="$1"

    log "Building $target ..."
    ensure_target "$target"

    # macOS targets: native cargo (universal on macOS hosts)
    if [[ "$target" == *"-apple-darwin" ]]; then
        if ! cargo build --release --target "$target"; then
            err "Build failed: $target"
            return 1
        fi

    # Linux musl targets: cargo + musl-cross linker
    elif [[ "$target" == *"-linux-musl" ]]; then
        local cc cxx
        cc="$(musl_cc "$target")"
        cxx="${cc/gcc/g++}"
        if ! command -v "$cc" &>/dev/null; then
            err "$cc not found. Install with: brew install filosottile/musl-cross/musl-cross"
            return 1
        fi
        local upper_target
        upper_target="$(echo "$target" | tr '[:lower:]-' '[:upper:]_')"
        local cxx_wrapper
        cxx_wrapper="$(musl_cxx_wrapper "$cxx")"
        export "CC_${upper_target}=${cc}"
        export "CXX_${upper_target}=${cxx_wrapper}"
        export "CARGO_TARGET_${upper_target}_LINKER=${cc}"
        # Also set generic CXX for crates that ignore target-specific vars
        export CXX="${cxx_wrapper}"
        if ! cargo build --release --target "$target"; then
            err "Build failed: $target"
            return 1
        fi

    # Windows MSVC targets: cargo xwin (needs LLVM tools in PATH)
    elif [[ "$target" == *"-windows-msvc" ]]; then
        if ! command -v cargo-xwin &>/dev/null; then
            err "cargo-xwin not found. Install with: cargo install cargo-xwin"
            return 1
        fi
        # Auto-detect LLVM path for llvm-lib, lld-link etc.
        if ! command -v llvm-lib &>/dev/null; then
            local llvm_bin=""
            for d in /opt/homebrew/opt/llvm*/bin /usr/local/opt/llvm*/bin /opt/homebrew/Cellar/llvm*/*/bin; do
                if [[ -x "$d/llvm-lib" ]]; then
                    llvm_bin="$d"
                    break
                fi
            done
            if [[ -n "$llvm_bin" ]]; then
                log "Adding LLVM to PATH: $llvm_bin"
                export PATH="${llvm_bin}:${PATH}"
            else
                err "llvm-lib not found. Install LLVM with: brew install llvm"
                return 1
            fi
        fi
        if ! cargo xwin build --release --target "$target"; then
            err "Build failed: $target"
            return 1
        fi

    else
        err "Unknown target: $target"
        return 1
    fi

    ok "Built: $target"
    package_target "$target"
}

# Package built binary into dist/
package_target() {
    local target="$1"
    local ext="" archive_name

    mkdir -p "$DIST_DIR"

    if [[ "$target" == *"-windows"* ]]; then
        ext=".exe"
    fi

    local bin_path="target/${target}/release/${BINARY}${ext}"
    if [[ ! -f "$bin_path" ]]; then
        err "Binary not found: $bin_path"
        return 1
    fi

    if [[ "$target" == *"-windows"* ]]; then
        archive_name="${BINARY}-${VERSION}-${target}.zip"
        if command -v zip &>/dev/null; then
            zip -j "${DIST_DIR}/${archive_name}" "$bin_path"
        else
            cp "$bin_path" "${DIST_DIR}/${BINARY}-${VERSION}-${target}${ext}"
            warn "zip not found, copied raw binary instead of archive"
            return 0
        fi
    else
        archive_name="${BINARY}-${VERSION}-${target}.tar.gz"
        COPYFILE_DISABLE=1 tar czf "${DIST_DIR}/${archive_name}" --no-xattrs -C "$(dirname "$bin_path")" "${BINARY}${ext}" 2>/dev/null || \
        COPYFILE_DISABLE=1 tar czf "${DIST_DIR}/${archive_name}" -C "$(dirname "$bin_path")" "${BINARY}${ext}"
    fi

    ok "Packaged: ${DIST_DIR}/${archive_name}"
}

# Generate checksums
generate_checksums() {
    log "Generating checksums ..."
    cd "$DIST_DIR"
    if command -v sha256sum &>/dev/null; then
        sha256sum ${BINARY}-* > SHA256SUMS.txt
    elif command -v shasum &>/dev/null; then
        shasum -a 256 ${BINARY}-* > SHA256SUMS.txt
    else
        warn "No sha256sum/shasum found, skipping checksums"
        cd ..
        return
    fi
    cd ..
    ok "Checksums: ${DIST_DIR}/SHA256SUMS.txt"
}

# Print summary
print_summary() {
    echo ""
    log "Build complete. Artifacts in ${DIST_DIR}/:"
    echo ""
    ls -lh "${DIST_DIR}/" 2>/dev/null
    echo ""
}

# --- Main ---
main() {
    local targets=()

    if [[ $# -eq 0 ]]; then
        if [[ "$HOST_TARGET" == "unknown" ]]; then
            err "Cannot detect host platform"
            exit 1
        fi
        targets=("$HOST_TARGET")
    else
        for arg in "$@"; do
            case "$arg" in
                all)      targets+=("${ALL_TARGETS[@]}") ;;
                macos)    targets+=("${TARGETS_MACOS[@]}") ;;
                linux)    targets+=("${TARGETS_LINUX[@]}") ;;
                windows)  targets+=("${TARGETS_WINDOWS[@]}") ;;
                clean)
                    log "Cleaning dist/ ..."
                    rm -rf "$DIST_DIR"
                    ok "Cleaned"
                    exit 0
                    ;;
                --help|-h)
                    echo "Usage: $0 [all|macos|linux|windows|clean|TARGET...]"
                    echo ""
                    echo "Platform groups:"
                    echo "  all       All 6 targets"
                    echo "  macos     ${TARGETS_MACOS[*]}"
                    echo "  linux     ${TARGETS_LINUX[*]}"
                    echo "  windows   ${TARGETS_WINDOWS[*]}"
                    echo "  clean     Remove dist/ directory"
                    echo ""
                    echo "Or specify exact targets: $0 x86_64-apple-darwin aarch64-unknown-linux-musl"
                    echo ""
                    echo "Host: $HOST_TARGET"
                    echo "cargo-xwin: $(command -v cargo-xwin &>/dev/null && echo yes || echo no)"
                    exit 0
                    ;;
                *)  targets+=("$arg") ;;
            esac
        done
    fi

    # Deduplicate
    local unique_targets=()
    for t in "${targets[@]}"; do
        local dup=false
        for u in "${unique_targets[@]+"${unique_targets[@]}"}"; do
            if [[ "$t" == "$u" ]]; then dup=true; break; fi
        done
        if ! $dup; then
            unique_targets+=("$t")
        fi
    done

    log "rsclaw $VERSION -- building ${#unique_targets[@]} target(s)"
    log "Host: $HOST_TARGET"
    echo ""

    local failed=0
    for target in "${unique_targets[@]}"; do
        if ! build_target "$target"; then
            ((failed++))
        fi
        echo ""
    done

    generate_checksums
    print_summary

    if [[ $failed -gt 0 ]]; then
        err "$failed target(s) failed"
        exit 1
    fi

    ok "All ${#unique_targets[@]} target(s) built successfully"
}

main "$@"
