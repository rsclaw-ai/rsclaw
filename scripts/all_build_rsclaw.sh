#!/usr/bin/env bash
# Build one static Linux RsClaw binary, upload only that binary, and run a
# remote gateway/A2A smoke test. No source tree, config, or build artifact
# directory is transferred.
#
# Usage:
#   bash scripts/all_build_rsclaw.sh
#   BUILD_ONLY=1 TARGET=x86_64-unknown-linux-musl bash scripts/all_build_rsclaw.sh
#
# Optional home-level entry point (run outside a workspace-restricted agent):
#   install -m 755 scripts/all_build_rsclaw.sh ~/all_build_rsclaw.sh
#   RSCLAW_REPO_ROOT=/absolute/path/to/rsclaw ~/all_build_rsclaw.sh
#
# Environment:
#   RSCLAW_REPO_ROOT      repository root (required for a copied home-level entry point)
#   RSCLAW_E2E_NODE       cls node name (default: DEFAULT-UNKNOWN-LINUX-CURSOR)
#   TARGET                build-only target, or expected remote target assertion
#   BUILD_ONLY            1 = build and verify locally, do not contact cls
#   SKIP_REMOTE_E2E       1 = upload/install but skip gateway/A2A smoke
#   DEPLOY_EXISTING       1 = deploy an already verified binary without rebuilding
#   EXPECTED_BINARY_SHA   required exact SHA-256 for DEPLOY_EXISTING=1
#   EXPECTED_SOURCE_TREE  immutable Git tree used to build the existing binary
#   EXPECTED_RUNNER_SHA   exact SHA-256 of this deployment script
#   RSCLAW_REMOTE_PORT    isolated remote smoke port (default: 28889)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR=""
NODE="${RSCLAW_E2E_NODE:-DEFAULT-UNKNOWN-LINUX-CURSOR}"
BUILD_ONLY="${BUILD_ONLY:-0}"
SKIP_REMOTE_E2E="${SKIP_REMOTE_E2E:-0}"
DEPLOY_EXISTING="${DEPLOY_EXISTING:-0}"
EXPECTED_BINARY_SHA="${EXPECTED_BINARY_SHA:-}"
EXPECTED_SOURCE_TREE="${EXPECTED_SOURCE_TREE:-}"
EXPECTED_RUNNER_SHA="${EXPECTED_RUNNER_SHA:-}"
REMOTE_PORT="${RSCLAW_REMOTE_PORT:-28889}"
REMOTE_STAGING=""
REMOTE_UPLOAD=""
REMOTE_STAGING_CREATED=0
REMOTE_STAGING_TOKEN=""
LOCAL_STAGING=""
LOCAL_SOURCE_STAGING=""
FROZEN_SOURCE_TREE=""

log() {
  printf '[all-build-rsclaw] %s\n' "$*"
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    printf 'error: required command not found: %s\n' "$1" >&2
    exit 2
  }
}

output_has_evidence() {
  local output="$1" expected="$2"
  printf '%s\n' "$output" | awk -v expected="$expected" '
    {
      line = $0
      sub(/^\[[^][]+\][[:space:]]+/, "", line)
      if (line == expected) found = 1
    }
    END { exit(found ? 0 : 1) }
  '
}

require_bool() {
  case "$2" in
    0|1) ;;
    *)
      printf 'error: %s must be 0 or 1, got %s\n' "$1" "$2" >&2
      exit 2
      ;;
  esac
}

