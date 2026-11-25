#!/usr/bin/env python3
"""A year of the buttercream plant, heavy on faults, with Foreman's twins learning day by day.

Same plant, tags and twins as buttercream.py (run its `config` and `match` first). This adds a seeded
random year of trouble on top of normal production:

- wear and tear that grows for weeks until maintenance fixes it: bearings, PD-pump lobes, centrifugal
  impellers, fan belts, scraped-surface blades, conveyor chains;
- process faults: strainer clogs, filler-nozzle blockages, conveyor jams, cooler freeze-ups, air in a
  water or glycol loop, supply sags;
- faulty sensors: stuck, drifting, offset after a bad calibration, open loop (3.6 mA), spiking, noisy;
- comms dropouts: a drive's Modbus service or the PLC unreadable for minutes to hours;
- normal change: recipe runs (chocolate, cream cheese, lemon), production-rate weeks, seasons,
  holidays, repairs and recalibrations.

An operator behaves as one would: after a repair or a recalibration, a twin that held most of that day
is told "this is the new normal" (`cowpoke twins reset`). Recipe runs need no one: the twins read the
line's recipe number and keep a model per recipe. The twins never see the
schedule; the report compares what they flagged with it.

  python3 year.py plan --seed 1            # out/year/events.json: the year's schedule
  python3 year.py run --seed 1 --days 365  # simulate, replay, retrain nightly; out/year/log.jsonl
  python3 year.py report                   # out/year/report.txt and charts
"""

import argparse
import gzip
import io
import json
import math
import os
import subprocess
import sys
import time
from datetime import datetime, timezone

import numpy as np

import buttercream as bc
from buttercream import DAY0, DT, MACHINES, N, OUT, fla

YEAR = os.path.join(OUT, "year")

# Transmitters, their spans (for sensor faults), and their Cowpoke keys.
SENSORS = {
    "PT-301": (0, 10), "PT-401": (0, 10), "FT-301": (0, 10), "TT-301": (0, 100), "TT-302": (0, 100), "TT-303": (0, 100),
    "DT-401": (0, 2), "WT-201": (0, 2000), "TT-201": (0, 100), "LT-401": (0, 100), "LT-210": (0, 100), "TT-210": (0, 100),
    "TT-401": (0, 100), "TT-601": (0, 120), "TT-602": (-20, 40), "TT-501": (0, 100), "FT-501": (0, 40),
    "LT-110": (0, 100), "LT-120": (0, 100), "LT-130": (0, 100), "LT-501": (0, 100),
}
PD = ["P-301", "P-401", "P-110", "P-120", "P-130", "P-201"]
CENTRIFUGAL = ["P-501", "P-502", "P-601", "P-602"]
CONVEYORS = ["CV-411", "CV-412", "CV-413", "CV-414"]
BIG = ["AG-201", "AE-401", "HX-302", "HX-301", "B-101", "AG-202", "P-301", "P-601", "P-602", "P-501", "DC-101", "SC-101", "P-401"]
SERVICES = [f"vfd-{d.split('-')[1]}" for _, (d, _, _) in MACHINES.items() if d not in bc.VIA_PLC]
RECIPES = {"vanilla": (1.0, 1.0, 36.0), "chocolate": (1.38, 1.25, 33.0), "cream cheese": (1.2, 1.15, 34.0), "lemon": (0.92, 0.95, 37.0)}


def date_of(day):
    return datetime.fromtimestamp(DAY0 + day * 86400, timezone.utc)


def is_off(day):
    d = date_of(day)
    return d.weekday() == 6 or (d.month, d.day) in {(12, 25), (12, 26), (1, 1), (7, 4)}


# ---------------------------------------------------------------------------------------- the plan

