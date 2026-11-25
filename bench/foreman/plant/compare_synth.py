#!/usr/bin/env python3
"""Compare year runs that differ only in how much of Foreman's own synthetic data the twins learn.

Run the variants first (same seed, same plan), e.g.:

  for s in 0 0.1 0.25 0.5 0.75; do python3 year.py run --out synth-$(python3 -c "print(int($s*100))") --synthetic $s & done

then `python3 compare_synth.py` writes out/synth-compare.txt and out/synth-compare.png.
Synthetic data starts after day 30, so the comparisons below are for days 30 on.
"""

import glob
import json
import os
import statistics as st
from collections import defaultdict

import numpy as np

from buttercream import OUT

START = 30
INK, INK2, MUTED, GRID, AXIS, SURFACE = "#0b0b0b", "#52514e", "#898781", "#e1e0d9", "#c3c2b7", "#fcfcfb"
# Baseline in secondary ink; shares on one blue ramp, light → dark (validated as an ordinal ramp).
BLUES = ["#86b6ef", "#3987e5", "#1c5cab", "#0d366b"]


def runs():
    out = []
    for d in glob.glob(os.path.join(OUT, "synth-*")):
        if not os.path.exists(os.path.join(d, "scores.json")):
            continue
        parts = os.path.basename(d).split("-")  # synth-<share>[-s<seed>]; no suffix is seed 1
        share = int(parts[1])
        seed = int(parts[2][1:]) if len(parts) > 2 else 1
        sc = json.load(open(os.path.join(d, "scores.json")))
        log = [json.loads(l) for l in open(os.path.join(d, "log.jsonl"))]
        tw = json.load(open(os.path.join(d, "twins", "twins.json")))
        out.append({"share": share, "seed": seed, "dir": d, "scores": sc, "log": log, "twins": tw})
    return sorted(out, key=lambda r: (r["seed"], r["share"]))


def summarize(r, base):
    hits = [h for h in r["scores"]["hits"] if h["observable"] and h["d0"] >= START]
    caught = [h for h in hits if h["detected"]]
    by_cls = defaultdict(lambda: [0, 0])
    for h in hits:
        by_cls[h["cls"]][0] += 1
        by_cls[h["cls"]][1] += h["detected"]
    wear = [h for h in caught if h["cls"] == "wear"]
    false = [f for f in r["scores"]["false"] if f["day"] >= START]
    # Paired with the baseline: same events, how much sooner or later.
    bmap = {h["id"]: h for h in base["scores"]["hits"]}
    dd = [h["delay_h"] - bmap[h["id"]]["delay_h"] for h in wear if bmap.get(h["id"], {}).get("detected")]
    # Fidelity: each twin's model error against the baseline's, same day.
    ratio = defaultdict(list)
    bl = {e["day"]: e for e in base["log"]}
    for e in r["log"]:
        if e["day"] < START or e["day"] not in bl:
            continue
        m = e["date"][:7]
        for n, s in e["twins"].items():
            b = bl[e["day"]]["twins"].get(n, {})
            for k, v in (s.get("rmse") or {}).items():
                bv = (b.get("rmse") or {}).get(k)
                if v and bv and k != "dc_bus":
                    ratio[m].append(v / bv)
    synth = sum(t.get("synth_learned", 0) for t in r["twins"].values())
    real = sum(t.get("learned", 0) for t in r["twins"].values())
    return {
        "share": r["share"], "seed": r["seed"], "wear_deltas_d": [x / 24 for x in dd], "observable": len(hits), "caught": len(caught),
        "by_cls": {k: tuple(v) for k, v in by_cls.items()},
        "wear_delay_d": st.median([h["delay_h"] / 24 for h in wear]) if wear else None,
        "wear_vs_base_d": st.mean(dd) / 24 if dd else 0.0,
        "false": len(false), "false_h": sum(f["minutes"] for f in false) / 60,
        "false_by_month": _by_month(false, r["log"]),
        "resets": sum(len(e.get("accepted", [])) for e in r["log"] if e["day"] >= START),
        "retrains": sum(len(s.get("retrained", [])) for e in r["log"] if e["day"] >= START for s in e["twins"].values()),
        # The median twin-day is unchanged at every share; the mean shows the twins it hurts.
        "err_ratio": {m: float(np.mean(v)) for m, v in sorted(ratio.items())},
        "err_p90": float(np.percentile([x for v in ratio.values() for x in v], 90)) if ratio else 1.0,
        "synth_share_real": synth / max(1, synth + real),
        "missed": sorted(f"{h['kind']} {h['target']} (day {h['d0']})" for h in hits if not h["detected"]),
        "trips_first": sum(1 for h in hits if h.get("before_trip")),
    }


