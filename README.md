# tailbench

Tailbench is an benchmark environment for measuring p99 latency of a target program.

Compared to many other benchmarking environments that measure mean/median latency of single functions, tailbench aims to simulate async environments under load with varios simualted downstreams. In this way it can be thought of as a simple version of real distributed computing environments where you have a hotpath and various downstreams that you must interact with.

It provides a way for researchers to define
- load generation: produce requests at specific cadences
- downstream services: simulated services that respond with latencies drawn from various distributions (`constant`, `uniform`, `lognormal`, `bimodal`,  `pareto`)

**`crates/program/src/main.rs` is the only file an agent / research system may edit.**

See the [program rules](#program-rules) for how each program must behave within the environment.

See [scenario config](#scenario-config) for creating your own levels for testing. 5 pre-defined levels are provided as part of the repo.

## Quick start

```bash
cargo test
scripts/run.sh scenarios/smoke.toml
scripts/run.sh scenarios/level3.toml
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
  --config scenarios/level3.toml --socket /tmp/tb/downstreams.sock &

cargo run --release --bin program -- \
  --listen /tmp/tb/program.sock --downstreams /tmp/tb/downstreams.sock --verbose &

cargo run --release --bin loadgen -- run \
  --config scenarios/level3.toml --socket /tmp/tb/program.sock
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

## Program Rules

`crates/program/src/main.rs` is the code under test, and the only file open to
optimization. It receives a request, calls the downstreams that request
requires, folds their replies into a digest, and returns it before the deadline.
This repo includes a baseline sample implementation of program.rs

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

## Tracking variants

Each run records `program_sha256` — the hash of the program source that actually
ran. `git_sha` alone cannot tell two variants apart, because an agent iterating
on one file leaves a dirty tree, so every such run reports the same commit;
`git_dirty` flags exactly that case. `program_variant` records the branch.

One branch per variant, each differing from `master` in one file:

```bash
git switch -c variant/my-change
# edit crates/program/src/main.rs
scripts/run.sh scenarios/level3.toml
```

Then compare in `notebooks/compare_runs.ipynb`.

### Verifying a change

```bash
scripts/run.sh scenarios/level3.toml
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

## Pre-defined levels

Five scenarios of increasing difficulty, `scenarios/level1.toml` ... `level5.toml`.
Each is a different *architectural* problem rather than the same problem with the
dial turned up -- the point is to find out which kinds of reasoning an optimizer
can and cannot do.

| Level | The problem | What a naive program does | What the fix requires |
|---|---|---|---|
| 1 | One well-behaved dependency, 10x budget headroom | Passes, 100% ok | Nothing. Confirms the program isn't self-serialising |
| 2 | Fan-out of 3; budget sits between `max(mean)` and `sum(mean)` | Serial fan-out misses the SLO | Issue the calls concurrently |
| 3 | Bimodal straggler: 6% of calls take 150ms against a 60ms budget | ~3% expire, `cvar_99` pins to `penalty_ms` | Hedge. A retry draws fresh latency, so `p` becomes `p²` |
| 4 | Two tail shapes at once: bimodal *and* pareto | ~4.6% expire | Treat them differently -- retry the discrete one, time out the unbounded one |
| 5 | Same tails, but `svc_b` has capacity 6 against ~3.0 offered | ~5.5% expire | Be selective. A blanket hedge saturates `svc_b` and makes it *worse* |

The ladder is built on two properties of the harness:

- **Retries draw fresh latency.** `attempt` is part of the RNG key, so a second
  call to a straggler is an independent sample -- which is what makes hedging a
  real strategy rather than a repeat of the same bad draw.
- **Queue wait counts toward `timeout_ms`.** Capacity is a semaphore, so extra
  calls consume permits that first attempts need. This is the tension levels 4
  and 5 are built on, and why level 5 cannot be solved by hedging harder.

Levels 1-3 each have a single dominant right answer. Levels 4 and 5 do not: they
have a trade-off to locate, and level 5 punishes the policy that wins level 3.
That contrast is the actual experiment -- a system that has learned "hedge the
tail" as a rule will regress on level 5, while one reasoning about capacity
will not.

## Scenario Config

```toml
[scenario]
id         = "level3-straggler"     # label, appears in run.json
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

### Key Parameters

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

## Measurement environment

Authoritative runs need a host that supports real CPU pinning: Linux with
cpusets, ideally `isolcpus` on the boot line and no hypervisor in the way.
over the 1ms threshold against a gate of 0.1%. That is the gate working.

Under Docker with cpusets applied:

```bash
cd docker && SCENARIO=level3.toml REPEAT=1 docker compose up
```
