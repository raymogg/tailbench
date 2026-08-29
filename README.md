# tailbench

An experimental environment for measuring and optimizing p99 latency of async
services under open-loop load.

**`crates/program/src/main.rs` is the only file an agent may edit.** Everything
else is measurement apparatus, and the crate boundary enforces it: `program`
depends on `tailbench-abi`, not on the harness, so reaching the scorer is a
compile error. See [What gets optimized](#what-gets-optimized).

## Quick start

```bash
cargo test
scripts/run.sh scenarios/smoke.toml          # 5 requests, checks the wiring
scripts/run.sh scenarios/fanout-bimodal.toml
```

The script starts all three processes (downstreams.rs, loadgen.rs and program.rs), runs one scenario, and shuts down.

Each run creates its own directory, `results/<UTC timestamp>-<scenario id>/`,
holding `requests.jsonl` (one record per request), `report.json`, and `run.json`
(config, seed, git SHA, environment). `--repeat N`
writes its replays into a single directory as `requests.0.jsonl`, `.1`, ...

To run the processes by hand — useful for `--verbose` on the program, which
logs each request as it routes, you must start three seperate binaries:

```bash
cargo run --release --bin downstreams -- \
  --config scenarios/fanout-bimodal.toml --socket /tmp/tb/downstreams.sock &

cargo run --release --bin program -- \
  --listen /tmp/tb/program.sock --downstreams /tmp/tb/downstreams.sock --verbose &

cargo run --release --bin loadgen -- run \
  --config scenarios/fanout-bimodal.toml --socket /tmp/tb/program.sock
```

Other subcommands:

```bash
loadgen run --repeat 5    # N runs; reports replay std. dev. of cvar_99 and p99
loadgen report --log results/<run>/requests.jsonl --config <scenario>
loadgen validate --config <scenario>   # measured quantiles vs the closed form
```

## Architecture

```
 loadgen               program               downstreams
┌─────────────────┐   ┌─────────────────┐   ┌─────────────────┐
│ timeline        │   │ fan out to      │   │ capacity+queue  │
│ dispatch loop   │─▶─│ required        │─▶─│ seeded latency  │
│ oracle, records │─◀─│ downstreams     │─◀─│ digest          │
└─────────────────┘   └─────────────────┘   └─────────────────┘
        │                      ▲
        ▼          crates/program/src/main.rs
  results/<run>/   (the only editable file)
```

- **Separate processes, pinned cores.** Sharing a runtime with the code under
  test would let changes to it move the measurement.

- **Open-loop load.** Arrivals are scheduled up front and dispatched regardless
  of program state; a closed-loop generator would slow down under overload and
  hide the tail. Latency is measured from *intended* dispatch.

- **Deterministic downstreams.** Each latency draw comes from
  `(seed, request_id, downstream_id, attempt)`, so it does not depend on what
  else is in flight.

- **Harness-side scoring.** `Ok` means on time, required calls made, digest
  correct. Everything else enters the latency population at `penalty_ms`, so
  failing cannot improve the tail.

- **`cvar_99` is the primary metric**, p99 reported alongside. p99 is flat in
  `penalty_ms` below 1% failures and equal to it above; CVaR responds
  throughout. Outcome rates always accompany both.

## What gets optimized

`crates/program/src/main.rs` is the code under test, and the only file open to
optimization. It receives a request, calls the downstreams that request
requires, folds their replies into a digest, and returns it before the deadline.
This repo includes a baseline sample implementation of program.rs

==

| Process | Files | Role |
|---|---|---|
| `program` | `crates/program/src/main.rs` | **The code under test. Edit this.** |
| `loadgen` | `crates/tailbench/src/bin/loadgen.rs`, `load_generator.rs`, `timeline.rs`, `oracle.rs`, `report.rs`, `loadgen_client.rs` | Schedules arrivals, scores outcomes |
| `downstreams` | `crates/tailbench/src/bin/downstreams.rs`, `downstream.rs` | Simulates dependencies with seeded latency |
| shared | `crates/tailbench-abi/` — `protocol.rs`, `wire.rs`, `call.rs`, `span.rs`, `digest.rs`, `ready.rs` | The program/harness contract (~150 lines) |
| harness-only | `config.rs`, `rng.rs`, `clock.rs`, `distributions.rs`, `record.rs` | Seeded draws and scoring. Not reachable from `program`. |

### The rules

Fixed, and not optimizable away:

- **Every downstream in `requires` needs at least one successful call.** The
  digest is folded from values obtainable only by actually calling them, so
  skipping work and fabricating an answer scores `Incorrect`, not `Ok`.
- **A reply after the deadline scores `Expired`**, correct or not. Past
  `intended_dispatch + budget_ms` the response has no value.
- **Failure cannot beat slowness.** Every scheduled request contributes to the
  latency population — anything not `Ok` enters at `penalty_ms`, which exceeds
  `budget_ms` by construction. A percentile over survivors only would be
  trivially gamed by dropping the slow ones.

Open, and the point of the exercise:

- **Fan-out strategy** — concurrent, sequential, staged, prioritized.
- **Retries and hedging.** `requires` is a *minimum*: extra calls are legal and
  call order is unconstrained, so a second attempt at a slow downstream is
  allowed. Each `attempt` draws a fresh latency.
- **Timeouts**, and what to do when one fires.

### The isolation boundary

`crates/program` depends on `tailbench-abi` and **not** on `tailbench`. That is
what makes the rules above enforceable rather than advisory:

```rust
use tailbench::oracle::Oracle;              // unresolved module or unlinked crate
use tailbench_abi::digest::call_digest;     // no `call_digest` in `digest`
```

The ABI carries the wire protocol, the framing, the downstream client, the
readiness handshake, and `fold_digest` — roughly 150 lines. It deliberately
excludes `call_digest`, `call_rng`, and `payload_nonce`: a program holding those
could compute every expected digest from the seed and skip the work entirely.

Two things the boundary cannot do, covered separately: a program could
reimplement the digest algorithm from scratch, or read the seed out of
`scenarios/*.toml`. `crates/tailbench/tests/isolation.rs` greps for the former;
`scripts/run.sh` launches the program from a scratch cwd for the latter, which
is what `docker/compose.yml` already achieves by not mounting `/scenarios`.

### Tracking variants

Each run records `program_sha256` — the hash of the program source that actually
ran. `git_sha` alone cannot tell two variants apart, because an agent iterating
on one file leaves a dirty tree, so every such run reports the same commit;
`git_dirty` flags exactly that case. `program_variant` records the branch.

One branch per variant, each differing from `master` in one file:

```bash
git switch -c variant/my-change
# edit crates/program/src/main.rs
scripts/run.sh scenarios/fanout-bimodal.toml
```

Then compare in `notebooks/compare_runs.ipynb`.

### Verifying a change

```bash
scripts/run.sh scenarios/fanout-bimodal.toml
```

`cvar_99` is the number to move; the outcome rates beneath it say whether the
improvement was real or bought by failing requests. A run that reports
`RUN FAILED` or any nonzero `incorrect` did not earn its latency.

## Analysing a run

```bash
python3 -m venv .venv
.venv/bin/pip install pandas numpy matplotlib jupyterlab ipykernel
.venv/bin/jupyter lab notebooks/analyse_run.ipynb
```

Two notebooks:

- **`analyse_run.ipynb`** — one run directory: outcome mix, latency distribution
  and tail, behaviour over time, and the per-class and per-downstream breakdowns
  that say *where* a tail comes from.
- **`compare_runs.ipynb`** — baseline vs candidate, which is where a change to
  `program.rs` is judged. Also sweeps a series of runs and checks a delta against
  replay noise.

## Scenario Config

```toml
[scenario]
id         = "fanout-bimodal-001"  # label, appears in run.json
seed       = 42                    # seeds arrivals, class mix, latency draws
duration_s = 20.0                  # length of the arrival timeline
warmup_s   = 2.0                   # leading window discarded before scoring

[load]
arrival       = "poisson"   # constant | poisson | bursty
rate_rps      = 300.0       # mean arrival rate
burstiness_cv = 1.4         # bursty only; coefficient of variation, must be > 1

[slo]
budget_ms  = 120.0          # per-request deadline
penalty_ms = 1200.0         # latency a failed request contributes; default 10x budget

[[request_class]]
name     = "cheap"
weight   = 0.9              # fraction of requests; all weights must sum to 1.0
requires = ["svc_a"]        # downstreams this class must call

[[request_class]]
name     = "expensive"
weight   = 0.1
requires = ["svc_a", "svc_b"]

[[downstream]]
id           = "svc_a"
distribution = { kind = "lognormal", median_ms = 8.0, sigma = 0.6 }
capacity     = 64           # concurrent calls before others queue
timeout_ms   = 250.0        # applies to queue + service time

[[downstream]]
id           = "svc_b"
distribution = { kind = "bimodal", fast_ms = 3.0, slow_ms = 180.0, p_slow = 0.02 }
capacity     = 16
timeout_ms   = 250.0
```

### The parameters worth understanding

**`budget_ms`** is a per-request deadline. A response arriving after
`intended_dispatch + budget_ms` results in `Expired`, exactly
as if it never arrived.

**`penalty_ms`** is the latency value a failed request contributes to the
percentile, defaulting to `10 × budget_ms`.
It must exceed `budget_ms`, or quitting would beat completing slowly.

**`weight` and `requires`** are what make requests heterogeneous. `requires` is a
*minimum* — a service may call a downstream more than once (retries, hedging)
without penalty, and call order is unconstrained. Both matter: fixing either
would forbid legitimate optimizations.

**`capacity`** is how many calls a downstream serves concurrently.

**`warmup_s`** requests are discarded on *intended* dispatch time, so the
boundary does not move with service behaviour.

### Latency distributions

| `kind` | Parameters | Notes |
|---|---|---|
| `constant` | `ms` | Useful for validation — the answer is known exactly |
| `uniform` | `min_ms`, `max_ms` | |
| `lognormal` | `median_ms`, `sigma` | The ordinary well-behaved dependency |
| `bimodal` | `fast_ms`, `slow_ms`, `p_slow` | Cache hit/miss; the important one |
| `pareto` | `scale_ms`, `alpha` | Heavy-tailed; `alpha > 1` required |
| `empirical` | `samples_ms` | Resampled from a trace |

`bimodal` is where mean and p99 diverge hardest. With `fast_ms = 3`,
`slow_ms = 180`, `p_slow = 0.02`, the mean is ~6.5ms and p99 is exactly 180ms —
a 28× gap. A service tuned on mean latency will barely notice it.

Invalid configs are rejected at load with the offending value named — unknown
fields, weights that do not sum to 1, `requires` naming an undeclared
downstream, out-of-domain distribution parameters.

## Measurement environment

Authoritative runs need a host that supports real CPU pinning: Linux with
cpusets, ideally `isolcpus` on the boot line and no hypervisor in the way.
over the 1ms threshold against a gate of 0.1%. That is the gate working.

Under Docker with cpusets applied:

```bash
cd docker && SCENARIO=fanout-bimodal.toml REPEAT=1 docker compose up
```
