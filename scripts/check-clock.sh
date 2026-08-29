#!/usr/bin/env bash
# Enforce the rule stated at the top of crates/tailbench/src/clock.rs: nothing outside that
# module calls `Instant::now` or `tokio::time::sleep` directly.
#
#   scripts/check-clock.sh
#
# Why the rule exists: every read of the clock and every sleep in the harness
# goes through the `Clock` trait so the determinism spike can swap the impl for
# a virtual clock and replay a run without wall time. One direct `Instant::now`
# is enough to break that -- the swapped clock is still installed, the run still
# completes, and the one bypassing call silently reintroduces real time into
# what is supposed to be a deterministic replay. That failure is invisible in
# the output, so it has to be caught in the source.
#
# Using the abstraction is fine and is what this checks for the absence of a
# bypass, not the absence of timing: `clock.now()` and `clock.sleep_until(..)`
# (ready.rs, load_generator.rs, downstream.rs) are the intended
# call sites and are not matched here.
set -euo pipefail
cd "$(dirname "$0")/.."

# `tokio::time::sleep` matches `sleep_until` too -- both bypass the trait.
PATTERN='Instant::now|tokio::time::sleep'

# crates/tailbench/src/clock.rs is the one module allowed to do this, so it is excluded rather
# than special-cased in the regex.
#
# --include comes before the path: BSD grep stops parsing flags at the first
# non-flag argument, so `grep ... crates/ --include=*.rs` reads --include as a
# second path, silently scans everything, and finds nothing to complain about.
# That failure mode is a check that always passes, which is worse than no check.
# `crates/tailbench-abi/src/ready.rs` is exempt: it sleeps directly by design,
# because exporting the Clock trait to the program under test would have been
# the larger cost. Its poll runs before the first request and lands in no
# measurement. See that file's module docs.
offenders="$(grep -rnE --include='*.rs' "$PATTERN" crates/ \
  | grep -v '/tailbench/src/clock\.rs:' \
  | grep -v '/tailbench-abi/src/ready\.rs:' || true)"

if [ -n "$offenders" ]; then
  echo "check-clock: direct clock access outside crates/tailbench/src/clock.rs:" >&2
  echo "$offenders" | sed 's/^/  /' >&2
  echo >&2
  echo "Use the Clock trait instead: clock.now() / clock.sleep_until(deadline)." >&2
  echo "See the module docs in crates/tailbench/src/clock.rs for why." >&2
  exit 1
fi

echo "check-clock: OK -- all clock access goes through crates/tailbench/src/clock.rs"