def _by_month(false, log):
    dates = {e["day"]: e["date"][:7] for e in log}
    c = defaultdict(int)
    for f in false:
        c[dates.get(f["day"], "?")] += 1
    return dict(c)


def report(S):
    L = [f"Foreman's own synthetic data vs. none, buttercream year (seed 1), days {START}-364\n"]
    L.append(f"{'synthetic':>9} {'caught':>9} {'wear: median':>13} {'vs 0 %':>8} {'false holds':>12} {'hours':>6} {'resets':>7} {'retrains':>9} {'model error vs 0 %':>19} {'worst 10 %':>11}")
    for s in S:
        errs = list(s["err_ratio"].values())
        L.append(f"{s['share']:>8}% {s['caught']:>4}/{s['observable']:<4} {s['wear_delay_d'] or 0:>11.1f} d {s['wear_vs_base_d']:>+6.1f} d {s['false']:>12} {s['false_h']:>6.1f} {s['resets']:>7} {s['retrains']:>9} {np.mean(errs) if errs else 1:>18.3f}× {s['err_p90']:>10.3f}×")
    L.append("\nCaught by kind (caught/observable):")
    kinds = sorted({k for s in S for k in s["by_cls"]})
    L.append(f"{'':>9} " + " ".join(f"{k:>10}" for k in kinds))
    for s in S:
        L.append(f"{s['share']:>8}% " + " ".join(f"{'%d/%d' % (s['by_cls'].get(k, (0, 0))[1], s['by_cls'].get(k, (0, 0))[0]):>10}" for k in kinds))
    base = set(S[0]["missed"])
    L.append("\nMissed (observable), against 0 %:")
    for s in S:
        m = set(s["missed"])
        L.append(f"  {s['share']:>3}%: {len(m)} missed" + (f"; newly caught: {', '.join(sorted(base - m))}" if base - m else "") + (f"; newly missed: {', '.join(sorted(m - base))}" if m - base else ""))
    L.append("\nFalse holds by month:")
    months = sorted({m for s in S for m in s["false_by_month"]})
    L.append(f"{'':>9} " + " ".join(f"{m[2:]:>6}" for m in months))
    for s in S:
        L.append(f"{s['share']:>8}% " + " ".join(f"{s['false_by_month'].get(m, 0):>6}" for m in months))
    text = "\n".join(L)
    with open(os.path.join(OUT, "synth-compare.txt"), "w") as fh:
        fh.write(text + "\n")
    with open(os.path.join(OUT, "synth-compare.json"), "w") as fh:
        json.dump(S, fh, indent=1)
    print(text)


