#!/usr/bin/env python3
"""A buttercream frosting plant, day by day: Cowpoke config, synthetic production days, and Foreman's
twins learning from them one day at a time.

The plant is drawn in Ohmstead (schematic + P&ID). Its exports go in out/ (git-ignored: the drawing is a
real project used as test data):
  out/ohmstead-io.json      export_io_points: the PLC I/O
  out/ohmstead-twins.json   export_twins: every motor on a drive, what it turns, a pump's instruments

  python3 buttercream.py config            # out/cowpoke.yaml: the tags a site would have, real-world names
  python3 buttercream.py match             # cowpoke twins match + apply → out/train/twins.yaml
  python3 buttercream.py day 3             # out/days/day-03.csv.gz (2 s samples, 24 h)
  python3 buttercream.py train --days 28   # generate each day, replay it through the twins, report

Every day is generated from the same plant: batches in the premix (charge fats and glucose, feed sugar,
high-shear mix, transfer), a continuous line (timing pump through the scraped-surface heater and
cooler, the aerator, the filler feed pump, four conveyors), CIP at night, utilities around the clock,
and Sundays off. Faults and normal changes are spread over the weeks (EVENTS); the twins never see that
list, the report compares what they flagged with it.
"""

import argparse
import gzip
import json
import math
import os
import subprocess
import sys
from datetime import datetime, timezone

import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "out")
ROOT = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
DT = 2.0  # s
N = int(86400 / DT)
DAY0 = datetime(2026, 12, 1, tzinfo=timezone.utc).timestamp()

# --------------------------------------------------------------------------------------------- plant

# machine: (drive, motor hp, load model)
MACHINES = {
    "B-101": ("VFD-101", 25, "fan"),
    "DC-101": ("VFD-103", 7.5, "fan"),
    "SC-101": ("VFD-104", 3, "screw"),
    "P-110": ("VFD-111", 3, "pd_free"),
    "P-120": ("VFD-121", 3, "pd_free"),
    "P-130": ("VFD-131", 2, "pd_free"),
    "AG-201": ("VFD-201", 30, "mixer"),
    "AG-202": ("VFD-202", 5, "scraper"),
    "P-201": ("VFD-203", 5, "pd_free"),
    "P-301": ("VFD-301", 7.5, "pd"),
    "HX-301": ("VFD-302", 15, "sshe_heat"),
    "HX-302": ("VFD-303", 20, "sshe_cool"),
    "AE-401": ("VFD-401", 25, "aerator"),
    "P-401": ("VFD-402", 3, "pd"),
    "CV-411": ("VFD-411", 1, "conveyor"),
    "CV-412": ("VFD-412", 1, "conveyor"),
    "CV-413": ("VFD-413", 1, "conveyor"),
    "CV-414": ("VFD-414", 1, "conveyor"),
    "P-501": ("VFD-501", 10, "centrifugal"),
    "P-502": ("VFD-502", 5, "centrifugal"),
    "P-601": ("VFD-601", 5, "centrifugal"),
    "P-602": ("VFD-602", 10, "centrifugal"),
}
# Drives read through the PLC (legacy tag names) vs. their own Modbus service.
VIA_PLC = {d for m, (d, _, _) in MACHINES.items() if d < "VFD-400"}

def is_sunday(day):
    return datetime.fromtimestamp(DAY0 + day * 86400, timezone.utc).weekday() == 6


