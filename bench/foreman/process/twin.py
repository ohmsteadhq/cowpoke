#!/usr/bin/env python3
"""Pump digital twin that never stops learning.

The twin predicts what a healthy pump should read at the speed it is running: differential
pressure, motor current and DC bus voltage. It learns those relations from the pump's own data
with recursive least squares, one sample at a time, forever. What it can't explain is the residual,
and residuals are what Foreman reads to name a fault.

Physics gives each model its shape, so a handful of numbers describe a pump:

  ΔP  = a + b·r²                     pump curve meets system curve (affinity laws), r = f / f_base
  I²  = c + d·r² + e·r⁴              magnetizing current² + torque current², torque ∝ load
  Vdc = g + h·(I·Vout)/1000          bus droop with load

Learning is gated so the twin learns the pump, not its faults:
  - only steady running (drive in RUN, frequency still for a second),
  - only samples it already explains (all residuals inside `gate_z`), once warmed up,
  - not while a transmitter is frozen at one value, or a residual stays ~3x rougher than usual,
  - with forgetting (a half-life), so it follows slow normal change: seasons, product, wear.

Because the live twin absorbs slow change, a frozen copy from commissioning (the baseline) is
kept next to it. Live vs. baseline is degradation: a worn impeller shows up there, not in the
residuals. Fidelity is tracked as the rolling RMSE of each prediction, versioned in the state file.

  python3 twin.py learn  --state twin.json run1.csv run2.csv ...
  python3 twin.py watch  --state twin.json fault.csv --out residuals.csv
  python3 twin.py demo   --out out/twin       # pump_sim plays the real pump: learn, then faults
"""

import argparse
import csv
import json
import math
import os
import random
import time
from collections import deque

F_BASE = 60.0
FLAT_S = 600  # a transmitter unchanged this long while running is stuck (live readings move in the last digits)
ROUGH_GATE = 3.0  # hold when a residual stays this many times rougher than usual for ~60 s

MODELS = {
    # name: (unit, residual floor, features(row, r))
    "dp_bar": ("bar", 0.03, lambda row, r: [1.0, r * r]),
    "current_a": ("A", 0.05, lambda row, r: [1.0, r * r, r ** 4]),
    "dc_bus_v": ("V", 2.0, lambda row, r: [1.0, row["current_a"] * row["out_voltage_v"] / 1000]),
}


def target(name, row):
    if name == "dp_bar":
        return row["discharge_bar"] - row["suction_bar"]
    if name == "current_a":
        return row["current_a"] ** 2
    return row["dc_bus_v"]


def from_target(name, y):
    return math.sqrt(max(y, 0.0)) if name == "current_a" else y


class RLS:
    """Recursive least squares with exponential forgetting."""

    def __init__(self, n, lam, p0=1e4):
        self.n, self.lam = n, lam
        self.theta = [0.0] * n
        self.P = [[p0 if i == j else 0.0 for j in range(n)] for i in range(n)]

    def predict(self, x):
        return sum(t * v for t, v in zip(self.theta, x))

    def leverage(self, x):
        n = self.n
        return max(0.0, sum(x[i] * sum(self.P[i][j] * x[j] for j in range(n)) for i in range(n)))

    def update(self, x, y):
        n, P = self.n, self.P
        Px = [sum(P[i][j] * x[j] for j in range(n)) for i in range(n)]
        denom = self.lam + sum(x[i] * Px[i] for i in range(n))
        k = [v / denom for v in Px]
        err = y - self.predict(x)
        self.theta = [t + ki * err for t, ki in zip(self.theta, k)]
        self.P = [[(P[i][j] - k[i] * Px[j]) / self.lam for j in range(n)] for i in range(n)]

    def to_json(self):
        return {"theta": self.theta, "P": self.P, "lam": self.lam}

    @classmethod
    def from_json(cls, d):
        m = cls(len(d["theta"]), d["lam"])
        m.theta, m.P = d["theta"], d["P"]
        return m