def plot(S):
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    plt.rcParams.update({"font.family": "sans-serif", "font.size": 9, "axes.edgecolor": AXIS, "axes.labelcolor": INK2,
                         "xtick.color": MUTED, "ytick.color": MUTED, "text.color": INK, "axes.titlecolor": INK})
    col = {s["share"]: (INK2 if s["share"] == 0 else BLUES[min(i - 1, 3)]) for i, s in enumerate(S)}
    lab = {s["share"]: ("none (baseline)" if s["share"] == 0 else f"{s['share']} % synthetic") for s in S}
    fig, ax = plt.subplots(2, 2, figsize=(12, 7.5), facecolor=SURFACE)
    for a in ax.flat:
        a.set_facecolor(SURFACE)
        a.grid(axis="y", color=GRID, lw=0.6)
        a.set_axisbelow(True)
        for sp in ("top", "right"):
            a.spines[sp].set_visible(False)
    shares = [s["share"] for s in S]
    xs = np.arange(len(S))

    def bars(a, vals, title, fmt, unit=""):
        b = a.bar(xs, vals, width=0.6, color=[col[x] for x in shares], edgecolor=SURFACE, linewidth=2)
        a.set_xticks(xs, [f"{x} %" for x in shares])
        a.set_xlabel("share of synthetic samples learned")
        a.set_title(title, loc="left", fontsize=10)
        for r, v in zip(b, vals):
            a.annotate(fmt.format(v) + unit, (r.get_x() + r.get_width() / 2, r.get_height()), ha="center", va="bottom", fontsize=8, color=INK2, xytext=(0, 2), textcoords="offset points")

    bars(ax[0, 0], [100 * s["caught"] / s["observable"] for s in S], f"Faults caught, days {START}–364 (% of those a twin can see)", "{:.0f}", " %")
    ax[0, 0].set_ylim(0, 110)
    bars(ax[0, 1], [s["false"] for s in S], "Holds no event explains", "{:.0f}")
    bars(ax[1, 0], [s["wear_delay_d"] or 0 for s in S], "Wear: median days from onset to first hold", "{:.1f}", " d")
    a = ax[1, 1]
    for s in S:
        ms = list(s["err_ratio"])
        a.plot(range(len(ms)), list(s["err_ratio"].values()), color=col[s["share"]], lw=2, marker="o", ms=4, label=lab[s["share"]])
    a.set_xticks(range(len(ms)), [m[2:] for m in ms], rotation=45)
    a.axhline(1, color=AXIS, lw=1)
    a.set_title("Model error on real samples, vs. baseline (mean over twins)", loc="left", fontsize=10)
    a.set_ylabel("× baseline")
    a.legend(frameon=False, fontsize=8, loc="upper left")
    fig.suptitle("Foreman training on its own synthetic data after day 30 — buttercream year, seed 1", x=0.01, ha="left", fontsize=11)
    fig.tight_layout()
    path = os.path.join(OUT, "synth-compare.png")
    fig.savefig(path, dpi=130, facecolor=SURFACE)
    print("wrote", path)


def pooled(S):
    """Every seed's runs against that seed's own baseline, pooled per share."""
    seeds = sorted({s["seed"] for s in S})
    shares = sorted({s["share"] for s in S})
    rows = []
    for sh in shares:
        g = [s for s in S if s["share"] == sh]
        d = [x for s in g for x in s["wear_deltas_d"]]
        rows.append({
            "share": sh, "seeds": [s["seed"] for s in g],
            "caught": sum(s["caught"] for s in g), "observable": sum(s["observable"] for s in g),
            "false": [s["false"] for s in g], "retrains": [s["retrains"] for s in g],
            "wear_delta_mean": float(np.mean(d)) if d else 0.0, "wear_n": len(d),
            "wear_sooner": sum(1 for x in d if x < -0.5), "wear_later": sum(1 for x in d if x > 0.5),
            "err_mean": float(np.mean([np.mean(list(s["err_ratio"].values())) for s in g])),
            "err_p90": [round(s["err_p90"], 3) for s in g],
            "missed": [len(s["missed"]) for s in g],
        })
    L = [f"Foreman's own synthetic data, seeds {', '.join(map(str, seeds))}: each run against its own seed's baseline, days {START}-364\n",
         f"{'synthetic':>9} {'caught':>9} {'missed by seed':>15} {'false holds by seed':>20} {'wear vs none':>13} {'sooner/later':>13} {'retrains by seed':>17} {'error':>7} {'worst 10 % by seed':>20}"]
    for r in rows:
        L.append(f"{r['share']:>8}% {r['caught']:>4}/{r['observable']:<4} {str(r['missed']):>15} {str(r['false']):>20} {r['wear_delta_mean']:>+11.1f} d {str(r['wear_sooner']) + '/' + str(r['wear_later']) + ' of ' + str(r['wear_n']):>13} {str(r['retrains']):>17} {r['err_mean']:>6.3f}× {str(r['err_p90']):>20}")
    L.append("\nwear vs none: mean change in days from onset to first hold, same wear events, caught in both runs;")
    L.append("sooner/later: events caught at least half a day sooner / later.")
    text = "\n".join(L)
    with open(os.path.join(OUT, "synth-compare-seeds.txt"), "w") as fh:
        fh.write(text + "\n")
    print(text)
    return rows, seeds