# Faults and normal changes. day, start/end in hours (end may pass 24 within the day only), what.
EVENTS = [
    {"day": 6, "from": 14.2, "to": 14.21, "machines": "all drives", "kind": "supply sag",
     "what": "A 0.4 s, 25 % supply sag: every drive's DC bus dips at once."},
    {"day": 9, "from": 6.0, "to": 22.0, "machines": "P-301 HX-301 HX-302 AE-401 P-401 AG-201", "kind": "recipe change (normal)",
     "what": "Chocolate buttercream days 9-11: thicker product. Normal operation the twins have not seen."},
    {"day": 10, "from": 6.0, "to": 22.0, "machines": "P-301 HX-301 HX-302 AE-401 P-401 AG-201", "kind": "recipe change (normal)", "what": "Chocolate, day 2."},
    {"day": 11, "from": 6.0, "to": 22.0, "machines": "P-301 HX-301 HX-302 AE-401 P-401 AG-201", "kind": "recipe change (normal)", "what": "Chocolate, day 3."},
    *[{"day": d, "from": 6.0, "to": 22.0, "machines": "HX-302", "kind": "cooler freeze-up",
       "what": "Glycol too cold: product freezes on the cooler wall; rotor torque creeps up a little more each day."}
      for d in range(15, 20) if not is_sunday(d)],
    {"day": 20, "from": 6.0, "to": 8.0, "machines": "HX-302", "kind": "repair (normal)", "what": "Glycol setpoint corrected over the weekend: the cooler runs as before."},
    *[{"day": d, "from": 6.0, "to": 22.0, "machines": "P-301", "kind": "lobe wear",
       "what": "Timing pump lobes wearing: more slip, so less flow and pressure at the same speed."}
      for d in range(16, 25) if not is_sunday(d)],
    {"day": 25, "from": 6.0, "to": 9.0, "machines": "P-301", "kind": "repair (normal)", "what": "Timing pump lobes replaced: flow and pressure back to new."},
    {"day": 18, "from": 10.33, "to": 11.0, "machines": "P-401", "kind": "filler blockage",
     "what": "Filler nozzle partly blocked for 40 min: P-401 discharge pressure and current up."},
    *[{"day": d, "from": 6.0, "to": 22.0, "machines": "AG-201", "kind": "mixer bearing",
       "what": "High-shear mixer bearing failing over a week: current higher and rougher at the same speed."}
      for d in range(22, 28) if not is_sunday(d)],
    {"day": 25, "from": 15.08, "to": 15.2, "machines": "CV-412", "kind": "conveyor jam",
     "what": "Lid conveyor jams: current climbs until the drive trips on overload."},
    {"day": 26, "from": 0.0, "to": 24.0, "machines": "P-602", "kind": "air in glycol loop",
     "what": "Air in the glycol loop: P-602 current lower and ragged at the same speed."},
    *[{"day": d, "from": at, "to": at + 0.75, "machines": "P-301 HX-301 HX-302 AE-401 P-401", "kind": "thick batch slug",
       "what": "A premix batch short of water goes to the line: a slug of thick product passes the timing pump, "
               "heater, cooler, aerator and filler pump in turn, a few minutes apart."}
      for d, at in ((4, 11.0), (23, 14.5))],
]
# Minutes after the slug reaches the timing pump that it reaches each machine down the line.
SLUG_DELAY = {"P-301": 0.0, "HX-301": 3.0, "HX-302": 6.0, "AE-401": 9.0, "P-401": 12.0}
FAULT_DAYS_FIXED = {"HX-302": 20, "AG-201": 28}  # repaired before these days


def fla(hp):
    return hp * 746 / (math.sqrt(3) * 460 * 0.88 * 0.9)


# --------------------------------------------------------------------------------------------- config

def make_config():
    io = json.load(open(os.path.join(OUT, "ohmstead-io.json")))
    tw = json.load(open(os.path.join(OUT, "ohmstead-twins.json")))
    plc_tags, n = [], 0
    for p in io["points"]:
        name = p["name"]
        # Area 400 was programmed years ago: its instruments keep their old PLC names.
        if p["kind"] == "analog" and p["direction"] == "in" and "40" in name.split(".")[0][3:5]:
            base = name.split(".")[0].replace("-", "")
            name = f"Skid4.{base}_PV" + ("" if "." not in p["name"] else ("_N" if p["name"].endswith("-") else ""))
        units = (p.get("range") or {}).get("units")
        tag = {"name": name, "datatype": "float32" if p["kind"] == "analog" else "bool", "cip_tag": name.replace(".", "_").replace("-", "_").replace("+", "P")}
        if units:
            tag["units"] = units
        plc_tags.append(tag)
        n += 1
    # The line's recipe number, set by the HMI when a product run starts (1 vanilla, 2 chocolate, ...).
    plc_tags.append({"name": "Line_RecipeNo", "datatype": "int16", "cip_tag": "Line_RecipeNo"})
    services = [{"id": "cp1-plc", "name": "CP-1 CompactLogix", "kind": "eip", "mode": "client",
                 "settings": {"host": "10.10.1.10", "poll_ms": 500}, "tags": plc_tags}]
    for t in tw["twins"]:
        d = t["drive"]
        num = d.split("-")[1]
        if d in VIA_PLC:
            for nm, u in (("OutFreq", "Hz"), ("FreqCmd", "Hz"), ("OutCurrent", "A"), ("DCBusVolts", "V"), ("OutVolts", "V")):
                plc_tags.append({"name": f"VFD_{num}_{nm}", "datatype": "float32", "units": u, "cip_tag": f"VFD_{num}_{nm}"})
            plc_tags.append({"name": f"VFD_{num}_Faulted", "datatype": "bool", "cip_tag": f"VFD_{num}_Faulted"})
        else:
            services.append({"id": f"vfd-{num}", "name": f"{d} PowerFlex 525", "kind": "modbus", "mode": "client",
                             "settings": {"host": f"10.10.4.{int(num) % 256}", "poll_ms": 500},
                             "tags": [{"name": nm, "datatype": "float32", "register": r, "units": u}
                                      for r, (nm, u) in enumerate((("OutputFreq", "Hz"), ("OutputCurrent", "A"), ("DCBusVoltage", "V"), ("OutputVoltage", "V"), ("FaultCode", None)))]})
            for tg in services[-1]["tags"]:
                tg["register"] *= 2
                if tg["units"] is None:
                    del tg["units"]
                    tg["datatype"] = "uint16"
    import yaml
    path = os.path.join(OUT, "cowpoke.yaml")
    with open(path, "w") as fh:
        fh.write("# The buttercream plant's Cowpoke services, as a site would have them (generated by buttercream.py).\n")
        yaml.safe_dump({"services": services}, fh, sort_keys=False, width=200)
    print(f"wrote {path}: {len(services)} services, {sum(len(s['tags']) for s in services)} tags")


