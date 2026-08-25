#!/usr/bin/env bash
# Start downstreams + program, run one scenario, shut down.
#
#   scripts/run.sh <scenario.toml> [extra loadgen flags...]
#
# The scenario is required and has no default: a run that silently used the
# wrong config produces a plausible report that is not the experiment you
# meant, and you find out afterwards. Fail before starting instead.
set -euo pipefail
cd "$(dirname "$0")/.."

if [ $# -lt 1 ]; then
  echo "usage: scripts/run.sh <scenario.toml> [extra loadgen flags...]" >&2
  echo >&2
  echo "available scenarios:" >&2
  for f in scenarios/*.toml; do echo "  $f" >&2; done
  exit 2
fi

SCENARIO="$1"
shift

if [ ! -f "$SCENARIO" ]; then
  echo "scripts/run.sh: no such scenario: $SCENARIO" >&2
  exit 2
fi
RUN_DIR="$(mktemp -d)"
cleanup() {
  # `kill` messages go to the shell's stderr, not the job's, so silence the
  # whole function rather than the command.
  { kill $(jobs -p) 2>/dev/null; wait; } 2>/dev/null || true
  rm -rf "$RUN_DIR"
}
trap cleanup EXIT

cargo build --release --quiet

./target/release/downstreams --config "$SCENARIO" --socket "$RUN_DIR/downstreams.sock" &
./target/release/program --listen "$RUN_DIR/program.sock" --downstreams "$RUN_DIR/downstreams.sock" &

./target/release/loadgen run \
  --config "$SCENARIO" \
  --socket "$RUN_DIR/program.sock" \
  "$@"