cleanup_staging() {
  local exit_status=$?
  if [[ -n "$LOCAL_STAGING" ]]; then
    if ! rm -rf -- "$LOCAL_STAGING"; then
      printf 'warning: could not remove local staging directory: %s\n' \
        "$LOCAL_STAGING" >&2
    fi
  fi
  if [[ -n "$LOCAL_SOURCE_STAGING" ]]; then
    if ! rm -rf -- "$LOCAL_SOURCE_STAGING"; then
      printf 'warning: could not remove frozen source directory: %s\n' \
        "$LOCAL_SOURCE_STAGING" >&2
    fi
  fi
  if [[ "$REMOTE_STAGING_CREATED" == '1' && -n "$REMOTE_STAGING" && -n "$REMOTE_STAGING_TOKEN" ]]; then
    if ! cls run -n "$NODE" -t 30 \
      "set -eu; staging='$REMOTE_STAGING'; token='$REMOTE_STAGING_TOKEN'; sentinel=\"\$staging/.rsclaw-owner\"; if [ -d \"\$staging\" ] && [ ! -L \"\$staging\" ] && [ \"\$(stat -c %u \"\$staging\")\" = \"\$(id -u)\" ] && [ \"\$(stat -c %a \"\$staging\")\" = 700 ] && [ -f \"\$sentinel\" ] && [ ! -L \"\$sentinel\" ] && [ \"\$(stat -c %u \"\$sentinel\")\" = \"\$(id -u)\" ] && [ \"\$(stat -c %a \"\$sentinel\")\" = 600 ] && [ \"\$(cat \"\$sentinel\")\" = \"\$token\" ]; then rm -rf -- \"\$staging\"; fi" >/dev/null 2>&1; then
      printf 'warning: could not remove remote staging directory: %s\n' \
        "$REMOTE_STAGING" >&2
    fi
  fi
  return "$exit_status"
}

resolve_root_dir() {
  local candidate
  if [[ -n "${RSCLAW_REPO_ROOT:-}" ]]; then
    candidate="$RSCLAW_REPO_ROOT"
  elif [[ -f "$SCRIPT_DIR/../Cargo.toml" && -f "$SCRIPT_DIR/build.sh" ]]; then
    candidate="$SCRIPT_DIR/.."
  elif [[ -f "$PWD/Cargo.toml" && -f "$PWD/scripts/build.sh" ]]; then
    candidate="$PWD"
  else
    printf 'error: cannot locate RsClaw repository; set RSCLAW_REPO_ROOT\n' >&2
    return 1
  fi
  if [[ ! -f "$candidate/Cargo.toml" || ! -f "$candidate/scripts/build.sh" ]]; then
    printf 'error: RSCLAW_REPO_ROOT is not an RsClaw repository: %s\n' "$candidate" >&2
    return 1
  fi
  (cd "$candidate" && pwd)
}

sha256_stream() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 | awk '{print $1}'
  else
    echo 'error: sha256sum or shasum is required' >&2
    return 1
  fi
}

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    echo 'error: sha256sum or shasum is required' >&2
    return 1
  fi
}

random_hex() {
  local bytes="$1" value
  [[ -r /dev/urandom ]] || {
    printf 'error: /dev/urandom is required\n' >&2
    return 1
  }
  value="$(od -An -N"$bytes" -tx1 /dev/urandom | tr -d ' \n')"
  [[ "${#value}" -eq $((bytes * 2)) ]] || {
    printf 'error: could not generate random identifier\n' >&2
    return 1
  }
  printf '%s\n' "$value"
}

freeze_source_tree() {
  local index_file source_dir
  LOCAL_SOURCE_STAGING="$(mktemp -d "$ROOT_DIR/target/rsclaw-source.XXXXXX")"
  index_file="$LOCAL_SOURCE_STAGING/index"
  source_dir="$LOCAL_SOURCE_STAGING/source"
  mkdir -m 700 "$source_dir"

  GIT_INDEX_FILE="$index_file" git -C "$ROOT_DIR" read-tree HEAD
  GIT_INDEX_FILE="$index_file" git -C "$ROOT_DIR" add -A -- .
  FROZEN_SOURCE_TREE="$(GIT_INDEX_FILE="$index_file" git -C "$ROOT_DIR" write-tree)"
  [[ "$FROZEN_SOURCE_TREE" =~ ^[0-9a-f]{40,64}$ ]] || {
    printf 'error: could not create immutable source tree\n' >&2
    return 1
  }
  git -C "$ROOT_DIR" archive "$FROZEN_SOURCE_TREE" | tar -xf - -C "$source_dir"
  rm -f -- "$index_file"

  mkdir -m 700 "$LOCAL_SOURCE_STAGING/build-target"
  mkdir -m 700 "$LOCAL_SOURCE_STAGING/build-dist"
  ln -s "$LOCAL_SOURCE_STAGING/build-target" "$source_dir/target"
  ln -s "$LOCAL_SOURCE_STAGING/build-dist" "$source_dir/dist"
  [[ -f "$source_dir/Cargo.toml" && -f "$source_dir/scripts/build.sh" ]] || {
    printf 'error: frozen source tree is incomplete\n' >&2
    return 1
  }
}