def cowpoke_bin():
    if os.environ.get("COWPOKE_BIN"):
        return os.environ["COWPOKE_BIN"]
    for p in ("target/release/cowpoke", "target/debug/cowpoke"):
        if os.path.exists(os.path.join(ROOT, p)):
            return os.path.join(ROOT, p)
    sys.exit("build cowpoke first (cargo build --release)")


def run_match():
    env = {**os.environ, "COWPOKE_CONFIG": os.path.join(OUT, "cowpoke.yaml"), "COWPOKE_DATA": os.path.join(OUT, "train")}
    exp = os.path.join(OUT, "ohmstead-twins.json")
    prop = json.loads(subprocess.run([cowpoke_bin(), "twins", "match", exp], env=env, check=True, capture_output=True, text=True).stdout)
    print("match:", prop["summary"])
    for t in prop["twins"]:
        bad = [f"{r['role']}={r['status']}" for r in t["roles"] if r["status"] not in ("exact", "likely")]
        if bad:
            print(f"  {t['name']}: {' '.join(bad)}")
    os.makedirs(env["COWPOKE_DATA"], exist_ok=True)
    print(subprocess.run([cowpoke_bin(), "twins", "apply", exp], env=env, check=True, capture_output=True, text=True).stdout.strip())
    with open(os.path.join(OUT, "match.json"), "w") as fh:
        json.dump(prop, fh, indent=1)


# --------------------------------------------------------------------------------------------- one day

def column_names():
    """Cowpoke `service/tag` keys for every simulated signal, as in out/cowpoke.yaml."""
    cols = {}
    for m, (d, _, _) in MACHINES.items():
        num = d.split("-")[1]
        if d in VIA_PLC:
            cols[(m, "freq")] = f"cp1-plc/VFD_{num}_OutFreq"
            cols[(m, "cmd")] = f"cp1-plc/VFD_{num}_FreqCmd"
            cols[(m, "current")] = f"cp1-plc/VFD_{num}_OutCurrent"
            cols[(m, "dc_bus")] = f"cp1-plc/VFD_{num}_DCBusVolts"
            cols[(m, "out_voltage")] = f"cp1-plc/VFD_{num}_OutVolts"
        else:
            cols[(m, "freq")] = f"vfd-{num}/OutputFreq"
            cols[(m, "current")] = f"vfd-{num}/OutputCurrent"
            cols[(m, "dc_bus")] = f"vfd-{num}/DCBusVoltage"
            cols[(m, "out_voltage")] = f"vfd-{num}/OutputVoltage"
            cols[(m, "fault")] = f"vfd-{num}/FaultCode"
    for tag, key in (("PT-301", "cp1-plc/PT-301"), ("FT-301", "cp1-plc/FT-301.AO+"), ("TT-301", "cp1-plc/TT-301"), ("TT-302", "cp1-plc/TT-302"),
                     ("TT-303", "cp1-plc/TT-303"), ("PT-401", "cp1-plc/Skid4.PT401_PV"), ("LT-401", "cp1-plc/Skid4.LT401_PV"),
                     ("LT-110", "cp1-plc/LT-110"), ("LT-120", "cp1-plc/LT-120"), ("LT-130", "cp1-plc/LT-130"),
                     ("FT-501", "cp1-plc/FT-501.AO+"), ("TT-501", "cp1-plc/TT-501"), ("LT-501", "cp1-plc/LT-501"),
                     ("TT-601", "cp1-plc/TT-601"), ("TT-602", "cp1-plc/TT-602"),
                     ("WT-201", "cp1-plc/WT-201.AO+"), ("TT-201", "cp1-plc/TT-201"), ("LT-210", "cp1-plc/LT-210"), ("TT-210", "cp1-plc/TT-210"),
                     ("DT-401", "cp1-plc/Skid4.DT401_PV"), ("TT-401", "cp1-plc/Skid4.TT401_PV")):
        cols[(tag, "pv")] = key
    cols[("LINE", "recipe")] = "cp1-plc/Line_RecipeNo"
    return cols


