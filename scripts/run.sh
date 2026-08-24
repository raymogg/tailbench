#!/usr/bin/env bash
# Start mocks + service, run one scenario, shut down.
#
#   scripts/run.sh [scenario.toml] [extra loadgen flags...]
set -euo pipefail
cd "$(dirname "$0")/.."

SCENARIO="${1:-scenarios/fanout-bimodal.toml}"
shift || true
RUN_DIR="$(mktemp -d)"
cleanup() {
  # `kill` messages go to the shell's stderr, not the job's, so silence the
  # whole function rather than the command.
  { kill $(jobs -p) 2>/dev/null; wait; } 2>/dev/null || true
  rm -rf "$RUN_DIR"
}
trap cleanup EXIT

cargo build --release --quiet

./target/release/mocks --config "$SCENARIO" --socket "$RUN_DIR/mocks.sock" &
./target/release/service --listen "$RUN_DIR/service.sock" --mocks "$RUN_DIR/mocks.sock" &

./target/release/loadgen run \
  --config "$SCENARIO" \
  --socket "$RUN_DIR/service.sock" \
  "$@"