assert_existing_artifact_source() {
  local built_tree="$1" current_tree="$2" path
  local changed_paths="$LOCAL_SOURCE_STAGING/changed-paths"
  git -C "$ROOT_DIR" cat-file -e "$built_tree^{tree}"
  git -C "$ROOT_DIR" diff --name-only -z "$built_tree" "$current_tree" >"$changed_paths"
  while IFS= read -r -d '' path; do
    case "$path" in
      scripts/all_build_rsclaw.sh|docs/reviews/dev.md) ;;
      *)
        printf 'error: existing binary source changed at %s; rebuild is required\n' "$path" >&2
        return 1
        ;;
    esac
  done <"$changed_paths"
}

publish_local_artifacts() {
  local private_binary="$1" private_dist="$2" target="$3"
  local target_dir="$ROOT_DIR/target/$target/release"
  local binary_temp archive archive_temp checksums_temp supported archive_path
  local checksum_files=()

  mkdir -p "$target_dir" "$ROOT_DIR/dist"
  binary_temp="$(mktemp "$target_dir/rsclaw.publish.XXXXXX")"
  cp "$private_binary" "$binary_temp"
  chmod 755 "$binary_temp"
  mv -f -- "$binary_temp" "$target_dir/rsclaw"

  archive_path="$private_dist/rsclaw-$RSCLAW_BUILD_VERSION-$target.tar.gz"
  [[ -f "$archive_path" ]] || {
    printf 'error: expected private archive not found: %s\n' "$archive_path" >&2
    return 1
  }
  archive="$archive_path"
  archive_temp="$(mktemp "$ROOT_DIR/dist/rsclaw-archive.publish.XXXXXX")"
  cp "$archive" "$archive_temp"
  mv -f -- "$archive_temp" "$ROOT_DIR/dist/$(basename "$archive")"

  for supported in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
    archive_path="rsclaw-$RSCLAW_BUILD_VERSION-$supported.tar.gz"
    if [[ -f "$ROOT_DIR/dist/$archive_path" ]]; then
      checksum_files+=("$archive_path")
    fi
  done
  [[ "${#checksum_files[@]}" -gt 0 ]] || {
    printf 'error: no supported release archives available for checksums\n' >&2
    return 1
  }
  checksums_temp="$(mktemp "$ROOT_DIR/dist/SHA256SUMS.publish.XXXXXX")"
  (
    cd "$ROOT_DIR/dist"
    if command -v sha256sum >/dev/null 2>&1; then
      sha256sum "${checksum_files[@]}" >"$checksums_temp"
    else
      shasum -a 256 "${checksum_files[@]}" >"$checksums_temp"
    fi
  )
  mv -f -- "$checksums_temp" "$ROOT_DIR/dist/SHA256SUMS.txt"
}

target_from_arch_output() {
  local output="$1" targets
  targets="$(printf '%s\n' "$output" | awk '
    {
      for (i = 1; i <= NF; i++) {
        if ($i == "x86_64") print "x86_64-unknown-linux-musl"
        if ($i == "aarch64" || $i == "arm64") print "aarch64-unknown-linux-musl"
      }
    }
  ' | sort -u)"
  case "$targets" in
    x86_64-unknown-linux-musl|aarch64-unknown-linux-musl) printf '%s\n' "$targets" ;;
    *)
      printf 'error: unsupported or ambiguous remote architecture: %s\n' "$output" >&2
      return 1
      ;;
  esac
}

detect_remote_target() {
  local output
  output="$(cls run -n "$NODE" -t 30 'uname -m')"
  target_from_arch_output "$output"
}