def active(day, machine, kind):
    for e in EVENTS:
        if e["day"] == day and e["kind"] == kind and (machine in e["machines"].split() or e["machines"] == "all drives"):
            return e
    return None


def simulate_day(day, seed=0):
    rng = np.random.default_rng(1000 * seed + day)
    t = np.arange(N) * DT
    h = t / 3600
    sunday = is_sunday(day)
    sig = {}

    def win(a, b):
        return (h >= a) & (h < b)

    # --- schedule (speed references, Hz) ---
    ref = {m: np.zeros(N) for m in MACHINES}
    prod = np.zeros(N, bool) if sunday else win(6.0, 22.0)
    ref["P-601"][:] = 50.0
    ref["P-602"][:] = 55.0 if not sunday else 40.0
    batch_min = np.full(N, -1.0)
    if not sunday:
        ref["DC-101"][win(5.5, 22.0)] = 50.0
        for b in range(16):
            s = 5.5 + b  # a batch an hour from 05:30
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
        recipe_choc = 9 <= day <= 11
        ref["P-301"][line] = 33.0 if recipe_choc else 36.0
        ref["HX-301"][line] = 40
        ref["HX-302"][line] = 45
        ref["AE-401"][win(6.15, 22.0)] = 50
        for c, f in (("CV-411", 50), ("CV-412", 40), ("CV-413", 40), ("CV-414", 50)):
            r = np.where(win(6.2, 22.0), f, 0.0)
            # The filler stops the conveyors for 30 s every ~20 min (changeovers, lid magazine).
            stop = (t % 1200) < 30
            r[stop] = 0
            ref[c] = r
        # CIP 22:15-23:55: pre-rinse, caustic, rinse, acid, final rinse.
        for a, b, f in ((22.25, 22.42, 50), (22.42, 23.08, 55), (23.08, 23.25, 50), (23.25, 23.58, 52), (23.58, 23.92, 50)):
            ref["P-501"][win(a, b)] = f
            ref["P-502"][win(a, b)] = 48
    else:
        recipe_choc = False
        line = np.zeros(N, bool)
        # Sunday sanitation: a long CIP in the morning.
        for a, b, f in ((8.0, 8.3, 50), (8.3, 9.5, 55), (9.5, 9.8, 50), (9.8, 10.5, 52), (10.5, 10.9, 50)):
            ref["P-501"][win(a, b)] = f
            ref["P-502"][win(a, b)] = 48

    # P-401 runs on hopper level: speed follows the filler's demand.
    lt401 = 55 + 10 * np.sin(2 * np.pi * t / 1500 + day) + rng.normal(0, 0.5, N)
    if not sunday:
        p401 = np.clip(34 + 0.5 * (lt401 - 55), 25, 45)
        ref["P-401"][win(6.15, 22.0)] = p401[win(6.15, 22.0)]

    # --- process state ---
    walk = np.cumsum(rng.normal(0, 0.03 / math.sqrt(N), N))  # a few % over a day
    walk -= np.linspace(0, walk[-1], N)
    mu_line = (1.38 if recipe_choc else 1.0) * (1 + 0.03 * np.sin(2 * np.pi * t / 7200) + walk)
    # Batch: charging fats is thin; sugar makes it thick; mixed batter is at full viscosity.
    bm = batch_min
    fill = np.clip(0.15 + np.where(bm < 0, 0, np.minimum(bm, 24) / 24 * 0.85), 0, 1)
    mu_batch = np.where(bm < 9, 0.35, np.where(bm < 24, 0.35 + (bm - 9) / 15 * 0.65, 1.0)) * (1.25 if recipe_choc else 1.0)
    # Hot water and glycol supplies cycle, so the product temperatures out of the heater and the
    # cooler move a degree or two, and the product's viscosity in them with it.
    tt301 = np.where(line, 74 + 1.2 * np.sin(2 * np.pi * t / 3600 + day) + rng.normal(0, 0.2, N), 30.0)
    tt303 = np.where(line, 18 + 1.5 * np.sin(2 * np.pi * t / 5400 + 0.7 * day) + rng.normal(0, 0.2, N), 22.0)
    # Overrun varies: the aerated product's density.
    dt401 = np.where(line, 0.75 + 0.05 * np.sin(2 * np.pi * t / 4000 + day) + rng.normal(0, 0.003, N), 0.0)
    # Premix: fats come in warm and the sugar cools the batch; the load cell weighs it.
    tt201 = np.where(bm < 0, 22.0, np.where(bm < 9, 38.0, np.maximum(26.0, 38.0 - (bm - 9) / 15 * 12))) + rng.normal(0, 0.2, N)
    mu_batch = mu_batch * np.exp(-0.03 * (tt201 - 28))
    wt201 = 1400 * fill * (bm >= 0) + 120 + rng.normal(0, 3, N)
    freeze = 0.0
    e = active(day, "HX-302", "cooler freeze-up")
    if e:
        days_in = day - 15
        prog = np.clip((h - 6) / 16, 0, 1)
        freeze = (0.06 + 0.07 * days_in) * prog
        tt303 = tt303 - 3 * freeze
    wear = 0.0
    e = active(day, "P-301", "lobe wear")
    if e:
        wear = 0.02 * (day - 15)
    block401 = np.zeros(N)
    e = active(day, "P-401", "filler blockage")
    if e:
        block401[win(e["from"], e["to"])] = 1.0
    bearing = 0.0
    e = active(day, "AG-201", "mixer bearing")
    if e:
        bearing = 0.04 + 0.025 * (day - 22)
    air602 = active(day, "P-602", "air in glycol loop") is not None
    slug_e = active(day, "P-301", "thick batch slug")

    def slug(m):
        """Viscosity multiplier at machine m: 1, or up to 1.45 while the thick slug passes (25 min, eased)."""
        if not slug_e or m not in SLUG_DELAY:
            return 1.0
        x = (h - slug_e["from"] - SLUG_DELAY[m] / 60) / (25 / 60)
        return 1 + 0.45 * np.where((x > 0) & (x < 1), np.sin(np.pi * np.clip(x, 0, 1)), 0)
    jam = active(day, "CV-412", "conveyor jam")
    sag = np.ones(N)
    e = active(day, "B-101", "supply sag")
    if e:
        sag[win(e["from"], e["from"] + 0.4 / 3600 + DT / 3600)] = 0.75

    # --- each drive ---
    tripped = {}
    for m, (d, hp, model) in MACHINES.items():
        f = np.zeros(N)
        acc = 60 / 6.0 * DT  # 6 s ramps
        r_ = ref[m]
        cur = 0.0
        for i in range(N):
            tgt = r_[i]
            cur = min(tgt, cur + acc) if tgt > cur else max(tgt, cur - acc)
            f[i] = cur
        r = f / 60.0
        T = np.zeros(N)
        runm = f > 0.5
        if model == "fan":
            T = 0.85 * r ** 2
        elif model == "centrifugal":
            T = 0.8 * r ** 2
            if m == "P-602" and air602:
                T = T * (0.82 + 0.06 * rng.normal(0, 1, N))
        elif model == "screw":
            T = 0.35 + 0.25 * fill + 0.05 * rng.normal(0, 1, N)
        elif model == "pd_free":
            T = 0.25 + 0.45 * r * (1.6 if m == "P-130" else 1.0)
        elif model == "scraper":
            T = 0.2 + 0.5 * fill * mu_batch * (0.5 + r)
        elif model == "mixer":
            T = fill * (0.12 + mu_batch * (0.3 * r + 0.55 * r ** 2)) * (1 + bearing) + bearing * 0.5 * rng.normal(0, 1, N) * runm
        elif model == "sshe_heat":
            mu_eff = mu_line * slug(m) * np.exp(-0.04 * (tt301 - 74))
            T = 0.22 + 0.55 * mu_eff * r * 0.8
        elif model == "sshe_cool":
            mu_cool = mu_line * slug(m) * np.exp(-0.06 * (tt303 - 18))
            T = 0.25 + 0.6 * (mu_cool + freeze * 2.2) * r * 0.85
        elif model == "aerator":
            T = 0.25 + 0.5 * r ** 2 * (np.maximum(dt401, 0.5) / 0.75) + 0.15 * mu_line * slug(m) * r
        elif model == "conveyor":
            load = 0.15 * (rng.random(N) < 0.3)
            T = 0.35 + load
            if m == "CV-412" and jam:
                j = win(jam["from"], jam["to"])
                ramp = np.clip((h - jam["from"]) / (jam["to"] - jam["from"]), 0, 1)
                T = np.where(j, T + 1.6 * ramp, T)
        elif model == "pd":
            if m == "P-301":
                mu = mu_line * slug(m)
                q = 6.0 * r * (1 - 0.03 / mu - wear)  # m3/h
                dp = 0.3 + 2.4 * mu * q / 6.0 + 0.4 * (q / 6.0) ** 2  # bar
                sig[("FT-301", "pv")] = np.where(runm, q, 0) + rng.normal(0, 0.03, N)
                sig[("PT-301", "pv")] = np.where(runm, dp, 0.05) + rng.normal(0, 0.02, N)
            else:
                q = 4.0 * r
                dp = 0.5 + 1.8 * mu_line * slug(m) * r + 0.6 * r ** 2 + 2.2 * block401
                sig[("PT-401", "pv")] = np.where(runm, dp, 0.1) + rng.normal(0, 0.02, N)
            T = 0.12 + 0.28 * dp
        T = np.where(runm, T, 0.0)
        # Overload trip (I²t on the drive): once tripped, the drive stays faulted for the day.
        I_fla = fla(hp)
        I = I_fla * np.sqrt(0.33 ** 2 + (0.944 * T) ** 2) * runm
        heat = 0.0
        trip_at = None
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
        p_pu = T * r
        vdc = 650 * sag - 14 * p_pu + rng.normal(0, 0.8, N)
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
    sig[("TT-601", "pv")] = 82 + rng.normal(0, 0.3, N)
    sig[("TT-602", "pv")] = (-2 if active(day, "HX-302", "cooler freeze-up") else 1) + rng.normal(0, 0.2, N)
    sig[("LINE", "recipe")] = np.full(N, 2.0 if recipe_choc else 1.0)
    return sig, tripped


