#!/usr/bin/env python3
"""Pictures of the synthetic runs and the twin watching them.

  python3 plots.py sheet  --out out/plots           # every scenario: measured vs. the twin's expected
  python3 plots.py grid   --out out/plots           # two random pumps per scenario: the dataset's variety
  python3 plots.py gif    --out out/plots --scenario leak_pid   # one run, animated, with the twin

The twin is commissioned first on healthy runs of the same pump (several speeds and the pressure
loop), as in `twin.py demo`, then watches each scenario with gated learning on.
"""

import argparse
import copy
import math
import os
import random

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402

import pump_sim as sim  # noqa: E402
from twin import Twin  # noqa: E402

INK = "#1f2937"
TWIN = "#2563eb"
HOLD = "#dc2626"
OK = "#16a34a"
MUTED = "#9ca3af"


def commission(seed):
    rng = random.Random(seed)
    plant = sim.build_run("normal", rng, randomize=True).plant
    twin = Twin(half_life_samples=20_000)
    runs = []
    for f_set in (42, 48, 54, 60):
        run = sim.build_run("normal", rng, plant=plant)
        run.plant.f_set, run.stop_at, run.duration = f_set, None, 240.0
        runs.append(run)
    runs += [sim.build_run("normal_pid", rng, plant=plant) for _ in range(3)]
    for run in runs:
        for row in sim.simulate(run, rng):
            twin.step(row)
    return plant, twin, rng


def watch(name, plant, twin, rng):
    """Run one scenario on the commissioned pump; the twin (a copy) watches and keeps learning."""
    run = sim.build_run(name, rng, plant=plant)
    tw = copy.deepcopy(twin)
    if run.dt >= 1.0:
        tw.learn_every = 1
    rows = sim.simulate(run, rng)
    outs = [tw.step(r) for r in rows]
    for o in outs:
        o["holding"] = o["steady"] and any(
            abs(o.get(f"{k}_z", 0)) >= tw.gate_z or abs(o.get(f"{k}_trend", 0)) >= tw.trend_gate
            for k in ("dp_bar", "current_a", "dc_bus_v"))
    return run, rows, outs


def series(rows, outs, key):
    nan = float("nan")
    meas = {"dp_bar": [r["discharge_bar"] - r["suction_bar"] for r in rows],
            "current_a": [r["current_a"] for r in rows],
            "dc_bus_v": [r["dc_bus_v"] for r in rows]}[key]
    pred = [o.get(f"{key}_pred", nan) if o["steady"] else nan for o in outs]
    band = []
    for o in outs:
        z, res = o.get(f"{key}_z"), o.get(f"{key}_res")
        band.append(abs(res / z) if o["steady"] and z else nan)
    return meas, pred, band


def spans(t, flags):
    """(start, end) of runs where flags is true."""
    out, start = [], None
    for ti, f in zip(t, flags):
        if f and start is None:
            start = ti
        if not f and start is not None:
            out.append((start, ti))
            start = None
    if start is not None:
        out.append((start, t[-1]))
    return out


def sheet(out, seed):
    plant, twin, rng = commission(seed)
    names = list(sim.SCENARIOS)
    fig, axes = plt.subplots(len(names), 3, figsize=(16, 2.0 * len(names)), squeeze=False)
    for i, name in enumerate(names):
        run, rows, outs = watch(name, plant, twin, rng)
        days = run.dt >= 1.0
        scale = 86400 if days else 1
        t = [r["t_s"] / scale for r in rows]
        hold = spans(t, [o["holding"] for o in outs])
        trip = next((r["t_s"] / scale for r in rows if r["vfd_state"] == "FAULT"), None)
        for j, (key, title, unit) in enumerate([("current_a", "Motor current", "A"),
                                                ("dp_bar", "Differential pressure", "bar"),
                                                ("dc_bus_v", "DC bus", "V")]):
            ax = axes[i][j]
            meas, pred, band = series(rows, outs, key)
            ax.plot(t, meas, color=INK, lw=0.7, label="measured")
            ax.plot(t, pred, color=TWIN, lw=1.1, ls="--", label="twin expected")
            lo = [p - 4 * b for p, b in zip(pred, band)]
            hi = [p + 4 * b for p, b in zip(pred, band)]
            ax.fill_between(t, lo, hi, color=TWIN, alpha=0.10, lw=0, label="±4σ learning gate")
            for a, b in hold:
                ax.axvspan(a, b, color=HOLD, alpha=0.08, lw=0)
            if run.fault.onset < run.duration:
                ax.axvline(run.fault.onset / scale, color=HOLD, lw=0.8, ls=":")
            if trip is not None:
                ax.axvline(trip, color=INK, lw=0.8)
            if key == "dc_bus_v":
                ax.set_ylim(max(300, min(meas) - 20), max(meas) + 20)
            ax.tick_params(labelsize=7)
            ax.grid(alpha=0.15)
            if i == 0:
                ax.set_title(f"{title} ({unit})", fontsize=10)
            if j == 0:
                ax.set_ylabel(name, fontsize=9, rotation=0, ha="right", va="center")
            if i == len(names) - 1 or days:
                ax.set_xlabel("days" if days else "s", fontsize=7)
        if i == 0:
            axes[0][1].legend(fontsize=7, loc="lower right")
    fig.suptitle("Synthetic pump runs on one machine, watched by its digital twin.  "
                 "Red dotted: fault starts · red shade: twin holding (won't learn) · black: drive trips",
                 fontsize=11, y=0.997)
    fig.tight_layout(rect=(0, 0, 1, 0.99))
    path = os.path.join(out, "all_runs_twin.png")
    fig.savefig(path, dpi=100)
    print("wrote", path)


