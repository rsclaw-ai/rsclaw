#!/usr/bin/env bash
# Manual smoke test for /watch:
#  1. Spin up an isolated gateway pointing at /tmp/rsclaw-watch-probe/
#  2. Append lines to /tmp/rsclaw-watch-probe/test.log
#  3. (you) open a chat channel and issue /watch /tmp/rsclaw-watch-probe/test.log
#  4. Observe that each appended line reaches chat within ~2s
set -euo pipefail

BASE=/tmp/rsclaw-watch-probe
mkdir -p "$BASE"
cat > "$BASE/rsclaw.json5" <<'EOF'
{
  gateway: { port: 28890 },
  channels: { cli: { enabled: true } }
}
EOF
: > "$BASE/test.log"

export RSCLAW_BASE_DIR="$BASE"
export RSCLAW_CONFIG_PATH="$BASE/rsclaw.json5"

echo "==> Start gateway: cargo run --bin rsclaw -- gateway run --log-level info"
echo "==> Then in another shell: /watch $BASE/test.log via your preferred channel"
echo "==> Append to $BASE/test.log to generate events."
echo "==> Press Ctrl-C to stop."
