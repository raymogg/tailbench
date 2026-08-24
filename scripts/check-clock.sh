#!/usr/bin/env bash
# every time read and every sleep goes through the Clock trait.
# The trait is worthless if call sites bypass it, so this is enforced, not
# documented.
set -euo pipefail
cd "$(dirname "$0")/.."

violations=$(grep -rn --include=*.rs \
  -e 'Instant::now' -e 'tokio::time::sleep' -e 'thread::sleep' \
  src/ \
  | grep -v '^src/clock.rs:' \
  || true)

if [ -n "$violations" ]; then
  echo "Direct time access outside src/clock.rs:" >&2
  echo "$violations" >&2
  exit 1
fi
echo "clock check: ok"