def write_day(day, path, seed=0):
    sig, tripped = simulate_day(day, seed)
    cols = column_names()
    keys = [k for k in cols if k in sig]
    t0 = DAY0 + day * 86400
    arr = np.column_stack([np.arange(N) * DT + t0] + [sig[k] for k in keys])
    fmt = ["%.1f"] + ["%.3f"] * len(keys)
    with gzip.open(path, "wt", compresslevel=3) as fh:
        fh.write(",".join(["t"] + [cols[k] for k in keys]) + "\n")
        np.savetxt(fh, arr, fmt=fmt, delimiter=",")
    return tripped


# --------------------------------------------------------------------------------------------- training

def train(days, seed=0, keep=False):
    data = os.path.join(OUT, "train")
    if not os.path.exists(os.path.join(data, "twins.yaml")):
        sys.exit("run `match` first")
    if os.path.exists(os.path.join(data, "twins.json")) and not keep:
        os.remove(os.path.join(data, "twins.json"))
    os.makedirs(os.path.join(OUT, "days"), exist_ok=True)
    env = {**os.environ, "COWPOKE_DATA": data, "COWPOKE_LOG": "warn"}
    log = []
    for day in range(days):
        path = os.path.join(OUT, "days", f"day-{day:02d}.csv.gz")
        tripped = write_day(day, path, seed)
        res = json.loads(subprocess.run([cowpoke_bin(), "twins", "replay", path], env=env, check=True, capture_output=True, text=True).stdout)[0]
        log.append({"day": day, "tripped": tripped, "twins": res["twins"], "plant": res.get("plant", [])})
        with open(os.path.join(OUT, "train-log.json"), "w") as fh:
            json.dump(log, fh)
        report_day(log[-1])
    report(log)