def grid(out, seed):
    rng = random.Random(seed)
    names = list(sim.SCENARIOS)
    cols = 4
    fig, axes = plt.subplots(len(names), cols, figsize=(17, 1.55 * len(names)), squeeze=False)
    for i, name in enumerate(names):
        for j in range(cols):
            run = sim.build_run(name, rng, randomize=True)
            rows = sim.simulate(run, rng)
            days = run.dt >= 1.0
            scale = 86400 if days else 1
            t = [r["t_s"] / scale for r in rows]
            ax = axes[i][j]
            ax.plot(t, [r["current_a"] / run.plant.fla for r in rows], color="#c2410c", lw=0.6)
            ax2 = ax.twinx()
            ax2.plot(t, [r["discharge_bar"] for r in rows], color="#15803d", lw=0.6)
            ax.set_ylim(0, 1.6)
            if run.fault.onset < run.duration:
                ax.axvline(run.fault.onset / scale, color=HOLD, lw=0.7, ls=":")
            trip = next((r["t_s"] / scale for r in rows if r["vfd_state"] == "FAULT"), None)
            if trip is not None:
                ax.axvline(trip, color=INK, lw=0.7)
            ax.set_title(f"{run.plant.p_rated_kw:.0f} kW · SG {run.plant.sg:.2f} · {run.plant.f_set:.0f} Hz", fontsize=7)
            for a in (ax, ax2):
                a.tick_params(labelsize=6)
            if j == 0:
                ax.set_ylabel(name, fontsize=8, rotation=0, ha="right", va="center")
    fig.suptitle("Randomised dataset: four different pumps per scenario.  "
                 "Orange: current (× FLA, left) · green: discharge pressure (bar, right) · red dotted: fault · black: trip",
                 fontsize=11, y=0.997)
    fig.tight_layout(rect=(0, 0, 1, 0.99))
    path = os.path.join(out, "dataset_grid.png")
    fig.savefig(path, dpi=95)
    print("wrote", path)