create_remote_staging() {
  local remote_nonce
  remote_nonce="$(random_hex 16)"
  REMOTE_STAGING_TOKEN="$(random_hex 32)"
  REMOTE_STAGING="/tmp/rsclaw-e2e-$remote_nonce"
  REMOTE_UPLOAD="$REMOTE_STAGING/rsclaw"
  REMOTE_STAGING_CREATED=1
  log "creating isolated upload directory on $NODE"
  cls run -n "$NODE" -t 30 \
    "set -eu; umask 077; staging='$REMOTE_STAGING'; token='$REMOTE_STAGING_TOKEN'; sentinel=\"\$staging/.rsclaw-owner\"; admitted=0; staging_is_owned() { [ -d \"\$staging\" ] && [ ! -L \"\$staging\" ] && [ \"\$(stat -c %u \"\$staging\")\" = \"\$(id -u)\" ] && [ \"\$(stat -c %a \"\$staging\")\" = 700 ] && [ -f \"\$sentinel\" ] && [ ! -L \"\$sentinel\" ] && [ \"\$(stat -c %u \"\$sentinel\")\" = \"\$(id -u)\" ] && [ \"\$(stat -c %a \"\$sentinel\")\" = 600 ] && [ \"\$(cat \"\$sentinel\")\" = \"\$token\" ]; }; cleanup_create() { if [ \"\$admitted\" = 1 ] && staging_is_owned; then rm -rf -- \"\$staging\"; fi; }; trap cleanup_create EXIT INT TERM; mkdir -m 700 -- \"\$staging\"; printf '%s' \"\$token\" >\"\$sentinel\"; chmod 600 \"\$sentinel\"; staging_is_owned; admitted=1; trap - EXIT INT TERM"
}

remote_smoke_command() {
  local expected_sha="$1"
  local upload_path="$2"
  local port="$3"
  local expected_target="$4"
  local expected_version="$5"
  local staging_token="$6"
  cat <<REMOTE
set -eu
upload='$upload_path'
expected_sha='$expected_sha'
port='$port'
expected_target='$expected_target'
expected_version='$expected_version'
staging_token='$staging_token'
bin="\$upload"
base="\$(mktemp -d /tmp/rsclaw-e2e.XXXXXX)"
pid=''
pid_starttime=''
process_stat_field() {
  stat_value="\$(cat "/proc/\$pid/stat" 2>/dev/null || true)"
  stat_fields="\${stat_value#*) }"
  case "\$1" in
    state) printf '%s\n' "\${stat_fields%% *}" ;;
    starttime) printf '%s\n' "\$stat_fields" | awk '{print \$20}' ;;
  esac
}
process_is_gateway() {
  [ -n "\$pid" ] && [ -n "\$pid_starttime" ] &&
    [ "\$(process_stat_field starttime)" = "\$pid_starttime" ]
}
stop_gateway() {
  if [ -z "\$pid" ]; then
    return 0
  fi
  if ! process_is_gateway; then
    if ! kill -0 "\$pid" 2>/dev/null; then
      wait "\$pid" 2>/dev/null || true
    fi
    pid=''
    pid_starttime=''
    return 0
  fi
  kill "\$pid" 2>/dev/null || true
  remaining=10
  while [ "\$remaining" -gt 0 ] && process_is_gateway; do
    if [ "\$(process_stat_field state)" = 'Z' ]; then
      break
    fi
    sleep 1
    remaining=\$((remaining - 1))
  done
  if process_is_gateway && [ "\$(process_stat_field state)" != 'Z' ]; then
    kill -9 "\$pid" 2>/dev/null || true
    remaining=5
    while [ "\$remaining" -gt 0 ] && process_is_gateway; do
      if [ "\$(process_stat_field state)" = 'Z' ]; then
        break
      fi
      sleep 1
      remaining=\$((remaining - 1))
    done
  fi
  if process_is_gateway && [ "\$(process_stat_field state)" != 'Z' ]; then
    echo 'error: gateway process did not stop after KILL' >&2
    return 1
  fi
  if [ "\$(process_stat_field state)" = 'Z' ] || ! kill -0 "\$pid" 2>/dev/null; then
    wait "\$pid" 2>/dev/null || true
  fi
  pid=''
  pid_starttime=''
}
staging_dir="\$(dirname "\$upload")"
sentinel="\$staging_dir/.rsclaw-owner"
staging_is_owned() {
  [ -d "\$staging_dir" ] &&
    [ ! -L "\$staging_dir" ] &&
    [ "\$(stat -c %u "\$staging_dir")" = "\$(id -u)" ] &&
    [ "\$(stat -c %a "\$staging_dir")" = 700 ] &&
    [ -f "\$sentinel" ] &&
    [ ! -L "\$sentinel" ] &&
    [ "\$(stat -c %u "\$sentinel")" = "\$(id -u)" ] &&
    [ "\$(stat -c %a "\$sentinel")" = 600 ] &&
    [ "\$(cat "\$sentinel")" = "\$staging_token" ]
}
remove_owned_staging() {
  if ! staging_is_owned; then
    echo 'error: refusing to remove unowned remote staging' >&2
    return 1
  fi
  rm -rf -- "\$staging_dir"
  [ ! -e "\$staging_dir" ]
}
cleanup() {
  cleanup_status=0
  stop_gateway || cleanup_status=\$?
  rm -rf "\$base" || cleanup_status=1
  remove_owned_staging || cleanup_status=1
  return "\$cleanup_status"
}
finish_cleanup() {
  rm -rf "\$base"
  [ ! -e "\$base" ]
  remove_owned_staging
  trap - EXIT INT TERM
}
trap cleanup EXIT INT TERM