def hours(ts, day):
    return (ts - DAY0 - day * 86400) / 3600


def episodes_of(entry):
    out = []
    for name, s in entry["twins"].items():
        for e in s["episodes"]:
            out.append({"twin": name, "from": hours(e["from"], entry["day"]), "to": hours(e["to"], entry["day"]), "z": e["peak_z"], "why": e["why"]})
    return out


def explain(ep, day):
    """The ground-truth event an episode overlaps, if any."""
    for e in EVENTS:
        if e["day"] != day:
            continue
        ms = e["machines"].split() if e["machines"] != "all drives" else list(MACHINES)
        if ep["twin"] in ms and ep["from"] <= e["to"] + 0.25 and ep["to"] >= e["from"] - 0.05:
            return e
    return None


def report_day(entry):
    day = entry["day"]
    tw = entry["twins"]
    base = sum(1 for s in tw.values() if s["baseline"])
    learned = sum(s["learned"] for s in tw.values())
    eps = episodes_of(entry)
    hits = [(ep, explain(ep, day)) for ep in eps]
    real = [x for x in hits if x[1] and "normal" not in x[1]["kind"]]
    normal = [x for x in hits if x[1] and "normal" in x[1]["kind"]]
    retrained = [(n, r) for n, s in tw.items() for r in s.get("retrained", [])]
    other = [x for x in hits if not x[1]]
    events = [e for e in EVENTS if e["day"] == day]
    wd = datetime.fromtimestamp(DAY0 + day * 86400, timezone.utc).strftime("%a %d %b")
    line = f"day {day:2d} {wd}: {base:2d}/22 twins past warm-up, learned {learned:5d} samples"
    if events:
        line += " | events: " + ", ".join(sorted({f"{e['kind']} ({e['machines'] if e['machines'] != 'all drives' else 'all'})" for e in events}))
    print(line)
    for ep, e in real:
        print(f"    caught  {ep['twin']:7} {ep['from']:5.2f}-{ep['to']:5.2f} h  peak {ep['z']:5.1f}σ  {ep['why']}  ← {e['kind']}")
    if normal:
        kinds = sorted({e["kind"] for _, e in normal})
        print(f"    held on {', '.join(kinds)}: {', '.join(sorted({ep['twin'] for ep, _ in normal}))} ({len(normal)} episodes)")
    for n, r in retrained:
        print(f"    Foreman retrained {n} {r['model']}: {', '.join(r['inputs']) or 'speed only'}  error {r['rmse_before']} → {r['rmse_after']}")
    for ep, _ in other:
        print(f"    unexplained {ep['twin']:7} {ep['from']:5.2f}-{ep['to']:5.2f} h  peak {ep['z']:5.1f}σ  {ep['why']}")
    for name, s in tw.items():
        for b in s.get("blips", []):
            print(f"    blip    {name:7} {hours(b['t'], day):5.2f} h  {b['model']} {b['z']:+.0f}σ for a sample or two")
    for m, at in entry["tripped"].items():
        print(f"    drive tripped: {m} at {at:.2f} h")
    for p in entry.get("plant", []):
        if p["kind"] != "machine":
            print(f"    PLANT   {hours(p['t'], day):5.2f} h  {p['kind']:8} {p['summary']}")