def gif(out, seed, name, every_s=1.0, fps=20):
    from matplotlib.animation import FuncAnimation, PillowWriter

    plant, twin, rng = commission(seed)
    run, rows, outs = watch(name, plant, twin, rng)
    t = [r["t_s"] for r in rows]
    step = max(1, int(round(every_s / run.dt)))
    frames = list(range(step, len(rows), step)) + [len(rows) - 1]

    fig = plt.figure(figsize=(10, 6.8))
    gs = fig.add_gridspec(4, 1, height_ratios=[0.9, 3, 3, 1.4], hspace=0.35)
    fig.subplots_adjust(left=0.09, right=0.97, top=0.97, bottom=0.07)
    head = fig.add_subplot(gs[0])
    head.axis("off")
    panels = []
    for k, (key, label, unit) in enumerate([("dp_bar", "Differential pressure", "bar"),
                                             ("current_a", "Motor current", "A")]):
        ax = fig.add_subplot(gs[k + 1])
        meas, pred, band = series(rows, outs, key)
        finite = [v for v in meas + pred if not math.isnan(v)]
        ax.set_xlim(0, run.duration)
        ax.set_ylim(min(finite) - 0.08 * (max(finite) - min(finite)) - 0.1, max(finite) * 1.12)
        ax.set_ylabel(f"{label}\n({unit})", fontsize=9)
        ax.grid(alpha=0.15)
        ax.tick_params(labelsize=8)
        if run.fault.onset < run.duration:
            ax.axvline(run.fault.onset, color=HOLD, lw=0.8, ls=":")
            if k == 0:
                ax.text(run.fault.onset, 1.0, " fault starts", color=HOLD, fontsize=8, va="top",
                        transform=ax.get_xaxis_transform())
        lm, = ax.plot([], [], color=INK, lw=1.0, label="measured")
        lp, = ax.plot([], [], color=TWIN, lw=1.6, ls="--", label="digital twin expects")
        panels.append((ax, key, meas, pred, band, lm, lp))
        if k == 0:
            ax.legend(fontsize=8, loc="lower right")
    axf = fig.add_subplot(gs[3])
    axf.set_xlim(0, run.duration)
    axf.set_ylim(0, 65)
    axf.set_ylabel("Drive\n(Hz)", fontsize=9)
    axf.set_xlabel("seconds", fontsize=8)
    axf.tick_params(labelsize=8)
    axf.grid(alpha=0.15)
    lf, = axf.plot([], [], color="#7c3aed", lw=1.0)
    fills = []

    def draw(n):
        for f in fills:
            f.remove()
        fills.clear()
        tt = t[: n + 1]
        for ax, key, meas, pred, band, lm, lp in panels:
            lm.set_data(tt, meas[: n + 1])
            lp.set_data(tt, pred[: n + 1])
            lo = [p - 4 * b for p, b in zip(pred[: n + 1], band[: n + 1])]
            hi = [p + 4 * b for p, b in zip(pred[: n + 1], band[: n + 1])]
            fills.append(ax.fill_between(tt, lo, hi, color=TWIN, alpha=0.12, lw=0))
            for a, b in spans(tt, [o["holding"] for o in outs[: n + 1]]):
                fills.append(ax.axvspan(a, b, color=HOLD, alpha=0.07, lw=0))
        lf.set_data(tt, [r["freq_hz"] for r in rows[: n + 1]])
        head.clear()
        head.axis("off")
        o, r = outs[n], rows[n]
        detail = ""
        if not o["steady"]:
            status, color = ("Twin: waiting for steady running" if r["freq_hz"] < 10 else "Twin: speed changing, watching"), MUTED
        else:
            bits = []
            for key, lab, unit in (("dp_bar", "pressure", "bar"), ("current_a", "current", "A")):
                bits.append(f"{lab}: expects {o[f'{key}_pred']:.2f} {unit}, reads {o[f'{key}_pred'] + o[f'{key}_res']:.2f} ({o[f'{key}_z']:+.0f}σ)")
            detail = "   ·   ".join(bits)
            if o["holding"]:
                status, color = "Twin: something's off, not learning", HOLD
            else:
                status, color = "Twin: behaving as learned, still learning", OK
        head.text(0, 1.0, f"{name.replace('_', ' ')}: {sim.SCENARIOS[name]}", fontsize=9.5, color=INK, va="top",
                  transform=head.transAxes)
        head.text(0, 0.55, f"t = {r['t_s']:3.0f} s   drive {r['freq_hz']:4.1f} Hz   {status}", fontsize=10.5, color=color,
                  va="center", transform=head.transAxes, weight="bold")
        head.text(0, 0.1, detail, fontsize=9, color=INK, va="center", transform=head.transAxes)
        return []

    anim = FuncAnimation(fig, draw, frames=frames, blit=False)
    path = os.path.join(out, f"{name}_twin.gif")
    anim.save(path, writer=PillowWriter(fps=fps), dpi=90)
    # Pillow merges the repeated end frames; re-save with a pause on the last one instead.
    from PIL import Image, ImageSequence
    im = Image.open(path)
    seq = [f.copy() for f in ImageSequence.Iterator(im)]
    durations = [int(1000 / fps)] * (len(seq) - 1) + [3000]
    seq[0].save(path, save_all=True, append_images=seq[1:], duration=durations, loop=0, optimize=True)
    print("wrote", path, f"({len(seq)} frames)")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("what", choices=["sheet", "grid", "gif", "all"])
    ap.add_argument("--out", default="out/plots")
    ap.add_argument("--seed", type=int, default=3)
    ap.add_argument("--scenario", default="leak_pid")
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    if a.what in ("sheet", "all"):
        sheet(a.out, a.seed)
    if a.what in ("grid", "all"):
        grid(a.out, a.seed)
    if a.what in ("gif", "all"):
        gif(a.out, a.seed, a.scenario)


if __name__ == "__main__":
    main()
