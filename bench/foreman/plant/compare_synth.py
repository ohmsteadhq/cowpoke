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
        share = int(os.path.basename(d).split("-")[1])
        sc = json.load(open(os.path.join(d, "scores.json")))
        log = [json.loads(l) for l in open(os.path.join(d, "log.jsonl"))]
        tw = json.load(open(os.path.join(d, "twins", "twins.json")))
        out.append({"share": share, "dir": d, "scores": sc, "log": log, "twins": tw})
    return sorted(out, key=lambda r: r["share"])


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
        "share": r["share"], "observable": len(hits), "caught": len(caught),
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


if __name__ == "__main__":
    R = runs()
    base = next(r for r in R if r["share"] == 0)
    S = [summarize(r, base) for r in R]
    report(S)
    plot(S)