[ -r /proc/self/stat ]
staging_is_owned

case "\$(uname -m)" in
  x86_64) actual_target='x86_64-unknown-linux-musl' ;;
  aarch64|arm64) actual_target='aarch64-unknown-linux-musl' ;;
  *) echo 'error: unsupported remote architecture during smoke' >&2; exit 1 ;;
esac
if [ "\$actual_target" != "\$expected_target" ]; then
  echo "error: remote architecture changed: expected \$expected_target, got \$actual_target" >&2
  exit 1
fi
printf 'REMOTE_TARGET=%s\n' "\$actual_target"

if command -v sha256sum >/dev/null 2>&1; then
  actual_sha="\$(sha256sum "\$upload" | awk '{print \$1}')"
elif command -v shasum >/dev/null 2>&1; then
  actual_sha="\$(shasum -a 256 "\$upload" | awk '{print \$1}')"
else
  echo 'error: remote sha256sum or shasum is required' >&2
  exit 1
fi
[ "\$actual_sha" = "\$expected_sha" ]
printf 'REMOTE_SHA256=%s\n' "\$actual_sha"
chmod 700 "\$bin"
[ -x "\$bin" ]
version_output="\$("\$bin" --version)"
printf '%s\n' "\$version_output"
[ "\$version_output" = "rsclaw \$expected_version" ]
printf 'REMOTE_VERSION=%s\n' "\$version_output"

if [ '$SKIP_REMOTE_E2E' = '1' ]; then
  finish_cleanup
  echo 'REMOTE_BINARY_OK'
  exit 0
fi

if [ ! -r /dev/urandom ]; then
  echo 'error: /dev/urandom is required for the isolated E2E token' >&2
  exit 1
fi
token="\$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')"
[ "\${#token}" = '64' ]
printf 'header = "Authorization: Bearer %s"\n' "\$token" >"\$base/curl-auth.conf"
chmod 600 "\$base/curl-auth.conf"

cat >"\$base/rsclaw.json5" <<EOF
{
  gateway: {
    port: \$port,
    bind: "loopback"
  }
}
EOF
RSCLAW_BASE_DIR="\$base" \
RSCLAW_CONFIG_PATH="\$base/rsclaw.json5" \
RSCLAW_AUTH_TOKEN="\$token" \
"\$bin" gateway run >"\$base/gateway.log" 2>&1 &
pid=\$!
pid_starttime="\$(process_stat_field starttime)"
if [ -z "\$pid_starttime" ]; then
  echo 'error: could not bind gateway process identity' >&2
  if ! kill -0 "\$pid" 2>/dev/null; then
    wait "\$pid" 2>/dev/null || true
  fi
  pid=''
  exit 1
