"""Loading and plotting helpers for tailbench run directories.

The notebook is the interface; this module is the part worth testing and
reusing. Kept separate so a plot can be fixed without re-running a cell chain,
and so `scripts/`-level tooling can import the same loader the notebook uses.

Nothing here recomputes a metric the harness already reports. `report.json` is
the authority on cvar_99/p99 -- recomputing them in Python would create a second
implementation of the scoring rule that could silently disagree with the one
that actually gates runs. Percentiles derived here are for *slicing* (per-class,
per-window), and use the same nearest-rank convention as `src/report.rs`.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path

import numpy as np
import pandas as pd

# Outcome order is fixed so a stacked bar keeps its colours across runs -- an
# outcome that appears in one run and not another must not shift the palette.
OUTCOMES = ["ok", "expired", "incorrect", "error", "dropped", "never_served"]
OUTCOME_COLORS = {
    "ok": "#2a9d5c",
    "expired": "#d1495b",
    "incorrect": "#8b2f97",
    "error": "#e08a1e",
    "dropped": "#5b6472",
    "never_served": "#1d2129",
}


def percentile(values, q: float) -> float:
    """Nearest-rank, matching `percentile()` in src/report.rs.

    numpy's default is linear interpolation, which gives a different p99 on the
    same data. Slices computed here must be comparable to the headline numbers,
    so the convention has to match rather than merely be close.
    """
    v = np.sort(np.asarray(values, dtype=float))
    if v.size == 0:
        return float("nan")
    rank = max(1, int(np.ceil(round(q * v.size, 9))))
    return float(v[min(rank, v.size) - 1])


def cvar(values, q: float) -> float:
    """Mean of the worst ceil(n*(1-q)) values. Matches `cvar()` in src/report.rs."""
    v = np.sort(np.asarray(values, dtype=float))
    if v.size == 0:
        return float("nan")
    k = min(max(1, int(np.ceil(round((1.0 - q) * v.size, 9)))), v.size)
    return float(v[v.size - k:].mean())


@dataclass
class Run:
    """One run directory: manifest, harness report, and per-request records.

    A `--repeat N` directory holds N replays. `replay` selects one; the frame
    carries a `replay` column either way so replay-to-replay noise stays
    visible rather than being averaged into a single line.
    """

    path: Path
    manifest: dict
    report: dict
    df: pd.DataFrame

    @property
    def label(self) -> str:
        return self.path.name

    @property
    def scenario_id(self) -> str:
        return self.manifest.get("scenario_id", "?")

    @property
    def authoritative(self) -> bool:
        return self.manifest.get("environment") == "linux-pinned"

    @property
    def budget_ms(self) -> float:
        return float(self.manifest["budget_ms"])

    @property
    def penalty_ms(self) -> float:
        return float(self.manifest["penalty_ms"])

    @property
    def warmup_s(self) -> float:
        return float(self.manifest.get("warmup_s", 0.0))

    def scored(self) -> pd.Series:
        """The latency population the headline metrics are computed over.

        Post-warmup only, and every non-`ok` request enters at `penalty_ms`.
        This is the series to plot when the question is "what does the harness
        see"; `df.e2e_ms` is the series to plot when the question is "how long
        did the work actually take".
        """
        d = self.df[self.df.post_warmup]
        return d.scored_ms

    def spans(self) -> pd.DataFrame:
        """One row per downstream call, joined to its request's outcome.

        Explodes `spans[]`, so a request that fanned out to two downstreams
        becomes two rows. Empty (with correct columns) when no request made a
        call, so downstream plots degrade to blank rather than raising.
        """
        cols = [
            "request_id", "class", "post_warmup", "outcome", "replay",
            "downstream_id", "attempt", "queue_wait_ms", "service_ms",
            "total_ms", "call_outcome",
        ]
        # `class` is a Python keyword, so itertuples() would rename it to a
        # positional `_3`. Iterate the columns explicitly instead.
        src = self.df[["request_id", "class", "post_warmup", "outcome", "replay", "spans"]]
        rows = []
        for rid, cls, pw, outcome, replay, spans in src.itertuples(index=False, name=None):
            for s in spans:
                q = s["queue_wait_ns"] / 1e6
                sv = s["service_ns"] / 1e6
                rows.append((
                    rid, cls, pw, outcome, replay, s["downstream_id"],
                    s["attempt"], q, sv, q + sv, s["outcome"],
                ))
        return pd.DataFrame(rows, columns=cols)


def load_run(path, replay: int | None = None) -> Run:
    """Load a run directory.

    `replay` picks one replay out of a `--repeat N` directory; the default
    loads every replay into one frame, tagged by the `replay` column.
    """
    path = Path(path)
    if not path.is_dir():
        raise NotADirectoryError(f"not a run directory: {path}")

    logs = sorted(path.glob("requests*.jsonl"))
    if not logs:
        raise FileNotFoundError(
            f"no requests*.jsonl in {path} -- is this a run directory? "
            f"(found: {', '.join(p.name for p in sorted(path.iterdir())) or 'nothing'})"
        )

    def replay_of(p: Path) -> int:
        # "requests.jsonl" -> 0; "requests.2.jsonl" -> 2.
        parts = p.name.split(".")
        return int(parts[1]) if len(parts) == 3 else 0

    if replay is not None:
        logs = [p for p in logs if replay_of(p) == replay]
        if not logs:
            raise FileNotFoundError(f"no replay {replay} in {path}")

    def sidecar(stem: str, rep: int) -> dict:
        # A single run writes `run.json`; a repeat writes `run.0.json`.
        for name in (f"{stem}.{rep}.json", f"{stem}.json"):
            f = path / name
            if f.exists():
                return json.loads(f.read_text())
        return {}

    frames = []
    for p in logs:
        rep = replay_of(p)
        d = pd.read_json(p, lines=True)
        d["replay"] = rep
        frames.append(d)
    df = pd.concat(frames, ignore_index=True)

    first = replay_of(logs[0])
    manifest = sidecar("run", first)
    report = sidecar("report", first)
    if not manifest:
        raise FileNotFoundError(f"no run.json in {path}")

    df = _derive(df, manifest)
    return Run(path=path, manifest=manifest, report=report, df=df)


def _derive(df: pd.DataFrame, manifest: dict) -> pd.DataFrame:
    """Add the derived columns every plot wants, in ms and seconds."""
    penalty = float(manifest["penalty_ms"])
    warmup_ns = float(manifest.get("warmup_s", 0.0)) * 1e9

    df = df.copy()
    df["t_s"] = df.intended_dispatch_ns / 1e9
    df["e2e_ms"] = (df.completion_ns - df.intended_dispatch_ns) / 1e6

    # Mirrors `scored_latency_ms` in src/record.rs: anything not `ok` enters the
    # population at the penalty, including requests that never completed.
    df["scored_ms"] = np.where(df.outcome == "ok", df.e2e_ms, penalty)
    df["scored_ms"] = df.scored_ms.fillna(penalty)

    # Warmup is decided on *intended* dispatch, so the boundary cannot move
    # with service behaviour. Same rule as report::build.
    df["post_warmup"] = df.intended_dispatch_ns >= warmup_ns

    df["late_dispatch_ms"] = df.late_dispatch_ns / 1e6
    df["queue_ms"] = df.spans.apply(
        lambda ss: sum(s["queue_wait_ns"] for s in ss) / 1e6 if ss else 0.0
    )
    df["n_calls"] = df.spans.apply(len)
    df["deadline_ms"] = (df.deadline_ns - df.intended_dispatch_ns) / 1e6
    return df


def load_runs(results_dir="results", pattern="*") -> list[Run]:
    """Every run directory under `results/`, oldest first.

    Directory names are timestamp-prefixed, so sorting by name sorts by time.
    Directories that fail to load are skipped with a note rather than aborting
    the sweep -- one partial run should not hide the other twenty.
    """
    runs = []
    for p in sorted(Path(results_dir).glob(pattern)):
        if not p.is_dir():
            continue
        try:
            runs.append(load_run(p))
        except (FileNotFoundError, NotADirectoryError, ValueError) as e:
            print(f"skipped {p.name}: {e}")
    return runs


def compare(runs: list[Run]) -> pd.DataFrame:
    """One row per run, headline metrics side by side.

    Values come from each run's own `report.json`, so this table says exactly
    what the harness said. `authoritative` is carried because an unpinned
    number must never be quoted as a result.
    """
    rows = []
    for r in runs:
        rep = r.report
        rows.append({
            "run": r.label,
            "scenario": r.scenario_id,
            "n": rep.get("n_post_warmup"),
            "cvar_99": rep.get("cvar_99"),
            "p99": rep.get("p99"),
            "p50": rep.get("p50"),
            "mean": rep.get("mean"),
            "ok_%": 100 * rep.get("ok_rate", float("nan")),
            "expired_%": 100 * rep.get("expiry_rate", float("nan")),
            "incorrect_%": 100 * rep.get("incorrect_rate", float("nan")),
            "calls": rep.get("downstream_calls"),
            "timeouts": rep.get("downstream_timeouts"),
            "failed": rep.get("failed"),
            "authoritative": r.authoritative,
            "seed": r.manifest.get("seed"),
            "git_sha": (r.manifest.get("git_sha") or "")[:8],
        })
    return pd.DataFrame(rows).set_index("run")


# ---------------------------------------------------------------------------
# Plots
#
# Every plot takes an `ax` so the notebook can compose panels. Latency axes are
# log-scaled by default: the whole point of the benchmark is that the tail is
# orders of magnitude past the median, and a linear axis renders the body as a
# single spike against a penalty value 200x its size.
# ---------------------------------------------------------------------------

import matplotlib.pyplot as plt
from matplotlib.ticker import FuncFormatter


def _style(ax, title=None, xlabel=None, ylabel=None):
    if title:
        ax.set_title(title, fontsize=11, loc="left", pad=8)
    if xlabel:
        ax.set_xlabel(xlabel, fontsize=9)
    if ylabel:
        ax.set_ylabel(ylabel, fontsize=9)
    ax.grid(alpha=0.25, linewidth=0.6)
    ax.set_axisbelow(True)
    for s in ("top", "right"):
        ax.spines[s].set_visible(False)
    return ax


def _ms_fmt(ax, axis="y"):
    f = FuncFormatter(lambda v, _: f"{v:g}")
    (ax.yaxis if axis == "y" else ax.xaxis).set_major_formatter(f)


def plot_outcomes(run: Run, ax=None, post_warmup=True):
    """Outcome mix as a single stacked bar, annotated with rates.

    First plot to read: a tail number is uninterpretable without it, because a
    service can post an excellent p99 by failing the slow 0.9% of requests.
    """
    ax = ax or plt.gca()
    d = run.df[run.df.post_warmup] if post_warmup else run.df
    counts = d.outcome.value_counts()
    total = max(len(d), 1)

    left = 0.0
    for o in OUTCOMES:
        c = int(counts.get(o, 0))
        if not c:
            continue
        pct = 100 * c / total
        ax.barh(0, pct, left=left, color=OUTCOME_COLORS[o], height=0.5,
                label=f"{o} {pct:.3f}% ({c:,})")
        # Only label a segment wide enough to hold text.
        if pct > 6:
            ax.text(left + pct / 2, 0, f"{o}\n{pct:.2f}%", ha="center",
                    va="center", color="white", fontsize=9, fontweight="bold")
        left += pct

    ax.set_xlim(0, 100)
    ax.set_ylim(-0.5, 0.6)
    ax.set_yticks([])
    # Above the bar: below it the legend landed on the next row's title.
    ax.legend(loc="lower center", bbox_to_anchor=(0.5, 0.62), ncol=2, fontsize=8,
              frameon=False)
    _style(ax, f"Outcome mix — {run.label}", "% of post-warmup requests")
    ax.grid(False)
    return ax


def plot_latency_dist(run: Run, ax=None, bins=120):
    """Scored-latency histogram with the metric landmarks drawn on.

    Log-x. The penalty pile at the right edge is not an outlier -- it is every
    failed request entering the population at `penalty_ms`, which is what stops
    failure from beating slowness. Seeing it next to the body is the point.
    """
    ax = ax or plt.gca()
    s = run.scored().to_numpy()
    s = s[s > 0]
    edges = np.logspace(np.log10(max(s.min(), 1e-3)), np.log10(s.max() * 1.05), bins)
    ax.hist(s, bins=edges, color="#4c78a8", alpha=0.85, edgecolor="none")
    ax.set_xscale("log")

    rep = run.report
    marks = [
        (rep.get("p50"), "#2a9d5c", "p50"),
        (rep.get("p99"), "#e08a1e", "p99"),
        (rep.get("cvar_99"), "#d1495b", "cvar_99"),
        (run.budget_ms, "#1d2129", "budget"),
    ]
    # Landmarks can sit close together (budget vs p99 especially), so stagger
    # the label heights rather than letting the text overplot.
    top = ax.get_ylim()[1]
    for i, (v, c, name) in enumerate(marks):
        if v and np.isfinite(v):
            ax.axvline(v, color=c, linestyle="--", linewidth=1.4)
            ax.text(v, top * (0.97 - 0.11 * (i % 3)), f" {name} {v:.1f}ms", color=c,
                    fontsize=7.5, va="top", fontweight="bold")

    _ms_fmt(ax, "x")
    return _style(ax, "Scored latency distribution (log scale)", "latency (ms)", "requests")


def plot_tail_cdf(run: Run, ax=None, floor=0.9):
    """Tail CDF from `floor` to 1.0, on a log-scaled survival axis.

    The tail is the deliverable, and a plain CDF spends 99% of its ink on the
    body. Plotting 1-F on a log axis gives every decade of rarity equal room, so
    p99 and p99.9 are both legible on one chart.
    """
    ax = ax or plt.gca()
    s = np.sort(run.scored().to_numpy())
    n = s.size
    if n == 0:
        return _style(ax, "Tail CDF — no data")
    q = np.arange(1, n + 1) / n
    keep = q >= floor
    # The slowest sample sits at q == 1.0, so 1-q is exactly 0 and cannot be
    # log-scaled. Drop that one point rather than the whole plot -- a short run
    # (the smoke scenario is 5 requests) is otherwise entirely zeros here.
    surv = 1 - q[keep]
    xs, ys = s[keep][surv > 0], surv[surv > 0]
    if ys.size == 0:
        return _style(ax, f"Tail CDF — too few requests (n={n})", "latency (ms)",
                      "fraction slower")
    ax.plot(xs, ys, color="#d1495b", linewidth=1.8)
    ax.set_xscale("log")
    ax.set_yscale("log")
    ax.axvline(run.budget_ms, color="#1d2129", linestyle="--", linewidth=1.2)
    ax.text(run.budget_ms, ax.get_ylim()[1], " budget", fontsize=8, va="top")
    for qq, lbl in [(0.01, "p99"), (0.001, "p99.9")]:
        ax.axhline(qq, color="#999", linestyle=":", linewidth=1)
        ax.text(ax.get_xlim()[0], qq, f" {lbl}", fontsize=8, va="bottom", color="#666")
    _ms_fmt(ax, "x")
    return _style(ax, f"Tail CDF (worst {100*(1-floor):.0f}%)", "latency (ms)",
                  "fraction slower")


def plot_latency_over_time(run: Run, ax=None, window_s=1.0, quantiles=(0.5, 0.99)):
    """Rolling percentiles of scored latency, with expiries marked underneath.

    Distinguishes a steady tail from a degrading one. A rising p99 across the
    run means a queue is building; a flat p99 with periodic spikes means the
    tail is arrival-driven. Those imply different fixes, and the aggregate
    number cannot tell them apart.
    """
    ax = ax or plt.gca()
    d = run.df[run.df.post_warmup]
    if d.empty:
        return _style(ax, "Latency over time — no data")

    bucket = (d.t_s // window_s) * window_s
    colors = {0.5: "#2a9d5c", 0.9: "#4c78a8", 0.99: "#d1495b", 0.999: "#8b2f97"}
    for q in quantiles:
        series = d.groupby(bucket).scored_ms.apply(lambda v, q=q: percentile(v, q))
        ax.plot(series.index, series.values, linewidth=1.6,
                color=colors.get(q, "#666"), label=f"p{q*100:g}")

    ax.axhline(run.budget_ms, color="#1d2129", linestyle="--", linewidth=1.2,
               label=f"budget {run.budget_ms:g}ms")
    ax.set_yscale("log")

    # Expiries on a twin axis: the tail's cause, at the moment it happened.
    exp = d[d.outcome != "ok"].groupby(bucket).size()
    if not exp.empty:
        ax2 = ax.twinx()
        ax2.bar(exp.index, exp.values, width=window_s * 0.9, color="#d1495b",
                alpha=0.18, zorder=0)
        ax2.set_ylabel("non-ok / window", fontsize=8, color="#d1495b")
        ax2.tick_params(axis="y", labelsize=7, colors="#d1495b")
        ax2.spines["top"].set_visible(False)
        ax2.set_ylim(bottom=0)

    ax.legend(fontsize=8, frameon=False, loc="upper left")
    _ms_fmt(ax, "y")
    return _style(ax, f"Latency over time ({window_s:g}s windows)", "run time (s)",
                  "scored latency (ms)")


def plot_throughput(run: Run, ax=None, window_s=1.0):
    """Offered load against completed-ok rate.

    A gap between the two is the shape of overload: requests arriving that the
    program is not turning into successes.
    """
    ax = ax or plt.gca()
    d = run.df[run.df.post_warmup]
    if d.empty:
        return _style(ax, "Throughput — no data")
    bucket = (d.t_s // window_s) * window_s
    offered = d.groupby(bucket).size() / window_s
    served = d[d.outcome == "ok"].groupby(bucket).size().reindex(offered.index,
                                                                fill_value=0) / window_s
    # A healthy run has offered ~= ok, so the two lines coincide. Draw offered
    # thick underneath and ok thin on top, otherwise one vanishes entirely and
    # the chart looks like it only plotted a single series.
    ax.plot(offered.index, offered.values, color="#4c78a8", linewidth=3.2,
            alpha=0.45, label="offered")
    ax.plot(served.index, served.values, color="#2a9d5c", linewidth=1.4, label="ok")
    ax.fill_between(offered.index, served.values, offered.values,
                    where=offered.values > served.values, color="#d1495b",
                    alpha=0.2, label="shortfall")
    ax.legend(fontsize=8, frameon=False)
    ax.set_ylim(bottom=0)
    return _style(ax, f"Throughput ({window_s:g}s windows)", "run time (s)", "rps")


def plot_by_class(run: Run, ax=None):
    """Per-class tail. Classes differ in `requires`, so they differ in exposure.

    An aggregate p99 blends a 90%-weighted cheap class with a 10% expensive one;
    if the expensive class is the entire tail, the aggregate hides where the
    problem is and which fan-out to fix.
    """
    ax = ax or plt.gca()
    d = run.df[run.df.post_warmup]
    classes = sorted(d["class"].unique())
    x = np.arange(len(classes))
    width = 0.26
    for i, (q, label, color) in enumerate(
        [(0.5, "p50", "#2a9d5c"), (0.99, "p99", "#e08a1e"), (None, "cvar_99", "#d1495b")]
    ):
        vals = []
        for c in classes:
            v = d[d["class"] == c].scored_ms
            vals.append(cvar(v, 0.99) if q is None else percentile(v, q))
        bars = ax.bar(x + (i - 1) * width, vals, width, label=label, color=color)
        ax.bar_label(bars, fmt="%.0f", fontsize=7, padding=2)
    # Headroom so bar_label text does not run into the top spine on a log axis.
    ax.margins(y=0.18)

    ax.set_xticks(x)
    ax.set_xticklabels([f"{c}\n(n={int((d['class']==c).sum()):,})" for c in classes],
                       fontsize=9)
    ax.axhline(run.budget_ms, color="#1d2129", linestyle="--", linewidth=1.2)
    ax.set_yscale("log")
    ax.legend(fontsize=8, frameon=False)
    _ms_fmt(ax, "y")
    return _style(ax, "Latency by request class", None, "ms (log)")


def plot_downstreams(run: Run, ax=None):
    """Per-downstream queue wait vs service time.

    The architectural diagnostic. Service time is dictated by the scenario and
    cannot be optimized; queue wait is the program's own doing -- it is what
    concurrency limits, retries, and hedging move. A tall queue bar is a
    self-inflicted bottleneck, and it is invisible in the end-to-end number.
    """
    ax = ax or plt.gca()
    sp = run.spans()
    sp = sp[sp.post_warmup]
    if sp.empty:
        return _style(ax, "Downstreams — no calls recorded")

    ds = sorted(sp.downstream_id.unique())
    x = np.arange(len(ds))
    q_med = [percentile(sp[sp.downstream_id == d].queue_wait_ms, 0.5) for d in ds]
    q_p99 = [percentile(sp[sp.downstream_id == d].queue_wait_ms, 0.99) for d in ds]
    s_med = [percentile(sp[sp.downstream_id == d].service_ms, 0.5) for d in ds]
    s_p99 = [percentile(sp[sp.downstream_id == d].service_ms, 0.99) for d in ds]

    w = 0.2
    for i, (vals, label, color) in enumerate([
        (s_med, "service p50", "#4c78a8"), (s_p99, "service p99", "#1f4e79"),
        (q_med, "queue p50", "#f2a54a"), (q_p99, "queue p99", "#d1495b"),
    ]):
        bars = ax.bar(x + (i - 1.5) * w, vals, w, label=label, color=color)
        ax.bar_label(bars, fmt="%.1f", fontsize=6.5, padding=1)
    ax.margins(y=0.20)

    ax.set_xticks(x)
    ax.set_xticklabels(
        [f"{d}\n({int((sp.downstream_id == d).sum()):,} calls)" for d in ds], fontsize=9)
    ax.set_yscale("symlog", linthresh=0.1)
    ax.legend(fontsize=8, frameon=False, ncol=2)
    return _style(ax, "Downstream service vs queue wait", None, "ms (symlog)")


def plot_dispatch_health(run: Run, ax=None):
    """Late dispatch over time — whether the *measurement* held up.

    Not a property of the program. If the generator dispatched late, some of the
    measured latency is the harness's own lag, and the run's tail is optimistic.
    This is the plot that says whether the other plots can be believed.
    """
    ax = ax or plt.gca()
    d = run.df[run.df.post_warmup]
    ax.scatter(d.t_s, d.late_dispatch_ms, s=2, alpha=0.3, color="#8b2f97",
               edgecolors="none")
    ax.axhline(1.0, color="#d1495b", linestyle="--", linewidth=1.2,
               label="1ms threshold")
    late = int((d.late_dispatch_ms > 1.0).sum())
    pct = 100 * late / max(len(d), 1)
    ax.legend(fontsize=8, frameon=False)
    ax.set_yscale("symlog", linthresh=0.01)
    return _style(ax, f"Dispatch lateness — {late:,} over 1ms ({pct:.3f}%, budget 0.1%)",
                  "run time (s)", "late by (ms)")


def overview(run: Run, figsize=(15, 16)):
    """The whole run on one page, in reading order.

    Outcomes first (is the tail even interpretable), then the distribution and
    its tail, then time, then the per-class and per-downstream breakdowns that
    say *where* the tail comes from, then dispatch health as the caveat.
    """
    fig, axes = plt.subplots(4, 2, figsize=figsize)
    fig.subplots_adjust(hspace=0.55, wspace=0.22)

    plot_outcomes(run, axes[0, 0])
    plot_latency_dist(run, axes[0, 1])
    plot_tail_cdf(run, axes[1, 0])
    plot_latency_over_time(run, axes[1, 1])
    plot_throughput(run, axes[2, 0])
    plot_by_class(run, axes[2, 1])
    plot_downstreams(run, axes[3, 0])
    plot_dispatch_health(run, axes[3, 1])

    rep = run.report
    flag = "" if run.authoritative else "   ·   NON-AUTHORITATIVE (unpinned)"
    failed = f"   ·   FAILED: {rep.get('failure_reason')}" if rep.get("failed") else ""
    fig.suptitle(
        f"{run.label}   ·   {run.scenario_id}   ·   seed {run.manifest.get('seed')}\n"
        f"cvar_99 {rep.get('cvar_99', float('nan')):.2f} ms   ·   "
        f"p99 {rep.get('p99', float('nan')):.2f} ms   ·   "
        f"ok {100*rep.get('ok_rate', float('nan')):.3f}%{flag}{failed}",
        fontsize=12, y=0.995, ha="center",
    )
    return fig


def plot_compare_tails(runs: list[Run], ax=None, floor=0.9):
    """Tail CDFs of several runs on one axis — the A/B view.

    This is the chart that answers "did the change help". Curves that separate
    only in the far tail are the interesting case: it means the change moved
    what p99 alone would have called noise.
    """
    ax = ax or plt.gca()
    cmap = plt.get_cmap("tab10")
    for i, r in enumerate(runs):
        s = np.sort(r.scored().to_numpy())
        if s.size == 0:
            continue
        q = np.arange(1, s.size + 1) / s.size
        keep = q >= floor
        # See plot_tail_cdf: the q == 1.0 point is exactly 0 survival.
        surv = 1 - q[keep]
        xs, ys = s[keep][surv > 0], surv[surv > 0]
        if ys.size == 0:
            continue
        ax.plot(xs, ys, linewidth=1.8, color=cmap(i % 10),
                label=f"{r.label}  (cvar_99 {r.report.get('cvar_99', float('nan')):.0f})")
    ax.set_xscale("log")
    ax.set_yscale("log")
    if runs:
        ax.axvline(runs[0].budget_ms, color="#1d2129", linestyle="--", linewidth=1.2)
    ax.legend(fontsize=8, frameon=False)
    _ms_fmt(ax, "x")
    return _style(ax, "Tail comparison", "latency (ms)", "fraction slower")


def plot_metric_bars(runs: list[Run], ax=None, metrics=("p50", "p99", "cvar_99")):
    """Headline metrics across runs, grouped by metric.

    Deliberately puts p99 and cvar_99 side by side: when an architectural change
    moves cvar_99 far more than p99, the two bars diverging *is* the finding.
    """
    ax = ax or plt.gca()
    x = np.arange(len(metrics))
    w = 0.8 / max(len(runs), 1)
    cmap = plt.get_cmap("tab10")
    for i, r in enumerate(runs):
        vals = [r.report.get(m, float("nan")) for m in metrics]
        bars = ax.bar(x + i * w - 0.4 + w / 2, vals, w, label=r.label,
                      color=cmap(i % 10))
        ax.bar_label(bars, fmt="%.1f", fontsize=7, padding=2)
    ax.margins(y=0.18)
    ax.set_xticks(x)
    ax.set_xticklabels(metrics, fontsize=9)
    ax.set_yscale("log")
    ax.legend(fontsize=7, frameon=False)
    _ms_fmt(ax, "y")
    return _style(ax, "Headline metrics", None, "ms (log)")


def delta_table(baseline: Run, candidate: Run) -> pd.DataFrame:
    """Candidate against baseline, with the sign convention spelled out.

    `better` is not simply "went down": a drop in `ok_%` is a regression even
    though the number fell, and cvar_99 improving while ok_% falls is the
    signature of a program that bought its tail by failing requests. The column
    encodes direction per metric so that case reads as mixed, not as a win.
    """
    lower_is_better = {
        "cvar_99": True, "p99": True, "p50": True, "p90": True, "mean": True,
        "max": True, "expiry_rate": True, "incorrect_rate": True,
        "error_rate": True, "downstream_timeouts": True,
        "ok_rate": False, "throughput_rps": False,
    }
    rows = []
    for m, lower in lower_is_better.items():
        b = baseline.report.get(m)
        c = candidate.report.get(m)
        if b is None or c is None:
            continue
        d = c - b
        pct = (d / b * 100) if b else float("nan")
        if abs(d) < 1e-12:
            verdict = "—"
        else:
            verdict = "better" if ((d < 0) == lower) else "WORSE"
        rows.append({"metric": m, "baseline": b, "candidate": c,
                     "delta": d, "delta_%": pct, "direction": verdict})
    return pd.DataFrame(rows).set_index("metric")