def plot_pooled(S, rows, seeds):
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    plt.rcParams.update({"font.family": "sans-serif", "font.size": 9, "axes.edgecolor": AXIS, "axes.labelcolor": INK2,
                         "xtick.color": MUTED, "ytick.color": MUTED, "text.color": INK, "axes.titlecolor": INK})
    shares = [r["share"] for r in rows]
    col = {sh: (INK2 if sh == 0 else BLUES[min(i - 1, 3)]) for i, sh in enumerate(shares)}
    fig, ax = plt.subplots(1, 3, figsize=(13, 4.2), facecolor=SURFACE)
    xs = np.arange(len(shares))
    marks = {sd: m for sd, m in zip(seeds, ["o", "s", "^", "D", "v"])}

    def panel(a, key, title, unit=""):
        a.set_facecolor(SURFACE)
        a.grid(axis="y", color=GRID, lw=0.6)
        a.set_axisbelow(True)
        for sp in ("top", "right"):
            a.spines[sp].set_visible(False)
        for i, sh in enumerate(shares):
            vals = [(s["seed"], key(s)) for s in S if s["share"] == sh]
            a.bar(i, np.mean([v for _, v in vals]), width=0.6, color=col[sh], edgecolor=SURFACE, linewidth=2, alpha=0.35)
            for sd, v in vals:
                a.scatter(i, v, color=col[sh], marker=marks[sd], s=40, edgecolors=SURFACE, linewidths=1.5, zorder=3)
        a.set_xticks(xs, [f"{x} %" for x in shares])
        a.set_xlabel("share of synthetic samples learned")
        a.set_title(title, loc="left", fontsize=10)

    panel(ax[0], lambda s: 100 * s["caught"] / s["observable"], "Faults caught (% of those visible)")
    panel(ax[1], lambda s: s["false"], "Holds no event explains")
    ax[1].set_yscale("symlog", linthresh=50)
    panel(ax[2], lambda s: float(np.mean(s["wear_deltas_d"])) if s["wear_deltas_d"] else 0.0, "Wear: days sooner (−) or later (+) than none")
    ax[2].axhline(0, color=AXIS, lw=1)
    from matplotlib.lines import Line2D
    ax[0].legend([Line2D([], [], marker=marks[sd], color=INK2, ls="") for sd in seeds], [f"seed {sd}" for sd in seeds],
                 frameon=False, fontsize=8, loc="lower left")
    fig.suptitle(f"Foreman's own synthetic data after day 30 — {len(seeds)} simulated years (bars: mean; marks: each seed)", x=0.01, ha="left", fontsize=11)
    fig.tight_layout()
    path = os.path.join(OUT, "synth-compare-seeds.png")
    fig.savefig(path, dpi=130, facecolor=SURFACE)
    print("wrote", path)


if __name__ == "__main__":
    R = runs()
    S = []
    for seed in sorted({r["seed"] for r in R}):
        group = [r for r in R if r["seed"] == seed]
        base = next((r for r in group if r["share"] == 0), None)
        if base:
            S += [summarize(r, base) for r in group]
    s1 = [s for s in S if s["seed"] == 1]
    if s1:
        report(s1)
        plot(s1)
    if len({s["seed"] for s in S}) > 1:
        rows, seeds = pooled(S)
        plot_pooled(S, rows, seeds)