fi

healthy=0
for _ in \$(seq 1 120); do
  if ! process_is_gateway; then
    cat "\$base/gateway.log" >&2
    exit 1
  fi
  if body=\$(curl -fsS --max-time 2 "http://127.0.0.1:\$port/api/v1/health" 2>/dev/null) \
      && printf '%s' "\$body" | grep -q '"status":"ok"'; then
    healthy=1
    break
  fi
  sleep 1
done
if [ "\$healthy" != '1' ]; then
  cat "\$base/gateway.log" >&2
  exit 1
fi

status="\$(curl -sS --max-time 5 -o "\$base/config-unauth.json" -w '%{http_code}' \
  "http://127.0.0.1:\$port/api/v1/config")"
[ "\$status" = '401' ]
status="\$(curl -sS --max-time 5 -o "\$base/a2a-unauth.json" -w '%{http_code}' \
  -H 'A2A-Version: 1.0' \
  -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","id":"unauth","method":"GetExtendedAgentCard","params":{}}' \
  "http://127.0.0.1:\$port/api/v1/a2a")"
[ "\$status" = '401' ]
status="\$(curl -sS --max-time 5 -o "\$base/config-auth.json" -w '%{http_code}' \
  --config "\$base/curl-auth.conf" \
  "http://127.0.0.1:\$port/api/v1/config")"
[ "\$status" = '200' ]
grep -q '"raw"' "\$base/config-auth.json"
status="\$(curl -sS --max-time 5 -o "\$base/card.json" -w '%{http_code}' \
  "http://127.0.0.1:\$port/.well-known/agent.json")"
[ "\$status" = '200' ]
grep -q '"protocolVersion"' "\$base/card.json"
status="\$(curl -sS --max-time 5 -o "\$base/a2a.json" -w '%{http_code}' \
  --config "\$base/curl-auth.conf" \
  -H 'A2A-Version: 1.0' \
  -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","id":"remote-e2e","method":"GetExtendedAgentCard","params":{}}' \
  "http://127.0.0.1:\$port/api/v1/a2a")"
[ "\$status" = '200' ]
grep -q '"id":"remote-e2e"' "\$base/a2a.json"
grep -q '"result"' "\$base/a2a.json"
! grep -q '"error"' "\$base/a2a.json"

stop_gateway
finish_cleanup
echo 'REMOTE_E2E_OK'
REMOTE
}

