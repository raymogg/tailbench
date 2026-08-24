# tailbench

An experimental environment for measuring and optimizing p99 latency of async
services under open-loop load.

## Quick start

```bash
cargo test
```

A run needs two processes. Start the mock downstream cluster:

```bash
cargo run --release --bin mocks -- \
  --config scenarios/fanout-bimodal.toml \
  --socket /tmp/tailbench/mocks.sock
```

Then drive load against it:

```bash
cargo run --release --bin loadgen -- run \
  --config scenarios/fanout-bimodal.toml \
  --socket /tmp/tailbench/mocks.sock
```

Results land in `results/`: `requests.jsonl` (one record per request),
`report.json`, and `run.json` (config, seed, git SHA, environment).

Useful flags:

```bash
--repeat 5     # run N times, report replay std. dev. of cvar_99 and p99
--out DIR      # default: results/
```

Two other subcommands:

```bash
# Re-aggregate an existing log without re-running.
cargo run --release --bin loadgen -- report \
  --log results/requests.jsonl --config scenarios/fanout-bimodal.toml

# Check measured quantiles against the closed form. No mocks process needed.
cargo run --release --bin loadgen -- validate \
  --config scenarios/validate-lognormal.toml
```

Under Docker, with pinned cores:

```bash
cd docker && SCENARIO=fanout-bimodal.toml docker compose up
```

## Architecture

```
  loadgen (2 cores)          mocks (4 cores)
 ┌──────────────────┐       ┌──────────────────┐
 │ timeline         │       │ per-downstream   │
 │ dispatch loop    │──UDS─▶│ capacity + queue │
 │ oracle + records │◀──────│ seeded latency   │
 └──────────────────┘       └──────────────────┘
         │
         ▼
   results/*.jsonl
```

Separate processes on disjoint pinned cores. If the generator's timers shared a
tokio runtime with the code being measured, changing that code would change the
measurement environment — a bias correlated with the thing under study, which no
amount of averaging removes. There is only one run mode for the same reason: a
second one would produce numbers that are not comparable to the first.

### Load generation is open-loop

The arrival schedule is computed before the run starts and dispatched on a fixed
timeline regardless of service state. A closed-loop generator issues request N+1
only after N completes, so an overloaded service automatically receives less
load and its tail looks healthy — which would make every measurement here
meaningless. Latency is measured from *intended* dispatch, so generator lag
shows up in the numbers instead of hiding.

### Determinism

Every downstream latency draw comes from an RNG derived from
`(seed, request_id, downstream_id, attempt)`. A request's latency is therefore
the same number regardless of what else is in flight, and a service that retries
cannot shift the sequence for every other call.

### What counts as success

A request is `Ok` only if it completes before its deadline, made every call its
class requires, and returned the digest the oracle expects. Everything else —
`Expired`, `Incorrect`, `Error`, `Dropped`, `NeverServed` — is a failure and
enters the latency population at `penalty_ms`, so failing cannot improve the
tail. Outcome rates are always reported alongside; a p99 without them is
uninterpretable.

`cvar_99` (mean of the worst 1%) is the primary metric, with p99 reported for
interpretability. p99 is a single order statistic and is flat with respect to
the penalty below 1% failures, then equal to it above — CVaR responds
proportionally throughout.

### Scenarios

One TOML file fully determines a run. `[[request_class]]` sets what fraction of
requests need which downstreams; `[[downstream]]` sets each service's latency
distribution, capacity, and timeout. See `scenarios/` for two worked examples.

## Layout

```
src/
├── bin/loadgen.rs    # generator process + CLI
├── bin/mocks.rs      # downstream cluster process
├── clock.rs          # Clock trait; the only place time is read
├── config.rs         # scenario TOML + validation
├── dist.rs           # latency distributions, with closed-form quantiles
├── rng.rs            # per-call-site RNG derivation
├── timeline.rs       # precomputed arrival schedule
├── downstream.rs     # mock cluster + UDS client
├── target.rs         # the interface under measurement
├── load_generator.rs # open-loop dispatch loop
├── oracle.rs         # deadlines, outcomes, expected digest
├── record.rs         # per-request record types
└── report.rs         # percentiles, CVaR, outcome rates
```

`scripts/check-clock.sh` fails the build on any direct `Instant::now` or `sleep`
outside `clock.rs`.

## Measurement environment

Authoritative runs need a Linux host with real cpusets. macOS has no CPU
affinity mechanism on Apple Silicon, and Docker there pins only to VM vCPUs the
host scheduler still migrates freely.

Runs detect this and stamp `environment` in `run.json`; anything not
`linux-pinned` is reported as non-authoritative. Locally you will see the
coordinated-omission gate fail runs — macOS timer granularity puts ~7% of
dispatches over the 1ms threshold, against a gate of 0.1%. That is the gate
working, not a bug.