def report(log):
    print("\nevents vs. the twins:")
    for e in EVENTS:
        entry = next((x for x in log if x["day"] == e["day"]), None)
        if not entry:
            continue
        ms = e["machines"].split() if e["machines"] != "all drives" else list(MACHINES)
        eps = [ep for ep in episodes_of(entry) if explain(ep, e["day"]) is e]
        caught = sorted({ep["twin"] for ep in eps})
        first = min((ep["from"] for ep in eps), default=None)
        delay = f"{(first - e['from']) * 60:.0f} min after it began" if first is not None else ""
        blips = sorted({n for n, s in entry["twins"].items() if any(e["from"] - 0.02 <= hours(b["t"], e["day"]) <= e["to"] + 0.02 for b in s.get("blips", []))})
        seen = f"caught by {', '.join(caught)}" if caught else (f"blips on {len(blips)} twins" if blips else "not caught")
        print(f"  day {e['day']:2d} {e['kind']:24} {seen:40} {delay}")
    with open(os.path.join(OUT, "events.json"), "w") as fh:
        json.dump(EVENTS, fh, indent=1)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("what", choices=["config", "match", "day", "train", "plot"])
    ap.add_argument("arg", nargs="?", type=int)
    ap.add_argument("--days", type=int, default=28)
    ap.add_argument("--seed", type=int, default=0)
    a = ap.parse_args()
    if a.what == "config":
        make_config()
    elif a.what == "match":
        run_match()
    elif a.what == "day":
        os.makedirs(os.path.join(OUT, "days"), exist_ok=True)
        p = os.path.join(OUT, "days", f"day-{a.arg:02d}.csv.gz")
        print("tripped:", write_day(a.arg, p, a.seed), "→", p)
    elif a.what == "plot":
        plot()
    else:
        train(a.days, a.seed)
        plot()