main() {
  require_cmd awk
  require_cmd git
  require_cmd mktemp
  require_cmd od
  require_cmd tar
  require_cmd tr
  require_bool BUILD_ONLY "$BUILD_ONLY"
  require_bool SKIP_REMOTE_E2E "$SKIP_REMOTE_E2E"
  require_bool DEPLOY_EXISTING "$DEPLOY_EXISTING"
  if [[ "$DEPLOY_EXISTING" != '1' ]]; then
    require_cmd cargo
    require_cmd rustup
  fi
  if [[ "$DEPLOY_EXISTING" == '1' && "$BUILD_ONLY" == '1' ]]; then
    printf 'error: DEPLOY_EXISTING=1 cannot be combined with BUILD_ONLY=1\n' >&2
    exit 2
  fi
  if [[ "$DEPLOY_EXISTING" == '1' ]]; then
    if [[ ! "$EXPECTED_BINARY_SHA" =~ ^[0-9a-f]{64}$ ]]; then
      printf 'error: DEPLOY_EXISTING=1 requires a lowercase 64-character EXPECTED_BINARY_SHA\n' >&2
      exit 2
    fi
    if [[ ! "$EXPECTED_SOURCE_TREE" =~ ^[0-9a-f]{40,64}$ ]]; then
      printf 'error: DEPLOY_EXISTING=1 requires EXPECTED_SOURCE_TREE\n' >&2
      exit 2
    fi
    if [[ ! "$EXPECTED_RUNNER_SHA" =~ ^[0-9a-f]{64}$ ]]; then
      printf 'error: DEPLOY_EXISTING=1 requires EXPECTED_RUNNER_SHA\n' >&2
      exit 2
    fi
  fi
  if [[ ! "$NODE" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]]; then
    printf 'error: RSCLAW_E2E_NODE must name exactly one safe node\n' >&2
    exit 2
  fi
  ROOT_DIR="$(resolve_root_dir)"
  if [[ "$DEPLOY_EXISTING" == '1' ]]; then
    local runner_path="$SCRIPT_DIR/$(basename "${BASH_SOURCE[0]}")"
    if [[ "$(sha256_file "$runner_path")" != "$EXPECTED_RUNNER_SHA" ]]; then
      printf 'error: deployment runner SHA-256 does not match EXPECTED_RUNNER_SHA\n' >&2
      exit 1
    fi
  fi

  if [[ ! "$REMOTE_PORT" =~ ^[0-9]+$ ]]; then
    printf 'error: RSCLAW_REMOTE_PORT must be an integer from 1 to 65535\n' >&2
    exit 2
  fi
  REMOTE_PORT=$((10#$REMOTE_PORT))
  if (( REMOTE_PORT < 1 || REMOTE_PORT > 65535 )); then
    printf 'error: RSCLAW_REMOTE_PORT must be an integer from 1 to 65535\n' >&2
    exit 2
  fi

  local target="${TARGET:-}"
  if [[ -z "$target" && "$BUILD_ONLY" == '1' ]]; then
    target='x86_64-unknown-linux-musl'
    log "BUILD_ONLY without TARGET; defaulting to $target"
  fi
  case "$target" in
    ''|x86_64-unknown-linux-musl|aarch64-unknown-linux-musl) ;;
    *)
      printf 'error: TARGET must be a supported static Linux target, got %s\n' "$target" >&2
      exit 2
      ;;
  esac

  if [[ "$BUILD_ONLY" != '1' ]]; then
    require_cmd cls
    local detected_target
    detected_target="$(detect_remote_target)"
    if [[ -n "$target" && "$target" != "$detected_target" ]]; then
      printf 'error: TARGET %s does not match remote architecture target %s\n' \
        "$target" "$detected_target" >&2
      exit 2
    fi
    target="$detected_target"
  fi

  if [[ -z "${RSCLAW_BUILD_VERSION:-}" ]]; then
    RSCLAW_BUILD_VERSION="$(awk '/^version = / { version=$3; gsub(/^"|"$/, "", version); print version; exit }' "$ROOT_DIR/Cargo.toml")"
  fi
  if [[ ! "$RSCLAW_BUILD_VERSION" =~ ^[0-9A-Za-z][0-9A-Za-z.+-]{0,63}$ ]]; then
    printf 'error: RSCLAW_BUILD_VERSION contains unsupported characters or is too long\n' >&2
    exit 2
  fi
  if [[ -z "${RSCLAW_BUILD_DATE:-}" ]]; then
    RSCLAW_BUILD_DATE="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  fi
  export RSCLAW_BUILD_VERSION RSCLAW_BUILD_DATE
  export CARGO_INCREMENTAL=0

  trap cleanup_staging EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  local binary="$ROOT_DIR/target/$target/release/rsclaw"
  local binary_sha upload_binary='' sha=''
  freeze_source_tree
  log "immutable source tree: $FROZEN_SOURCE_TREE"

  if [[ "$DEPLOY_EXISTING" == '1' ]]; then
    assert_existing_artifact_source "$EXPECTED_SOURCE_TREE" "$FROZEN_SOURCE_TREE"
    [[ -x "$binary" ]] || {
      printf 'error: expected existing binary not found: %s\n' "$binary" >&2
      exit 1
    }
    binary_sha="$(sha256_file "$binary")"
    if [[ "$binary_sha" != "$EXPECTED_BINARY_SHA" ]]; then
      printf 'error: existing binary SHA-256 does not match EXPECTED_BINARY_SHA\n' >&2
      exit 1
    fi
    LOCAL_STAGING="$(mktemp -d "$ROOT_DIR/target/rsclaw-upload.XXXXXX")"
    upload_binary="$LOCAL_STAGING/rsclaw"
    cp "$binary" "$upload_binary"
    chmod 700 "$upload_binary"
    sha="$(sha256_file "$upload_binary")"
    if [[ "$sha" != "$EXPECTED_BINARY_SHA" ]]; then
      printf 'error: existing binary changed while freezing upload copy\n' >&2
      exit 1
    fi
    log "deploying existing artifact from source tree: $EXPECTED_SOURCE_TREE"
  else
    local frozen_source="$LOCAL_SOURCE_STAGING/source"
    log "building $target for RsClaw $RSCLAW_BUILD_VERSION"
    (cd "$frozen_source" && bash scripts/build.sh "$target")

    local private_binary="$LOCAL_SOURCE_STAGING/build-target/$target/release/rsclaw"
    local private_dist="$LOCAL_SOURCE_STAGING/build-dist"
    [[ -x "$private_binary" ]] || {
      printf 'error: expected private build output not found: %s\n' "$private_binary" >&2
      exit 1
    }
    binary_sha="$(sha256_file "$private_binary")"

    if [[ "$BUILD_ONLY" != '1' ]]; then
      LOCAL_STAGING="$(mktemp -d "$ROOT_DIR/target/rsclaw-upload.XXXXXX")"
      upload_binary="$LOCAL_STAGING/rsclaw"
      cp "$private_binary" "$upload_binary"
      chmod 700 "$upload_binary"
      sha="$(sha256_file "$upload_binary")"
      if [[ "$sha" != "$binary_sha" ]]; then
        printf 'error: private build artifact changed while freezing upload copy\n' >&2
        exit 1
      fi
    fi

    publish_local_artifacts "$private_binary" "$private_dist" "$target"
    if [[ "$(sha256_file "$binary")" != "$binary_sha" ]]; then
      printf 'error: published local binary does not match private build output\n' >&2
      exit 1
    fi
  fi

  rm -rf -- "$LOCAL_SOURCE_STAGING"
  LOCAL_SOURCE_STAGING=""
  log "binary: $binary"
  log "sha256: $binary_sha"
  log "artifact provenance: source_tree=${EXPECTED_SOURCE_TREE:-$FROZEN_SOURCE_TREE} binary_sha=$binary_sha"

  if [[ "$BUILD_ONLY" == '1' ]]; then
    trap - EXIT INT TERM
    log 'BUILD_ONLY=1; upload and remote E2E skipped'
    return 0
  fi
  create_remote_staging
  log 'uploading exactly one file: the rsclaw binary'
  cls upload -n "$NODE" "$upload_binary" "$REMOTE_UPLOAD"
  log 'verifying and running the uploaded binary in an isolated remote E2E'
  local smoke_output expected_marker
  smoke_output="$(cls run -n "$NODE" -t 300 "$(remote_smoke_command "$sha" "$REMOTE_UPLOAD" "$REMOTE_PORT" "$target" "$RSCLAW_BUILD_VERSION" "$REMOTE_STAGING_TOKEN")")"
  printf '%s\n' "$smoke_output"
  if [[ "$SKIP_REMOTE_E2E" == '1' ]]; then
    expected_marker='REMOTE_BINARY_OK'
  else
    expected_marker='REMOTE_E2E_OK'
  fi
  for evidence in \
    "REMOTE_TARGET=$target" \
    "REMOTE_SHA256=$sha" \
    "REMOTE_VERSION=rsclaw $RSCLAW_BUILD_VERSION" \
    "$expected_marker"; do
    if ! output_has_evidence "$smoke_output" "$evidence"; then
      printf 'error: remote command exited without %s evidence\n' "$evidence" >&2
      exit 1
    fi
  done
  REMOTE_STAGING_CREATED=0
  rm -rf -- "$LOCAL_STAGING"
  LOCAL_STAGING=""
  trap - EXIT INT TERM
  if [[ "$SKIP_REMOTE_E2E" == '1' ]]; then
    log 'remote binary verification completed; E2E skipped'
  else
    log 'remote binary verification and E2E completed'
  fi
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