class Twin:
    def __init__(self, half_life_samples=50_000, gate_z=4.0, warmup=400, learn_every=10):
        lam = 0.5 ** (1 / half_life_samples)
        probe = {"current_a": 0.0, "out_voltage_v": 0.0}
        self.models = {k: RLS(len(f(probe, 1.0)), lam) for k, (_, _, f) in MODELS.items()}
        self.var = {k: MODELS[k][1] ** 2 * 4 for k in MODELS}  # residual variance, starts loose
        self.mse = {k: None for k in MODELS}  # rolling squared error on samples before learning them
        self.learned = 0
        self.seen = 0
        self.gate_z, self.warmup, self.learn_every = gate_z, warmup, learn_every
        self.baseline = None  # frozen thetas after commissioning
        self.history = []  # fidelity versions
        self._f = deque(maxlen=10)
        self.trend = {k: 0.0 for k in MODELS}  # EWMA of z over ~60 s: slow creep the per-sample gate misses
        self.rough = {k: 1.0 for k in MODELS}  # EWMA of z² over ~5 s: ragged signals (cavitation)
        self.trend_gate = 1.0
        self.rough_ref = {k: 1.0 for k in MODELS}  # healthy spread, learned with the model
        self._last_z = {}
        self.rough_avg = {k: 1.0 for k in MODELS}  # ~60 s average of the roughness: hold at ROUGH_GATE
        self._held = {}  # signal -> (value, since t_s): a transmitter frozen at one value is stuck
        self.flat_ref = {}  # signal -> longest still stretch that moved again on its own, s

    # ---- one sample in, prediction and residuals out
    def step(self, row, learn=True):
        self._f.append(row["freq_hz"])
        running = row["vfd_state"] not in ("STOPPED", "FAULT") and row["current_a"] > 0 and row["freq_hz"] > 10
        # Steady enough that inertia doesn't matter: under ~1 Hz/s, so a PID's trimming still counts.
        steady = running and len(self._f) == self._f.maxlen and max(self._f) - min(self._f) < 1.0
        out = {"steady": steady, "learned": False}
        if not running:
            self._held = {}
        stuck = self.stuck(row) if running else []
        if not steady:
            self._last_z = {}
            return out
        r = row["freq_hz"] / F_BASE
        z = {}
        for k, (unit, floor, feat) in MODELS.items():
            x = feat(row, r)
            pred = from_target(k, self.models[k].predict(x))
            meas = from_target(k, target(k, row))
            res = meas - pred
            sd = max(math.sqrt(self.var[k]), floor, (0.0 if k == "dc_bus_v" else 0.04) * abs(pred))  # product moves load a few %
            out[f"{k}_pred"], out[f"{k}_res"] = round(pred, 3), round(res, 3)
            z[k] = res / (sd * math.sqrt(1 + self.models[k].leverage(x)))
            self.trend[k] += (z[k] - self.trend[k]) / 600
            w = self._last_z.setdefault(k, deque(maxlen=50))  # spread over the last 5 s
            w.append(z[k])
            mu = sum(w) / len(w)
            self.rough[k] = sum((v - mu) ** 2 for v in w) / len(w)
            out[f"{k}_roughvar"] = self.rough[k]
            out[f"{k}_z"] = round(z[k], 2)
            out[f"{k}_trend"] = round(self.trend[k], 2)
            out[f"{k}_rough"] = round(math.sqrt(self.rough[k] / max(self.rough_ref[k], 1e-6)), 2)
            if len(w) >= 25:
                self.rough_avg[k] += (out[f"{k}_rough"] - self.rough_avg[k]) / 600
        self.seen += 1
        warm = self.learned < self.warmup
        out["stuck"] = stuck
        explained = all(abs(v) < self.gate_z for v in z.values()) and \
            all(abs(v) < self.trend_gate for v in self.trend.values()) and \
            all(v < ROUGH_GATE for v in self.rough_avg.values()) and not stuck
        out["holding"] = not warm and not explained
        if learn and (self.seen % self.learn_every == 0) and (warm or explained):
            for k, (_, _, feat) in MODELS.items():
                x = feat(row, r)
                res = out[f"{k}_res"]
                if not warm:
                    self.mse[k] = res * res if self.mse[k] is None else 0.995 * self.mse[k] + 0.005 * res * res
                self.var[k] = 0.98 * self.var[k] + 0.02 * res * res
                if len(self._last_z.get(k, ())) >= 25:
                    self.rough_ref[k] = 0.98 * self.rough_ref[k] + 0.02 * self.rough[k]
                self.models[k].update(x, target(k, row))
            self.learned += 1
            out["learned"] = True
            if self.learned == self.warmup and self.baseline is None:
                self.freeze_baseline()
        return out

    def stuck(self, row):
        """Pressures that have read exactly one value for FLAT_S of running, and three times longer than
        they ever sat still before moving again."""
        out = []
        for k in ("discharge_bar", "suction_bar"):
            v, t = row[k], row["t_s"]
            limit = max(FLAT_S, 3 * self.flat_ref.get(k, 0.0))
            held = self._held.setdefault(k, (v, t))
            if held[0] != v:
                if t - held[1] < limit:
                    self.flat_ref[k] = max(self.flat_ref.get(k, 0.0), t - held[1])
                self._held[k] = (v, t)
            elif t - held[1] >= limit:
                out.append(k)
        return out

    def freeze_baseline(self):
        self.baseline = {k: list(m.theta) for k, m in self.models.items()}

    def fidelity(self):
        return {k: (None if v is None else round(math.sqrt(v), 4)) for k, v in self.mse.items()}

    def snapshot(self, note=""):
        self.history.append({"t": time.strftime("%Y-%m-%dT%H:%M:%S"), "learned": self.learned,
                             "rmse": self.fidelity(), "note": note})

    def drift(self, freq_hz):
        """Live vs. baseline prediction at one speed, per model (relative)."""
        if not self.baseline:
            return {}
        r = freq_hz / F_BASE
        row = {"current_a": 30, "out_voltage_v": 460 * r}
        d = {}
        for k, (_, _, feat) in MODELS.items():
            x = feat(row, r)
            live = from_target(k, sum(t * v for t, v in zip(self.models[k].theta, x)))
            base = from_target(k, sum(t * v for t, v in zip(self.baseline[k], x)))
            d[k] = round((live - base) / base, 4) if base else None
        return d

    def save(self, path):
        with open(path, "w") as fh:
            json.dump({"models": {k: m.to_json() for k, m in self.models.items()}, "var": self.var,
                       "mse": self.mse, "learned": self.learned, "seen": self.seen, "baseline": self.baseline,
                       "history": self.history, "gate_z": self.gate_z, "warmup": self.warmup,
                       "learn_every": self.learn_every}, fh, indent=1)

    @classmethod
    def load(cls, path):
        with open(path) as fh:
            d = json.load(fh)
        t = cls(gate_z=d["gate_z"], warmup=d["warmup"], learn_every=d["learn_every"])
        t.models = {k: RLS.from_json(v) for k, v in d["models"].items()}
        for k in ("var", "mse", "learned", "seen", "baseline", "history"):
            setattr(t, k, d[k])
        return t