def plot(path=None):
    """Machines × days: what each twin did, with the events and Foreman's retrains."""
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    from matplotlib.patches import Patch

    log = json.load(open(os.path.join(OUT, "train-log.json")))
    names = list(MACHINES)
    days = len(log)
    fig, (ax, ax2) = plt.subplots(2, 1, figsize=(15, 11), gridspec_kw={"height_ratios": [3.2, 1]})
    colors = {"learning": "#d1fae5", "warm": "#f3f4f6", "caught": "#dc2626", "normal": "#f59e0b", "other": "#6b7280", "idle": "#ffffff"}
    for x, entry in enumerate(log):
        day = entry["day"]
        eps = episodes_of(entry)
        for y, n in enumerate(names):
            s = entry["twins"].get(n, {})
            c = colors["idle"] if not s.get("steady") else colors["learning"] if s.get("baseline") else colors["warm"]
            mine = [(ep, explain(ep, day)) for ep in eps if ep["twin"] == n]
            if any(e and "normal" not in e["kind"] for _, e in mine):
                c = colors["caught"]
            elif any(e and "normal" in e["kind"] for _, e in mine):
                c = colors["normal"]
            elif mine:
                c = colors["other"]
            ax.add_patch(plt.Rectangle((x, y), 0.94, 0.86, color=c))
            if any(b for b in s.get("blips", [])):
                ax.plot(x + 0.47, y + 0.43, marker="v", color="#1d4ed8", ms=5)
            if s.get("retrained"):
                ax.plot(x + 0.82, y + 0.2, marker="*", color="#7c3aed", ms=8)
            if n in entry["tripped"]:
                ax.text(x + 0.47, y + 0.62, "trip", ha="center", va="center", fontsize=6, color="white" if c == colors["caught"] else "#111")
    ax.set_xlim(0, days)
    ax.set_ylim(len(names), 0)
    ax.set_yticks([i + 0.43 for i in range(len(names))], [f"{n}  {MACHINES[n][2].replace('_', ' ')}" for n in names], fontsize=8)
    ax.set_xticks([i + 0.47 for i in range(days)], [datetime.fromtimestamp(DAY0 + d * 86400, timezone.utc).strftime("%a\n%d") for d in range(days)], fontsize=7)
    ax.set_title("Buttercream plant, 28 days: Foreman's twins learning one day at a time", fontsize=12)
    ax.legend(handles=[Patch(color=colors["warm"], label="warming up"), Patch(color=colors["learning"], label="learning, nothing off"),
                       Patch(color=colors["caught"], label="held on a real fault"), Patch(color=colors["normal"], label="held on a normal change (recipe, repair)"),
                       Patch(color=colors["other"], label="held, not explained"),
                       plt.Line2D([], [], marker="*", color="#7c3aed", ls="", label="Foreman retrained its regression"),
                       plt.Line2D([], [], marker="v", color="#1d4ed8", ls="", label="blip (one-sample spike)")],
              loc="upper center", bbox_to_anchor=(0.5, -0.07), ncol=4, fontsize=8, frameon=False)
    # Fidelity: each twin's current-model error as a share of its typical current, over the days.
    for n, col in (("AE-401", "#2563eb"), ("HX-302", "#dc2626"), ("AG-201", "#16a34a"), ("P-301", "#9333ea"), ("CV-412", "#ea580c")):
        ys = []
        for entry in log:
            s = entry["twins"].get(n, {})
            r = (s.get("rmse") or {}).get("current")
            ys.append(r)
        ax2.plot(range(days), [y if y is not None else float("nan") for y in ys], marker=".", color=col, label=n)
    ax2.set_ylabel("current model error (A, RMSE)", fontsize=8)
    ax2.set_xticks(range(days), [str(d) for d in range(days)], fontsize=7)
    ax2.set_xlabel("day", fontsize=8)
    ax2.grid(alpha=0.2)
    ax2.legend(fontsize=8, ncol=5)
    fig.tight_layout()
    path = path or os.path.join(OUT, "timeline.png")
    fig.savefig(path, dpi=105)
    print("wrote", path)

if __name__ == "__main__":
    main()