def plan(seed, days=365):
    rng = np.random.default_rng(seed)
    ev = []

    def add(kind, cls, target, d0, h0, d1, h1, sev=1.0, ramp_days=0.0, **kw):
        ev.append({"id": len(ev), "kind": kind, "cls": cls, "target": target, "d0": int(d0), "h0": round(float(h0), 3),
                   "d1": int(d1), "h1": round(float(h1), 3), "sev": round(float(sev), 3), "ramp_days": round(float(ramp_days), 2), **kw})

    def prod_day(lo, hi):
        for _ in range(100):
            d = int(rng.integers(lo, hi))
            if not is_off(d):
                return d
        return lo

    def times(rate):
        return rng.poisson(rate * days / 365)

    first = 10  # let the twins warm up on a clean start
    # --- wear and tear: grows for weeks, maintenance fixes it at the end
    for kind, targets, rate, ramp in (("bearing wear", BIG + CONVEYORS, 10, (10, 40)), ("lobe wear", PD, 4, (20, 60)),
                                      ("impeller wear", CENTRIFUGAL, 3, (30, 90)), ("belt slip", ["B-101", "DC-101"], 3, (5, 20)),
                                      ("blade wear", ["HX-301", "HX-302"], 2, (30, 60)), ("chain wear", CONVEYORS, 4, (14, 45))):
        for _ in range(times(rate)):
            d0 = prod_day(first, days - 5)
            r = rng.uniform(*ramp)
            d1 = min(days - 1, d0 + int(r + rng.uniform(0, 10)))
            add(kind, "wear", str(rng.choice(targets)), d0, 6, d1, 24, rng.uniform(0.6, 1.0), r)
    # --- process faults
    for _ in range(times(8)):
        d0 = prod_day(first, days - 5)
        add("strainer clog", "process", str(rng.choice(["P-301", "P-401"])), d0, 6, d0 + int(rng.integers(1, 4)), 22, rng.uniform(0.5, 1.0), rng.uniform(1, 3))
    for _ in range(times(6)):
        d0 = prod_day(first, days)
        h0 = rng.uniform(7, 20)
        add("filler blockage", "process", "P-401", d0, h0, d0, h0 + rng.uniform(0.3, 1.5), rng.uniform(0.6, 1.0))
    for _ in range(times(8)):
        d0 = prod_day(first, days)
        h0 = rng.uniform(7, 21)
        add("conveyor jam", "process", str(rng.choice(CONVEYORS)), d0, h0, d0, h0 + rng.uniform(0.05, 0.15), 1.0)
    for _ in range(times(3)):
        d0 = prod_day(first, days - 6)
        add("cooler freeze-up", "process", "HX-302", d0, 6, d0 + int(rng.integers(1, 5)), 22, rng.uniform(0.5, 1.0), 3)
    for _ in range(times(3)):
        d0 = prod_day(first, days - 2)
        add("air in loop", "process", str(rng.choice(["P-601", "P-602"])), d0, rng.uniform(0, 20), d0 + int(rng.integers(0, 2)), 24, rng.uniform(0.6, 1.0))
    for _ in range(times(8)):
        d0 = prod_day(first, days)
        h0 = rng.uniform(6.5, 21.5)
        add("supply sag", "process", "all drives", d0, h0, d0, h0 + 2.5 / 3600, rng.uniform(0.15, 0.4))
    # --- faulty sensors
    sensors = list(SENSORS)
    for kind, rate, dur in (("sensor stuck", 8, (2 / 24, 2)), ("sensor drift", 6, (10, 60)), ("sensor offset", 4, (5, 30)),
                            ("sensor open loop", 6, (0.5 / 24, 0.5)), ("sensor spikes", 5, (1, 3)), ("sensor noisy", 4, (1, 7))):
        for _ in range(times(rate)):
            d0 = prod_day(first, days - 3)
            h0 = rng.uniform(6.5, 20)
            span = rng.uniform(*dur)
            end = d0 * 24 + h0 + span * 24
            d1, h1 = int(end // 24), end % 24
            add(kind, "sensor", str(rng.choice(sensors)), d0, h0, min(d1, days - 1), h1 if d1 < days else 24,
                rng.uniform(0.05, 0.15) * float(rng.choice([-1, 1])), span if kind == "sensor drift" else 0)
    # --- comms
    for _ in range(times(10)):
        d0 = prod_day(first, days)
        h0 = rng.uniform(6.5, 21)
        target = "cp1-plc" if rng.random() < 0.2 else str(rng.choice(SERVICES))
        add("comms dropout", "comms", target, d0, h0, d0, min(23.9, h0 + rng.uniform(5, 120) / 60))
    # --- normal change: recipe runs, rate weeks, repairs and recalibrations
    d = 30
    while d < days - 10:
        d += int(rng.integers(40, 90))
        if d >= days - 10:
            break
        name = str(rng.choice(["chocolate", "cream cheese", "lemon"]))
        d1 = min(days - 1, d + int(rng.integers(7, 22)))
        add("recipe run", "normal", name, d, 6, d1, 22, recipe=name)
        d = d1
    for _ in range(times(6)):
        d0 = prod_day(first, days - 7)
        add("rate week", "normal", "P-301", d0, 6, d0 + 6, 22, float(rng.choice([33.5, 34.5, 37.5, 38.5])))
    for e in list(ev):
        if e["cls"] == "wear":
            add("repair", "normal", e["target"], min(days - 1, e["d1"] + 1), 6, min(days - 1, e["d1"] + 1), 22, of=e["id"])
        if e["kind"] in ("sensor drift", "sensor offset"):
            add("recalibration", "normal", e["target"], min(days - 1, e["d1"] + 1), 6, min(days - 1, e["d1"] + 1), 22, of=e["id"])
    ev.sort(key=lambda e: (e["d0"], e["h0"]))
    return ev


def level(e, day, h):
    """How far along an event is at hours h of day (0 outside it; ramps for wear)."""
    t = day * 24 + h
    t0, t1 = e["d0"] * 24 + e["h0"], e["d1"] * 24 + e["h1"]
    inside = (t >= t0) & (t < t1)
    if e["ramp_days"] > 0:
        return np.where(inside, abs(e["sev"]) * np.clip((t - t0) / (e["ramp_days"] * 24), 0.05, 1), 0.0)
    return np.where(inside, abs(e["sev"]), 0.0)


def active_on(events, day):
    return [e for e in events if e["d0"] <= day <= e["d1"]]


# ---------------------------------------------------------------------------------------- one day

def simulate_day(day, events, seed):
    rng = np.random.default_rng(100000 * seed + day)
    t = np.arange(N) * DT
    h = t / 3600
    off = is_off(day)
    act = active_on(events, day)
    sig = {}

    def win(a, b):
        return (h >= a) & (h < b)

    def lv(kind, target):
        out = np.zeros(N)
        for e in act:
            if e["kind"] == kind and e["target"] == target:
                out = np.maximum(out, level(e, day, h))
        return out

    recipe = next((e["recipe"] for e in act if e["kind"] == "recipe run"), "vanilla")
    mu_k, batch_k, p301_hz = RECIPES[recipe]
    rate = next((e["sev"] for e in act if e["kind"] == "rate week"), None)
    if rate:
        p301_hz = rate
    season = 10 * math.sin(2 * math.pi * (day - 120) / 365)  # ambient swing, °C

    ref = {m: np.zeros(N) for m in MACHINES}
    ref["P-601"][:] = 50.0
    ref["P-602"][:] = 40.0 if off else 55.0
    batch_min = np.full(N, -1.0)
    line = np.zeros(N, bool)
    if not off:
        ref["DC-101"][win(5.5, 22.0)] = 50.0
        for b in range(16):
            s = 5.5 + b
            if s > 21.0:
                break
            m = (h - s) * 60
            batch_min[(m >= 0) & (m < 60)] = m[(m >= 0) & (m < 60)]
        bm = batch_min
        on = lambda a, b: (bm >= a) & (bm < b)  # noqa: E731
        ref["P-110"][on(0, 4)] = 40
        ref["P-120"][on(4, 7)] = 40
        ref["P-130"][on(7, 9)] = 30
        ref["SC-101"][on(9, 24)] = 45
        ref["B-101"][on(8.5, 24.5)] = 55
        ref["AG-202"][on(0, 50)] = 30
        ref["AG-201"][on(2, 24)] = 25
        ref["AG-201"][on(24, 44)] = 50
        ref["AG-201"][on(44, 50)] = 20
        ref["P-201"][on(50, 58)] = 45
        line = win(6.0, 22.0)
        ref["P-301"][line] = p301_hz
        ref["HX-301"][line] = 40
        ref["HX-302"][line] = 45
        ref["AE-401"][win(6.15, 22.0)] = 50
        for c, f in (("CV-411", 50), ("CV-412", 40), ("CV-413", 40), ("CV-414", 50)):
            r = np.where(win(6.2, 22.0), f, 0.0)
            r[(t % 1200) < 30] = 0
            ref[c] = r
        for a, b, f in ((22.25, 22.42, 50), (22.42, 23.08, 55), (23.08, 23.25, 50), (23.25, 23.58, 52), (23.58, 23.92, 50)):
            ref["P-501"][win(a, b)] = f
            ref["P-502"][win(a, b)] = 48
    elif date_of(day).weekday() == 6:
        for a, b, f in ((8.0, 8.3, 50), (8.3, 9.5, 55), (9.5, 9.8, 50), (9.8, 10.5, 52), (10.5, 10.9, 50)):
            ref["P-501"][win(a, b)] = f
            ref["P-502"][win(a, b)] = 48
    lt401 = 55 + 10 * np.sin(2 * np.pi * t / 1500 + day) + rng.normal(0, 0.5, N)
    if not off:
        ref["P-401"][win(6.15, 22.0)] = np.clip(34 + 0.5 * (lt401 - 55), 25, 45)[win(6.15, 22.0)]

    walk = np.cumsum(rng.normal(0, 0.03 / math.sqrt(N), N))
    walk -= np.linspace(0, walk[-1], N)
    mu_line = mu_k * (1 + 0.03 * np.sin(2 * np.pi * t / 7200) + walk)
    bm = batch_min
    fill = np.clip(0.15 + np.where(bm < 0, 0, np.minimum(bm, 24) / 24 * 0.85), 0, 1)
    mu_batch = np.where(bm < 9, 0.35, np.where(bm < 24, 0.35 + (bm - 9) / 15 * 0.65, 1.0)) * batch_k
    tt301 = np.where(line, 74 + 1.2 * np.sin(2 * np.pi * t / 3600 + day) + rng.normal(0, 0.2, N), 30.0)
    tt303 = np.where(line, 18 + 0.12 * season + 1.5 * np.sin(2 * np.pi * t / 5400 + 0.7 * day) + rng.normal(0, 0.2, N), 22.0)
    dt401 = np.where(line, 0.75 + 0.05 * np.sin(2 * np.pi * t / 4000 + day) + rng.normal(0, 0.003, N), 0.0)
    tt201 = np.where(bm < 0, 22.0, np.where(bm < 9, 38.0, np.maximum(26.0, 38.0 - (bm - 9) / 15 * 12))) + rng.normal(0, 0.2, N)
    mu_batch = mu_batch * np.exp(-0.03 * (tt201 - 28))
    wt201 = 1400 * fill * (bm >= 0) + 120 + rng.normal(0, 3, N)
    freeze = lv("cooler freeze-up", "HX-302") * np.clip((h - 6) / 16, 0, 1) * 0.35
    blade = lv("blade wear", "HX-302")
    tt303 = tt303 - 3 * freeze + 3 * blade
    clog = {p: lv("strainer clog", p) * np.clip((h - 6) / 16, 0, 1) for p in ("P-301", "P-401")}
    block401 = lv("filler blockage", "P-401")
    sag = np.ones(N)
    for e in act:
        if e["kind"] == "supply sag":
            sag[win(e["h0"], e["h0"] + DT / 3600)] = 1 - e["sev"]

    tripped = {}
    for m, (d, hp, model) in MACHINES.items():
        f = np.zeros(N)
        acc = 60 / 6.0 * DT
        r_ = ref[m]
        cur = 0.0
        for i in range(N):
            tgt = r_[i]
            cur = min(tgt, cur + acc) if tgt > cur else max(tgt, cur - acc)
            f[i] = cur
        r = f / 60.0
        runm = f > 0.5
        bearing = lv("bearing wear", m)
        if model == "fan":
            slip = lv("belt slip", m) * (rng.random(N) < 0.4)
            T = 0.85 * r ** 2 * (1 - 0.3 * slip)
        elif model == "centrifugal":
            T = 0.8 * r ** 2 * (1 - 0.2 * lv("impeller wear", m))
            air = lv("air in loop", m)
            T = T * (1 - 0.18 * air + 0.06 * air * rng.normal(0, 1, N))
        elif model == "screw":
            T = 0.35 + 0.25 * fill + 0.05 * rng.normal(0, 1, N)
        elif model == "pd_free":
            T = (0.25 + 0.45 * r * (1.6 if m == "P-130" else 1.0)) * (1 - 0.15 * lv("lobe wear", m))
        elif model == "scraper":
            T = 0.2 + 0.5 * fill * mu_batch * (0.5 + r)
        elif model == "mixer":
            T = fill * (0.12 + mu_batch * (0.3 * r + 0.55 * r ** 2))
        elif model == "sshe_heat":
            T = (0.22 + 0.55 * mu_line * np.exp(-0.04 * (tt301 - 74)) * r * 0.8) * (1 - 0.15 * lv("blade wear", m))
        elif model == "sshe_cool":
            T = (0.25 + 0.6 * (mu_line * np.exp(-0.06 * (tt303 - 18)) + freeze * 2.2) * r * 0.85) * (1 - 0.15 * blade)
        elif model == "aerator":
            T = 0.25 + 0.5 * r ** 2 * (np.maximum(dt401, 0.5) / 0.75) + 0.15 * mu_line * r
        elif model == "conveyor":
            T = 0.35 + 0.15 * (rng.random(N) < 0.3) + 0.35 * lv("chain wear", m) * (1 + 0.5 * rng.normal(0, 1, N))
            jam = lv("conveyor jam", m)
            if jam.any():
                e = next(x for x in act if x["kind"] == "conveyor jam" and x["target"] == m)
                T = T + jam * 1.8 * np.clip((h - e["h0"]) / (e["h1"] - e["h0"]), 0, 1)
        elif model == "pd":
            wear = 0.25 * lv("lobe wear", m)
            if m == "P-301":
                q = 6.0 * r * (1 - 0.03 / mu_line - wear)
                dp = 0.3 + 2.4 * mu_line * q / 6.0 + 0.4 * (q / 6.0) ** 2 + 1.6 * clog["P-301"]
                sig[("FT-301", "pv")] = np.where(runm, q, 0) + rng.normal(0, 0.03, N)
                sig[("PT-301", "pv")] = np.where(runm, dp, 0.05) + rng.normal(0, 0.02, N)
            else:
                dp = (0.5 + 1.8 * mu_line * r + 0.6 * r ** 2) * (1 - wear) + 2.2 * block401 + 1.4 * clog["P-401"]
                sig[("PT-401", "pv")] = np.where(runm, dp, 0.1) + rng.normal(0, 0.02, N)
            T = 0.12 + 0.28 * dp
        else:
            T = np.zeros(N)
        T = T * (1 + 0.3 * bearing) + 0.25 * bearing * rng.normal(0, 1, N) * T
        T = np.where(runm, np.maximum(T, 0), 0.0)
        I_fla = fla(hp)
        I = I_fla * np.sqrt(0.33 ** 2 + (0.944 * T) ** 2) * runm
        heat, trip_at = 0.0, None
        for i in np.nonzero(I > 1.05 * I_fla)[0]:
            heat += ((I[i] / I_fla) ** 2 - 1.05) * DT
            if heat > 60 * (1.5 ** 2 - 1.05):
                trip_at = i
                break
        fault = np.zeros(N)
        if trip_at is not None:
            f[trip_at:] = 0
            I[trip_at:] = 0
            runm = f > 0.5
            fault[trip_at:] = 4
            tripped[m] = round(trip_at * DT / 3600, 3)
        vout = np.where(runm, 460 * np.minimum(1, np.maximum(f / 60, 0.06)), 0)
        vdc = 650 * sag - 14 * T * r + rng.normal(0, 0.8, N)
        sig[(m, "freq")] = f
        sig[(m, "cmd")] = r_
        sig[(m, "current")] = I * (1 + rng.normal(0, 0.004, N))
        sig[(m, "out_voltage")] = vout
        sig[(m, "dc_bus")] = vdc
        sig[(m, "fault")] = fault
    sig[("LT-401", "pv")] = np.where(line, lt401, 20)
    sig[("WT-201", "pv")] = wt201
    sig[("TT-201", "pv")] = tt201
    sig[("LT-210", "pv")] = 50 + 10 * np.sin(2 * np.pi * t / 3600) + rng.normal(0, 0.3, N)
    sig[("TT-210", "pv")] = 24 + rng.normal(0, 0.2, N)
    sig[("DT-401", "pv")] = dt401
    sig[("TT-401", "pv")] = np.where(line, 19 + rng.normal(0, 0.2, N), 22)
    for tag, base in (("LT-110", 70), ("LT-120", 65), ("LT-130", 60), ("LT-501", 80)):
        sig[(tag, "pv")] = base + 8 * np.sin(2 * np.pi * t / 5400) + rng.normal(0, 0.3, N)
    sig[("TT-301", "pv")] = tt301
    sig[("TT-302", "pv")] = tt301 - 1 + rng.normal(0, 0.2, N)
    sig[("TT-303", "pv")] = tt303
    sig[("FT-501", "pv")] = np.where(sig[("P-501", "freq")] > 1, 0.4 * sig[("P-501", "freq")], 0) + rng.normal(0, 0.1, N)
    sig[("TT-501", "pv")] = np.where(win(22.42, 23.08), 80, 30) + rng.normal(0, 0.3, N)
    sig[("TT-601", "pv")] = 82 + 0.1 * season + rng.normal(0, 0.3, N)
    sig[("TT-602", "pv")] = 1 + 0.1 * season - 3 * freeze + rng.normal(0, 0.2, N)

    # --- faulty sensors: the transmitter lies, the process doesn't change
    for e in act:
        if e["cls"] != "sensor" or (e["target"], "pv") not in sig:
            continue
        lo, hi = SENSORS[e["target"]]
        span = hi - lo
        x = sig[(e["target"], "pv")]
        inside = level(e, day, h) > 0
        if not inside.any():
            continue
        k = e["kind"]
        if k == "sensor stuck":
            if "stuck_at" not in e:
                e["stuck_at"] = float(x[np.argmax(inside)])
            x = np.where(inside, e["stuck_at"], x)
        elif k == "sensor drift":
            x = x + np.sign(e["sev"]) * level(e, day, h) * span
        elif k == "sensor offset":
            x = np.where(inside, x + e["sev"] * span, x)
        elif k == "sensor open loop":
            x = np.where(inside, lo - 0.025 * span, x)
        elif k == "sensor spikes":
            spike = inside & (rng.random(N) < 0.005)
            x = np.where(spike, x + rng.choice([-0.3, 0.3], N) * span, x)
        elif k == "sensor noisy":
            x = np.where(inside, x + rng.normal(0, 0.03 * span, N), x)
        sig[(e["target"], "pv")] = x
    blank = {}
    for e in act:
        if e["kind"] == "comms dropout":
            blank[e["target"]] = level(e, day, h) > 0
    sig[("LINE", "recipe")] = np.full(N, float(list(RECIPES).index(recipe) + 1))
    return sig, tripped, blank


def write_day(day, events, seed, path):
    sig, tripped, blank = simulate_day(day, events, seed)
    cols = bc.column_names()
    keys = [k for k in cols if k in sig]
    t0 = DAY0 + day * 86400
    data = [np.arange(N) * DT + t0]
    for k in keys:
        v = sig[k].astype(float)
        svc = cols[k].split("/")[0]
        if svc in blank:
            v = np.where(blank[svc], np.nan, v)
        data.append(v)
    buf = io.StringIO()
    np.savetxt(buf, np.column_stack(data), fmt=["%.1f"] + ["%.3f"] * len(keys), delimiter=",")
    text = buf.getvalue().replace("nan", "")
    with gzip.open(path, "wt", compresslevel=1) as fh:
        fh.write(",".join(["t"] + [cols[k] for k in keys]) + "\n")
        fh.write(text)
    return tripped


# ---------------------------------------------------------------------------------------- run

def twin_reads():
    """For each twin: the keys its models read (required roles) and its context, from twins.yaml."""
    import yaml
    cfg = yaml.safe_load(open(os.path.join(OUT, "train", "twins.yaml")))
    out = {}
    for t in cfg["twins"]:
        roles = {k: t[k] for k in ("freq", "current", "discharge", "suction", "out_voltage", "dc_bus") if t.get(k)}
        out[t["name"]] = {"roles": roles, "inputs": t.get("inputs", []),
                          "services": sorted({v.split("/")[0] for v in list(roles.values()) + t.get("inputs", [])})}
    return out


def key_of(sensor):
    return bc.column_names()[(sensor, "pv")]


def expected_twins(e, reads, inputs_now):
    """Which twins could see this event, and how."""
    if e["kind"] == "supply sag":
        return {n: "blip" for n in reads}
    if e["cls"] in ("wear", "process"):
        return {e["target"]: "hold"} if e["target"] in reads else {}
    if e["cls"] == "sensor":
        key = key_of(e["target"])
        out = {}
        for n, r in reads.items():
            if key in r["roles"].values():
                out[n] = "hold"
            elif any(i.startswith(key) for m in inputs_now.get(n, {}).values() for i in m):
                out[n] = "hold"
        return out
    if e["cls"] == "comms":
        return {n: "nodata" for n, r in reads.items() if e["target"] in r["services"]}
    if e["kind"] == "recipe run":
        return {n: "hold" for n in ("P-301", "HX-301", "HX-302", "AE-401", "P-401", "AG-201", "AG-202")}
    if e["kind"] in ("repair",):
        return {e["target"]: "hold"}
    if e["kind"] == "recalibration":
        key = key_of(e["target"])
        return {n: "hold" for n, r in reads.items() if key in r["roles"].values() or key in r["inputs"]}
    if e["kind"] == "rate week":
        return {"P-301": "hold"}
    return {}


def run(seed, days, keep_every=30):
    os.makedirs(os.path.join(YEAR, "days"), exist_ok=True)
    data = os.path.join(YEAR, "twins")
    os.makedirs(data, exist_ok=True)
    import shutil
    shutil.copy(os.path.join(OUT, "train", "twins.yaml"), os.path.join(data, "twins.yaml"))
    for f in ("twins.json", "baselines.json"):
        if os.path.exists(os.path.join(data, f)):
            os.remove(os.path.join(data, f))
    events = plan(seed, days)
    with open(os.path.join(YEAR, "events.json"), "w") as fh:
        json.dump(events, fh, indent=1)
    reads = twin_reads()
    env = {**os.environ, "COWPOKE_DATA": data, "COWPOKE_LOG": "warn"}
    log_path = os.path.join(YEAR, "log.jsonl")
    open(log_path, "w").close()
    cow = bc.cowpoke_bin()
    t_start = time.time()
    for day in range(days):
        path = os.path.join(YEAR, "days", f"day-{day:03d}.csv.gz")
        tripped = write_day(day, events, seed, path)
        res = json.loads(subprocess.run([cow, "twins", "replay", path], env=env, check=True, capture_output=True, text=True).stdout)[0]
        if day % keep_every:
            os.remove(path)
        twins = res["twins"]
        # A trip from wear brings maintenance the next morning.
        for m in tripped:
            for e in events:
                if e["cls"] == "wear" and e["target"] == m and e["d0"] <= day <= e["d1"]:
                    e["d1"], e["h1"], e["ended_by"] = day, 24, "trip"
                    for r in events:
                        if r.get("of") == e["id"]:
                            r["d0"] = r["d1"] = min(days - 1, day + 1)
        # The operator accepts a new normal: twins that held most of the day after a repair or a
        # recalibration. Recipe runs need nothing: the twins read the line's recipe number.
        accepted = []
        changes = [e for e in events if e["cls"] == "normal" and e["kind"] in ("repair", "recalibration") and e["d0"] == day]
        for e in changes:
            for n in expected_twins(e, reads, {}):
                s = twins.get(n)
                if s and s["steady"] and s["holding"] > 0.3 * s["steady"] and n not in accepted:
                    subprocess.run([cow, "twins", "reset", n], env=env, check=True, capture_output=True)
                    accepted.append(n)
        entry = {"day": day, "date": date_of(day).strftime("%Y-%m-%d"), "off": is_off(day), "tripped": tripped, "accepted": accepted,
                 "twins": {n: {k: s[k] for k in ("steady", "holding", "learned", "learned_total", "baseline", "rmse", "drift", "episodes", "blips", "nodata", "retrained", "inputs") if k in s} for n, s in twins.items()}}
        with open(log_path, "a") as fh:
            fh.write(json.dumps(entry) + "\n")
        with open(os.path.join(YEAR, "events.json"), "w") as fh:
            json.dump(events, fh, indent=1)
        if day % 7 == 0 or day == days - 1:
            el = time.time() - t_start
            print(f"day {day:3d} {entry['date']}  {el / 60:5.1f} min  ~{el / (day + 1) * (days - day - 1) / 60:4.0f} min left", flush=True)


# ---------------------------------------------------------------------------------------- report

def load():
    events = json.load(open(os.path.join(YEAR, "events.json")))
    log = [json.loads(l) for l in open(os.path.join(YEAR, "log.jsonl"))]
    return events, log


def score(events, log):
    reads = twin_reads()
    byday = {e["day"]: e for e in log}
    hits = []
    for e in events:
        if e["cls"] == "normal" or e["d0"] not in byday:
            continue
        t0 = e["d0"] * 24 + e["h0"]
        t1 = e["d1"] * 24 + e["h1"]
        inputs_now = {n: s.get("inputs", {}) for n, s in byday[e["d0"]]["twins"].items()}
        exp = expected_twins(e, reads, inputs_now)
        first, by = None, []
        for d in range(e["d0"], min(e["d1"], max(byday)) + 1):
            entry = byday.get(d)
            if not entry:
                continue
            for n, how in exp.items():
                s = entry["twins"].get(n, {})
                marks = {"hold": s.get("episodes", []), "nodata": s.get("nodata", []), "blip": s.get("blips", [])}[how]
                if how == "hold":
                    marks = marks + s.get("blips", [])
                for m in marks:
                    a = (m.get("from", m.get("t")) - DAY0) / 3600
                    b = (m.get("to", m.get("t")) - DAY0) / 3600
                    if b >= t0 - 0.05 and a <= t1 + 0.25:
                        when = max(a, t0)
                        if first is None or when < first:
                            first = when
                        by.append(n)
        trip = None
        for d in range(e["d0"], min(e["d1"], max(byday)) + 1):
            if e["target"] in byday.get(d, {}).get("tripped", {}):
                trip = d * 24 + byday[d]["tripped"][e["target"]]
                break
        hits.append({**{k: e[k] for k in ("id", "kind", "cls", "target", "d0", "d1")}, "observable": bool(exp), "detected": first is not None,
                     "delay_h": None if first is None else round(first - t0, 2), "by": sorted(set(by)),
                     "trip_h": None if trip is None else round(trip - t0, 2), "before_trip": None if trip is None else (first is not None and first < trip)})
    # Holds that no event (fault or normal) explains.
    windows = {}
    for e in events:
        for n in expected_twins(e, reads, {n: s.get("inputs", {}) for n, s in byday.get(e["d0"], {"twins": {}})["twins"].items()}):
            windows.setdefault(n, []).append((e["d0"] * 24 + e["h0"] - 0.25, e["d1"] * 24 + e["h1"] + 6))
    false = []
    for entry in log:
        for n, s in entry["twins"].items():
            for m in s.get("episodes", []):
                a = (m["from"] - DAY0) / 3600
                b = (m["to"] - DAY0) / 3600
                if not any(b >= w0 and a <= w1 for w0, w1 in windows.get(n, [])):
                    false.append({"day": entry["day"], "twin": n, "minutes": round((b - a) * 60, 1), "why": m["why"]})
    return hits, false


def report():
    events, log = load()
    hits, false = score(events, log)
    days = len(log)
    lines = [f"Buttercream plant, {days} days ({log[0]['date']} to {log[-1]['date']}), seed in events.json", ""]
    kinds = {}
    for x in hits:
        kinds.setdefault(x["kind"], []).append(x)
    lines.append(f"{'fault':22} {'events':>6} {'observable':>10} {'caught':>7} {'rate':>6} {'median delay':>13} {'before trip':>12}")
    tot = [0, 0, 0]
    for k, xs in sorted(kinds.items(), key=lambda kv: kv[0]):
        obs = [x for x in xs if x["observable"]]
        det = [x for x in obs if x["detected"]]
        delays = sorted(x["delay_h"] for x in det)
        med = delays[len(delays) // 2] if delays else None
        trips = [x for x in obs if x["trip_h"] is not None]
        bt = f"{sum(1 for x in trips if x['before_trip'])}/{len(trips)}" if trips else ""
        rate = f"{len(det) / len(obs):.0%}" if obs else "-"
        md = "" if med is None else (f"{med * 60:.0f} min" if med < 2 else f"{med:.1f} h" if med < 48 else f"{med / 24:.1f} d")
        lines.append(f"{k:22} {len(xs):6d} {len(obs):10d} {len(det):7d} {rate:>6} {md:>13} {bt:>12}")
        tot[0] += len(xs)
        tot[1] += len(obs)
        tot[2] += len(det)
    lines.append(f"{'all faults':22} {tot[0]:6d} {tot[1]:10d} {tot[2]:7d} {tot[2] / max(tot[1], 1):6.0%}")
    lines.append("")
    months = {}
    for f in false:
        mo = log[f["day"]]["date"][:7]
        months[mo] = months.get(mo, 0) + 1
    lines.append(f"Holds no event explains: {len(false)} in {days} days ({len(false) / days * 30:.1f} a month), "
                 f"{sum(f['minutes'] for f in false) / 60:.0f} h in all; median {sorted(f['minutes'] for f in false)[len(false) // 2] if false else 0:.0f} min")
    lines.append("  by month: " + ", ".join(f"{k} {v}" for k, v in sorted(months.items())))
    by_twin = {}
    for f in false:
        by_twin[f["twin"]] = by_twin.get(f["twin"], 0) + 1
    lines.append("  by twin: " + ", ".join(f"{k} {v}" for k, v in sorted(by_twin.items(), key=lambda kv: -kv[1])))
    acc = sum(len(e["accepted"]) for e in log)
    lines.append(f"Operator 'new normal' resets: {acc}")
    retr = sum(len(s.get("retrained", [])) for e in log for s in e["twins"].values())
    lines.append(f"Foreman retrains that changed a model: {retr}")
    missed = [x for x in hits if x["observable"] and not x["detected"]]
    lines.append("")
    lines.append("Missed (observable):")
    for x in missed:
        lines.append(f"  day {x['d0']:3d}-{x['d1']:3d} {x['kind']:20} {x['target']}")
    text = "\n".join(lines)
    with open(os.path.join(YEAR, "report.txt"), "w") as fh:
        fh.write(text + "\n")
    with open(os.path.join(YEAR, "scores.json"), "w") as fh:
        json.dump({"hits": hits, "false": false}, fh)
    print(text)


def plots():
    """out/year/year.png: what each twin did every day, detection by fault kind, false holds, fidelity."""
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    from matplotlib.patches import Patch

    events, log = load()
    hits, false = score(events, log)
    reads = twin_reads()
    names = list(MACHINES)
    days = len(log)
    fig = plt.figure(figsize=(16, 15))
    gs = fig.add_gridspec(3, 2, height_ratios=[2.2, 1.1, 1.0], hspace=0.35, wspace=0.25)
    ax = fig.add_subplot(gs[0, :])
    grid = np.zeros((len(names), days, 3)) + 1.0
    col = {"learning": (0.84, 0.96, 0.9), "warm": (0.95, 0.95, 0.96), "fault": (0.86, 0.15, 0.15), "normal": (0.96, 0.62, 0.04),
           "other": (0.42, 0.45, 0.5), "nodata": (0.15, 0.39, 0.92), "idle": (1, 1, 1)}
    fault_win, normal_win = {}, {}
    byday = {e["day"]: e for e in log}
    for e in events:
        exp = expected_twins(e, reads, {n: s.get("inputs", {}) for n, s in byday.get(e["d0"], {"twins": {}})["twins"].items()})
        for n in exp:
            (normal_win if e["cls"] == "normal" else fault_win).setdefault(n, []).append((e["d0"] * 24 + e["h0"] - 0.25, e["d1"] * 24 + e["h1"] + 6))
    for entry in log:
        d = entry["day"]
        for y, n in enumerate(names):
            s = entry["twins"].get(n, {})
            c = col["idle"] if not s.get("steady") else col["learning"] if s.get("baseline") else col["warm"]
            kinds = set()
            for m in s.get("episodes", []):
                a, b = (m["from"] - DAY0) / 3600, (m["to"] - DAY0) / 3600
                if any(b >= w0 and a <= w1 for w0, w1 in fault_win.get(n, [])):
                    kinds.add("fault")
                elif any(b >= w0 and a <= w1 for w0, w1 in normal_win.get(n, [])):
                    kinds.add("normal")
                else:
                    kinds.add("other")
            if s.get("nodata"):
                kinds.add("nodata")
            for k in ("fault", "nodata", "normal", "other"):
                if k in kinds:
                    c = col[k]
                    break
            grid[y, d] = c
    ax.imshow(grid, aspect="auto", interpolation="nearest")
    for entry in log:
        for m in entry["tripped"]:
            if m in names:
                ax.plot(entry["day"], names.index(m), marker="x", color="black", ms=4)
    ax.set_yticks(range(len(names)), names, fontsize=8)
    months = [d for d in range(days) if date_of(d).day == 1]
    ax.set_xticks(months, [date_of(d).strftime("%b") for d in months], fontsize=8)
    ax.set_title(f"A year of the buttercream plant: what each of Foreman's 22 twins did each day ({log[0]['date']} to {log[-1]['date']})", fontsize=11)
    ax.legend(handles=[Patch(color=col["learning"], label="learning"), Patch(color=col["fault"], label="held on a fault"),
                       Patch(color=col["normal"], label="held on a normal change"), Patch(color=col["other"], label="held, unexplained"),
                       Patch(color=col["nodata"], label="no data (comms)"), plt.Line2D([], [], marker="x", color="black", ls="", label="drive trip")],
              loc="upper center", bbox_to_anchor=(0.5, -0.04), ncol=6, fontsize=8, frameon=False)
    # Detection by kind
    ax2 = fig.add_subplot(gs[1, 0])
    kinds = {}
    for x in hits:
        kinds.setdefault(x["kind"], []).append(x)
    ks = sorted(kinds, key=lambda k: -sum(1 for x in kinds[k] if x["observable"] and x["detected"]) / max(1, sum(1 for x in kinds[k] if x["observable"])))
    for i, k in enumerate(ks):
        xs = kinds[k]
        obs = sum(1 for x in xs if x["observable"])
        det = sum(1 for x in xs if x["observable"] and x["detected"])
        ax2.barh(i, obs, color=(0.85, 0.85, 0.87))
        ax2.barh(i, det, color=(0.15, 0.39, 0.92))
        un = len(xs) - obs
        ax2.text(obs + 0.2, i, f"{det}/{obs}" + (f"  (+{un} no twin reads it)" if un else ""), va="center", fontsize=8)
    ax2.set_yticks(range(len(ks)), ks, fontsize=8)
    ax2.invert_yaxis()
    ax2.set_title("Faults caught / faults a twin could see", fontsize=10)
    ax2.tick_params(labelsize=8)
    # False holds by month
    ax3 = fig.add_subplot(gs[1, 1])
    mo = {}
    for f in false:
        k = log[f["day"]]["date"][:7]
        mo[k] = mo.get(k, 0) + 1
    allm = sorted({e["date"][:7] for e in log})
    ax3.bar(range(len(allm)), [mo.get(m, 0) for m in allm], color=(0.42, 0.45, 0.5))
    ax3.set_xticks(range(len(allm)), [datetime.strptime(m, "%Y-%m").strftime("%b") for m in allm], fontsize=8)
    ax3.set_title("Holds no event explains, by month (all 22 twins)", fontsize=10)
    ax3.tick_params(labelsize=8)
    # Fidelity
    ax4 = fig.add_subplot(gs[2, :])
    for n, c in (("AE-401", "#2563eb"), ("HX-302", "#dc2626"), ("AG-201", "#16a34a"), ("P-301", "#9333ea"), ("P-602", "#ea580c"), ("CV-412", "#0891b2")):
        ys = [(e["twins"].get(n, {}).get("rmse") or {}).get("current") for e in log]
        ax4.plot(range(days), [y if y is not None else np.nan for y in ys], lw=1, color=c, label=n)
    ax4.set_xticks(months, [date_of(d).strftime("%b") for d in months], fontsize=8)
    ax4.set_ylabel("current model error, A (RMSE)", fontsize=8)
    ax4.set_title("Model error over the year (resets after a new normal restart it)", fontsize=10)
    ax4.legend(fontsize=8, ncol=6)
    ax4.grid(alpha=0.2)
    path = os.path.join(YEAR, "year.png")
    fig.savefig(path, dpi=100, bbox_inches="tight")
    print("wrote", path)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("what", choices=["plan", "run", "report", "plots"])
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--days", type=int, default=365)
    a = ap.parse_args()
    if a.what == "plan":
        os.makedirs(YEAR, exist_ok=True)
        ev = plan(a.seed, a.days)
        json.dump(ev, open(os.path.join(YEAR, "events.json"), "w"), indent=1)
        kinds = {}
        for e in ev:
            kinds[e["kind"]] = kinds.get(e["kind"], 0) + 1
        print(len(ev), "events:", ", ".join(f"{k} {v}" for k, v in sorted(kinds.items())))
    elif a.what == "run":
        run(a.seed, a.days)
        report()
        plots()
    elif a.what == "plots":
        plots()
    else:
        report()


if __name__ == "__main__":
    main()