def read_csv(path):
    with open(path) as fh:
        for row in csv.DictReader(fh):
            for k in ("t_s", "freq_hz", "out_voltage_v", "dc_bus_v", "current_a", "suction_bar", "discharge_bar"):
                row[k] = float(row[k])
            yield row


# --------------------------------------------------------------------------------------------
# Demo: pump_sim is the real pump. One machine; learn on healthy days, then watch faults.


def demo(out, seed):
    import pump_sim as sim

    os.makedirs(out, exist_ok=True)
    rng = random.Random(seed)
    plant = sim.build_run("normal", rng, randomize=True).plant  # one random machine, unknown to the twin
    twin = Twin(half_life_samples=20_000)

    # Commissioning and normal days: a few speeds and the pressure loop, all healthy.
    healthy = []
    for f_set in (42, 48, 54, 60):
        run = sim.build_run("normal", rng, plant=plant)
        run.plant.f_set, run.stop_at, run.duration = f_set, None, 240.0
        healthy.append(run)
    for _ in range(3):
        healthy.append(sim.build_run("normal_pid", rng, plant=plant))
    for run in healthy:
        for row in sim.simulate(run, rng):
            twin.step(row)
        twin.snapshot(f"healthy {run.scenario} f_set={run.plant.f_set:.0f}")
    print("fidelity after healthy runs (RMSE):", twin.fidelity(), "learned", twin.learned)

    # Faults on the same machine: the twin keeps learning (gated) while it watches.
    import copy
    report = []
    traces = []
    for name in sim.SCENARIOS:
        if name in ("normal", "impeller_wear"):
            continue
        run = sim.build_run(name, rng, plant=plant)
        t2 = copy.deepcopy(twin)
        rows = sim.simulate(run, rng)
        res = [(row, t2.step(row)) for row in rows]
        on = run.fault.onset
        pre = [o for row, o in res if o["steady"] and row["t_s"] < on]
        post = [o for row, o in res if o["steady"] and row["t_s"] >= on + 5]
        mean = lambda xs, k: round(sum(x[k] for x in xs) / len(xs), 1) if xs else None
        learned_post = sum(1 for o in post if o["learned"])
        report.append({"scenario": name, **{f"{k}_z_pre": mean(pre, f"{k}_z") for k in MODELS},
                       **{f"{k}_rough_post": mean(post, f"{k}_rough") for k in MODELS},
                       **{f"{k}_z_post": mean(post, f"{k}_z") for k in MODELS},
                       "post_samples": len(post), "learned_post": learned_post})
        traces.append((run, res))

    # Slow wear over weeks: the live twin follows, the baseline doesn't.
    wear = sim.build_run("impeller_wear", rng, plant=plant)
    tw = copy.deepcopy(twin)
    tw.learn_every = 1
    drift = []
    for row in sim.simulate(wear, rng):
        o = tw.step(row)
        if int(row["t_s"]) % 86400 == 0:
            drift.append({"day": int(row["t_s"] // 86400), **tw.drift(wear.plant.f_set), "z_dp": o.get("dp_bar_z")})

    twin.snapshot("after demo")
    twin.save(os.path.join(out, "twin.json"))
    with open(os.path.join(out, "fingerprints.json"), "w") as fh:
        json.dump({"fidelity": twin.fidelity(), "faults": report, "wear_drift": drift}, fh, indent=1)
    plot(traces, os.path.join(out, "residuals.png"))
    return twin, report, drift


def plot(traces, path):
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    n = len(traces)
    fig, axes = plt.subplots(n, 1, figsize=(11, 1.35 * n), squeeze=False, sharex=False)
    colors = {"dp_bar": "#15803d", "current_a": "#c2410c", "dc_bus_v": "#1d4ed8"}
    for i, (run, res) in enumerate(traces):
        ax = axes[i][0]
        t = [row["t_s"] for row, o in res if o["steady"]]
        for k, c in colors.items():
            ax.plot(t, [max(-15, min(15, o[f"{k}_z"])) for row, o in res if o["steady"]], ".", ms=1.2, color=c,
                    label={"dp_bar": "ΔP", "current_a": "current", "dc_bus_v": "DC bus"}[k])
        ax.axhspan(-4, 4, color="#999", alpha=0.12)
        if run.fault.onset < run.duration:
            ax.axvline(run.fault.onset, color="#dc2626", lw=0.7, ls="--")
        ax.set_ylim(-16, 16)
        ax.set_xlim(0, run.duration)
        ax.set_ylabel(run.scenario, rotation=0, ha="right", va="center", fontsize=8)
        ax.tick_params(labelsize=7)
        if i == 0:
            ax.legend(fontsize=7, ncol=3, loc="upper left", markerscale=6)
            ax.set_title("Twin residuals (z, clipped at ±15) while running steady · grey band = learning gate", fontsize=9)
    fig.tight_layout()
    fig.savefig(path, dpi=110)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    a = sub.add_parser("learn")
    a.add_argument("--state", required=True)
    a.add_argument("csv", nargs="+")
    w = sub.add_parser("watch")
    w.add_argument("--state", required=True)
    w.add_argument("--out")
    w.add_argument("--no-learn", action="store_true")
    w.add_argument("csv")
    d = sub.add_parser("demo")
    d.add_argument("--out", default="out/twin")
    d.add_argument("--seed", type=int, default=3)
    args = ap.parse_args()

    if args.cmd == "learn":
        twin = Twin.load(args.state) if os.path.exists(args.state) else Twin()
        for path in args.csv:
            for row in read_csv(path):
                twin.step(row)
            twin.snapshot(os.path.basename(path))
        twin.save(args.state)
        print("learned", twin.learned, "rmse", twin.fidelity())
    elif args.cmd == "watch":
        twin = Twin.load(args.state)
        rows = []
        for row in read_csv(args.csv):
            rows.append({"t_s": row["t_s"], **twin.step(row, learn=not args.no_learn)})
        if args.out:
            keys = sorted({k for r in rows for k in r})
            with open(args.out, "w", newline="") as fh:
                wr = csv.DictWriter(fh, fieldnames=keys)
                wr.writeheader()
                wr.writerows(rows)
        if not args.no_learn:
            twin.snapshot(f"watch {os.path.basename(args.csv)}")
            twin.save(args.state)
    else:
        twin, report, drift = demo(args.out, args.seed)
        print(f"\n{'scenario':16} {'ΔP z':>12} {'current z':>12} {'bus z':>12}  rough ΔP/I  learned/after-onset")
        for r in report:
            cell = lambda k: f"{r[k + '_z_pre']}→{r[k + '_z_post']}"
            print(f"{r['scenario']:16} {cell('dp_bar'):>12} {cell('current_a'):>12} {cell('dc_bus_v'):>12}  "
                  f"{r['dp_bar_rough_post']}/{r['current_a_rough_post']}  {r['learned_post']}/{r['post_samples']}")
        print("\nwear: live vs baseline at running speed")
        for d in drift[:: max(1, len(drift) // 8)]:
            print(d)


if __name__ == "__main__":
    main()
