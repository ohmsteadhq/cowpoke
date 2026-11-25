#!/usr/bin/env python3
"""VFD + centrifugal pump simulator: synthetic process data for teaching Foreman pump faults.

One VFD (V/Hz) drives an induction motor turning a centrifugal pump that moves a slurry from a
suction tank into a discharge line. Each scenario is a start, a run and maybe a stop, with one
fault (or none) that starts part-way through. The signals are what a drive and two pressure
transmitters actually report:

  freq_hz        VFD output frequency
  out_voltage_v  VFD output voltage (fundamental RMS)
  dc_bus_v       VFD DC bus voltage
  current_a      motor current (RMS)
  suction_bar    suction pressure, gauge
  discharge_bar  discharge pressure, gauge
  flow_m3h       flow (many skids don't have a flowmeter; Foreman should not depend on it)
  vfd_state      STOPPED / ACCEL / RUN / DECEL / FAULT
  fault_code     drive fault code once tripped (OC, OL, OV, UV, PHL), else blank
  label          the injected fault once it has started, else "normal"

The physics is simple but consistent: pump and system curves with the affinity laws, NPSH and
cavitation, motor torque from shaft power and inertia, magnetizing current, flux weakening on a
low bus, DC bus droop and regen, I^2t overload and the drive's trip levels. Faults change the
plant, not the signals, so every symptom follows from the model.

  python3 pump_sim.py --list
  python3 pump_sim.py --catalog --out out/catalog          # one clean run of every scenario + PNG
  python3 pump_sim.py --dataset 500 --seed 1 --out out/ds  # randomised runs for training
  python3 pump_sim.py --scenario cavitation --cowpoke out/cavitation.yaml   # replay in Cowpoke
"""

import argparse
import csv
import dataclasses
import json
import math
import os
import random
from dataclasses import dataclass, field

G = 9.81
P_ATM = 1.013  # bar abs
P_VAPOR = 0.03  # bar abs, water-based slurry ~25 °C


# --------------------------------------------------------------------------------------------
# Plant


@dataclass
class Plant:
    # Supply and drive
    v_line: float = 460.0
    f_base: float = 60.0
    accel_s: float = 10.0  # 0 → base frequency
    decel_s: float = 10.0
    f_min: float = 20.0
    # Motor: 40 hp, 4-pole
    p_rated_kw: float = 30.0
    fla: float = 48.0
    imag_pu: float = 0.33  # magnetizing current, pu of FLA
    slip_pu: float = 0.015
    j_pu: float = 2.0  # torque pu needed per pu-speed/s of acceleration
    # Pump at base speed
    h0: float = 60.0  # shutoff head, m
    q_bep: float = 120.0  # m3/h
    h_bep: float = 45.0  # m
    eta_h: float = 0.80
    # System and material
    h_static: float = 15.0  # m
    sg: float = 1.2  # slurry specific gravity
    tank_level_m: float = 3.0
    tank_area_m2: float = 1.5
    suction_loss_bar: float = 0.05  # strainer + pipe loss at BEP flow
    # Control
    mode: str = "speed"  # speed | pressure (PID on discharge pressure)
    f_set: float = 55.0
    p_set_bar: float = 4.5
    kp: float = 6.0  # Hz per bar
    ki: float = 3.0  # Hz per bar·s
    # Drive protection
    oc_pu: float = 2.0
    ov_v: float = 810.0
    ov_stall_v: float = 760.0  # hold decel above this bus voltage (0 = off)
    uv_v: float = 425.0
    # Transmitters: 0-10 bar discharge, -1..3 bar suction
    pt_noise: float = 0.015

    @property
    def rho(self):
        return 1000.0 * self.sg

    @property
    def a_pump(self):
        return (self.h0 - self.h_bep) / self.q_bep ** 2

    @property
    def k_sys(self):
        return (self.h_bep - self.h_static) / self.q_bep ** 2

    @property
    def p_bep_hyd_kw(self):
        return self.rho * G * (self.q_bep / 3600) * self.h_bep / 1000


# --------------------------------------------------------------------------------------------
# Scenarios. A fault is a set of plant changes that start at `onset` and grow over `ramp_s`.


SCENARIOS = {
    "normal": "Healthy start, run with demand moving a little, then a normal stop.",
    "normal_pid": "Healthy, VFD holding discharge pressure with its PID while demand changes.",
    "deadhead": "Discharge valve closed while running: flow stops, pressure climbs to shutoff, current falls.",
    "dry_run": "Supply to the suction tank stops; the tank drains, the pump cavitates, then loses prime.",
    "cavitation": "Low suction head (low tank, hot or aerated feed): pressure and current go ragged, head sags.",
    "suction_clog": "Suction strainer plugging over minutes: suction pressure trends down, then cavitation.",
    "discharge_leak": "Line break downstream: flow runs out, discharge pressure drops, current rises to overload.",
    "leak_pid": "Same leak with the PID holding pressure: pressure looks fine, speed and current climb.",
    "thick_material": "Material gets denser/thicker: pressure and current rise together at the same speed.",
    "bearing_failure": "Bearing or seal dragging: current rises and gets noisy, pressure unchanged, then OL trip.",
    "impeller_wear": "Worn impeller over weeks (1 sample/min): pressure and flow sag at the same speed.",
    "line_sag": "Short supply sags: DC bus dips, output voltage clips, current bumps; a deep sag trips UV.",
    "phase_loss": "One input phase lost: DC bus ripple and droop under load, then the drive trips PHL.",
    "fast_decel": "Stop with a too-short decel time: regen pumps the DC bus up until the drive trips OV.",
    "pid_hunting": "PID gains raised mid-run: speed, current and pressure oscillate together around the setpoint.",
    "sensor_stuck": "Discharge transmitter freezes: pressure flatlines while current and speed keep moving.",
    "sensor_open": "Discharge transmitter loop opens: pressure drops to below zero instantly, drive is normal.",
}


@dataclass
class Fault:
    name: str
    onset: float  # s
    ramp_s: float = 0.0
    severity: float = 1.0
    extra: dict = field(default_factory=dict)

    def level(self, t):
        if t < self.onset:
            return 0.0
        if self.ramp_s <= 0:
            return self.severity
        return self.severity * min(1.0, (t - self.onset) / self.ramp_s)


@dataclass
class Run:
    scenario: str
    plant: Plant
    fault: Fault
    duration: float
    dt: float
    start_at: float = 5.0
    stop_at: float | None = None  # run command drops at this time
    demand_wander: float = 0.15  # how far demand drifts (fraction of system resistance)


def build_run(name, rng, randomize=False, plant=None):
    """Make a Run for a scenario. With randomize, plant, timing and severity vary.
    With `plant`, the run uses a copy of that plant (one machine, many scenarios)."""
    p = dataclasses.replace(plant) if plant else Plant()
    j = (lambda lo, hi: rng.uniform(lo, hi)) if randomize else (lambda lo, hi: (lo + hi) / 2)
    if randomize and plant is None:
        p.h0 = j(50, 70)
        p.q_bep = j(90, 150)
        p.h_bep = p.h0 * j(0.70, 0.80)
        p.h_static = p.h_bep * j(0.2, 0.45)
        p.sg = j(1.0, 1.4)
        p.f_set = j(48, 60)
        p.accel_s = j(5, 15)
        p.decel_s = j(5, 15)
        p.tank_level_m = j(2.0, 4.0)
        # Size the motor so BEP sits at 70-90 % load.
        p.p_rated_kw = p.p_bep_hyd_kw / p.eta_h / j(0.70, 0.90)
        p.fla = p.p_rated_kw * 1000 / (math.sqrt(3) * p.v_line * 0.86 * 0.92)
        p.p_set_bar = p.sg * 1000 * G * p.h_bep * (p.f_set / 60) ** 2 * j(0.85, 1.0) / 1e5 + 0.3
    dt, dur = 0.1, 180.0
    on = j(50, 80)
    stop_at = None
    f = Fault(name, onset=on)

    if name == "normal":
        f.onset, stop_at = 1e9, j(130, 160)
    elif name == "normal_pid":
        p.mode, f.onset, dur = "pressure", 1e9, 240.0
    elif name == "deadhead":
        f.ramp_s = j(3, 8)  # valve stroke
    elif name == "dry_run":
        dur = 300.0
        p.tank_level_m = j(1.5, 2.5) if randomize else 2.0
    elif name == "cavitation":
        f.ramp_s, f.severity = j(10, 30), j(0.6, 1.0)
    elif name == "suction_clog":
        dur, f.ramp_s = 360.0, j(180, 260)
    elif name == "discharge_leak":
        f.ramp_s, f.severity = j(0.5, 3), j(0.88, 0.95)
    elif name == "leak_pid":
        p.mode, dur = "pressure", 240.0
        f.ramp_s, f.severity = j(1, 4), j(0.5, 0.7)
    elif name == "thick_material":
        dur, f.ramp_s, f.severity = 240.0, j(40, 100), j(0.15, 0.35)
    elif name == "bearing_failure":
        dur, f.ramp_s, f.severity = 600.0, j(200, 300), j(0.6, 0.8)
    elif name == "impeller_wear":
        dt, dur = 60.0, 21 * 86400.0
        f.onset, f.ramp_s, f.severity = j(1, 3) * 86400, j(14, 17) * 86400, j(0.15, 0.25)
    elif name == "line_sag":
        # Two sags: a shallow one the drive rides through, then a deep one that trips it.
        f.extra = {"sags": [(on, j(0.3, 1.0), j(0.15, 0.25)), (on + j(40, 60), j(0.2, 0.6), j(0.35, 0.5))]}
    elif name == "phase_loss":
        f.extra = {"trip_after": j(8, 20)}
    elif name == "fast_decel":
        stop_at, f.onset = on + 30, on + 30
        f.extra = {"decel_s": j(1.0, 2.0)}
    elif name == "pid_hunting":
        p.mode, dur = "pressure", 240.0
        f.extra = {"kp": j(0.5, 1.5), "ki": j(70, 90)}  # integral cranked up: slow limit cycle
    elif name == "sensor_stuck":
        dur = 240.0
    elif name == "sensor_open":
        pass
    else:
        raise SystemExit(f"unknown scenario {name}")
    return Run(name, p, f, dur, dt, stop_at=stop_at)


# --------------------------------------------------------------------------------------------
# Simulation


def simulate(run: Run, rng: random.Random):
    p, flt = run.plant, run.fault
    dt = run.dt
    n = int(run.duration / dt) + 1
    fast = dt >= 1.0  # long-horizon runs: skip start-up, sample steady state

    f_out = 0.0  # drive output frequency, Hz
    r_mech = 0.0  # mechanical speed, pu of synchronous at base
    vdc = 1.35 * p.v_line
    level = p.tank_level_m
    prime = 1.0
    heat = 0.0  # I²t accumulator, pu²·s above 1.0
    integ = p.f_set  # PID integral (Hz)
    state, code = "STOPPED", ""
    demand = 1.0  # multiplier on system resistance from downstream users
    q_prev, h_prev = 0.0, 0.0
    p_disc_frozen = None
    pl_timer = 0.0
    cav_lp = 0.0  # low-pass noise for cavitation surging
    rows = []
    pv = 0.0  # PID's filtered pressure
    stall_on = not (flt.name == "fast_decel")  # the misconfiguration: stall prevention off

    for i in range(n):
        t = i * dt
        lv = flt.level(t)
        active = t >= flt.onset
        name = flt.name

        # Run command
        run_cmd = t >= run.start_at and (run.stop_at is None or t < run.stop_at)
        if fast:
            run_cmd = True

        # Demand drifts (other users downstream opening and closing)
        demand += (rng.gauss(0, 1) * 0.02 * math.sqrt(dt) - (demand - 1.0) * 0.01 * dt)
        demand = min(max(demand, 1 - run.demand_wander), 1 + run.demand_wander)

        # Plant changes from the fault
        k_sys = p.k_sys * demand
        h0, eta_h, sg = p.h0, p.eta_h, p.sg
        suction_loss = p.suction_loss_bar
        v_line = p.v_line
        bearing_kw = 0.0
        npsh_penalty = 0.0
        ripple = 1.5
        if name in ("deadhead",) and active:
            k_sys *= 1 + 400 * lv ** 3
        if name in ("discharge_leak", "leak_pid") and active:
            k_sys *= 1 - lv
        if name == "thick_material":
            sg *= 1 + lv
            eta_h *= 1 - 0.5 * lv
        if name == "suction_clog":
            suction_loss *= 1 + 30 * lv ** 2
        if name == "cavitation":
            npsh_penalty = 9.0 * lv  # m of NPSHa lost (gas in feed, warm liquid)
        if name == "impeller_wear":
            h0 *= 1 - lv
            eta_h *= 1 - 0.6 * lv
        if name == "bearing_failure":
            bearing_kw = 1.4 * p.p_rated_kw * lv ** 2
        if name == "line_sag":
            for (ts, dur_s, depth) in flt.extra["sags"]:
                if ts <= t < ts + dur_s:
                    v_line *= 1 - depth
        if name == "phase_loss" and active:
            ripple = 1.5 + 60.0
        rho = 1000.0 * sg
        a_pump = p.a_pump * h0 / p.h0  # wear scales the whole curve down

        # Tank: supplied to match outflow except when the supply is lost
        inflow = q_prev if not (name == "dry_run" and active) else 0.0
        level += (inflow - q_prev) / 3600 * dt / p.tank_area_m2
        level = max(level, 0.0)
        if not (name == "dry_run" and active):
            level += (p.tank_level_m - level) * 0.02 * dt  # level control

        # Drive: frequency reference
        if p.mode == "pressure" and state != "FAULT":
            p_meas = rows[-1]["discharge_bar"] if rows else 0.0
            pv += (p_meas - pv) * min(1.0, dt / 1.0)
            err = p.p_set_bar - pv
            kp, ki = (flt.extra["kp"], flt.extra["ki"]) if (name == "pid_hunting" and active) else (p.kp, p.ki)
            if run_cmd and r_mech > 0.25:
                integ += ki * err * dt
                integ = min(max(integ, p.f_min), p.f_base)
            f_ref = min(max(kp * err + integ, p.f_min), p.f_base)
        else:
            f_ref = p.f_set
        if not run_cmd:
            f_ref = 0.0

        # Ramp
        decel_s = flt.extra.get("decel_s", p.decel_s) if (name == "fast_decel" and active) else p.decel_s
        prev_f = f_out
        if state == "FAULT":
            f_out = 0.0
        elif fast:
            f_out = f_ref
        elif f_ref > f_out:
            f_out = min(f_ref, f_out + p.f_base / p.accel_s * dt)
        elif stall_on and vdc > p.ov_stall_v:
            pass  # OV stall prevention: hold frequency until the bus comes down
        else:
            f_out = max(f_ref, f_out - p.f_base / decel_s * dt)
        if state != "FAULT":
            if f_out <= 0 and not run_cmd:
                state = "STOPPED"
            elif f_out > prev_f + 1e-9:
                state = "ACCEL"
            elif f_out < prev_f - 1e-9:
                state = "DECEL"
            elif f_out > 0:
                state = "RUN"

        # Mechanical speed: follows the drive (minus slip), coasts when the drive is off
        r_prev = r_mech
        if state == "FAULT" or (f_out == 0 and not run_cmd):
            r_mech = max(0.0, r_mech - r_mech * dt / 4.0 - 0.01 * dt) if not fast else 0.0
        else:
            r_mech = f_out / p.f_base * (1 - p.slip_pu * min(1.0, r_mech))
        drdt = (r_mech - r_prev) / dt if not fast else 0.0

        # Hydraulics at this speed
        r = r_mech
        h_shut = h0 * r * r
        q = math.sqrt(max(0.0, h_shut - p.h_static) / (a_pump + k_sys)) if r > 0 else 0.0
        h = h_shut - a_pump * q * q

        # Suction side and NPSH
        ps_abs = max(P_VAPOR + 0.02, P_ATM + rho * G * level / 1e5 - suction_loss * (q / p.q_bep) ** 2)
        npsha = (ps_abs - P_VAPOR) * 1e5 / (rho * G) - npsh_penalty
        npshr = r * r * (2.0 + 4.0 * (q / max(p.q_bep * r, 1e-6)) ** 2) if r > 0.05 else 0.0
        cav = min(max((npshr - npsha + 1.0) / 3.0, 0.0), 1.0)
        # Gas collects in the eye when cavitation is heavy and the tank is low: prime is lost
        if r > 0.3 and (cav > 0.7 or level < 0.15):
            prime = max(0.0, prime - 0.08 * dt)
        elif level > 0.4 and cav < 0.2:
            prime = min(1.0, prime + 0.2 * dt)
        cav_lp += (rng.gauss(0, 1) - cav_lp) * min(1.0, dt / 0.4)
        head_factor = prime * (1 - 0.25 * cav) * (1 + 0.12 * cav * cav_lp)
        h_eff = h * head_factor
        q_eff = q * (prime * (1 - 0.15 * cav)) if h_eff > p.h_static or prime > 0.9 else 0.0
        if h_eff < p.h_static and q_eff > 0:  # can't lift the static head: check valve shuts
            q_eff *= max(0.0, (h_eff - 0.6 * p.h_static) / (0.4 * p.h_static))
        # Smooth (line inertia)
        tau = 0.3 if not fast else 0.0
        a = 1.0 if tau == 0 else min(1.0, dt / tau)
        q_prev = q_prev + (q_eff - q_prev) * a
        h_prev = h_prev + (h_eff - h_prev) * a
        ps_gauge = ps_abs - P_ATM
        p_disc = ps_gauge + rho * G * h_prev / 1e5
        if r < 0.02:
            p_disc = rho * G * p.h_static / 1e5 * 0.98 if t > run.start_at + 1 and not fast else 0.0  # check valve holds the column
            if rows and rows[-1]["freq_hz"] == 0 and t <= run.start_at:
                p_disc = 0.0

        # Shaft power and torque
        # Radial pump power curve: rises roughly linearly with flow, from shutoff to run-out,
        # scaled by speed³ (affinity), density, and lost efficiency (wear, viscosity).
        p_bep_shaft = p.p_bep_hyd_kw / p.eta_h
        p_shut = 0.45 * p_bep_shaft * (0.4 + 0.6 * prime)
        q_pu = q_prev / (p.q_bep * r) if r > 0.05 else 0.0
        pump_kw = r ** 3 * (p_shut + (p_bep_shaft - p_shut) * q_pu) * (sg / p.sg) * (p.eta_h / eta_h) ** 0.5
        shaft_kw = pump_kw + bearing_kw * r
        t_load = shaft_kw / p.p_rated_kw / max(r, 0.05) if r > 0 else 0.0
        t_acc = p.j_pu * drdt
        torque = t_load + t_acc
        if name == "bearing_failure" and active:
            torque *= 1 + 0.08 * lv * rng.gauss(0, 1)
        if cav > 0:
            torque *= 1 + 0.06 * cav * cav_lp

        # Drive electrical
        v_cmd = p.v_line * min(1.0, max(f_out / p.f_base, 0.0)) if f_out > 0 else 0.0
        if 0 < f_out < 6:
            v_cmd = max(v_cmd, 0.05 * p.v_line)  # low-speed boost
        v_max = vdc / 1.35 * 0.99
        v_out = min(v_cmd, v_max)
        flux = v_out / v_cmd if v_cmd > 0 else 0.0
        if state == "FAULT" or f_out == 0:
            i_mot, v_out, flux = 0.0, 0.0, 0.0
        else:
            imag = p.imag_pu * p.fla * flux
            it_fla = p.fla * math.sqrt(1 - p.imag_pu ** 2)
            i_t = it_fla * torque / max(flux, 0.3)
            i_mot = math.sqrt(imag ** 2 + i_t ** 2)
        p_elec_kw = (shaft_kw + t_acc * r * p.p_rated_kw) / 0.92 if i_mot > 0 else 0.0
        if torque < 0 and i_mot > 0:
            p_elec_kw = torque * r * p.p_rated_kw * 0.85 + 0.6  # regen back into the bus, less losses

        # DC bus
        vdc_nl = 1.35 * v_line
        if name == "phase_loss" and active:
            vdc_nl *= 1 - 0.07 * min(1.0, max(p_elec_kw, 0) / p.p_rated_kw)
        if p_elec_kw < 0:
            vdc += (-p_elec_kw * 1000) * dt / (0.012 * max(vdc, 100))
        else:
            target = vdc_nl - 0.39 * (p_elec_kw * 1000 / max(vdc, 100))
            vdc += (target - vdc) * (1.0 if fast else min(1.0, dt / 0.05))
        load = min(1.0, max(p_elec_kw, 0) / p.p_rated_kw)
        vdc_meas = vdc + rng.gauss(0, ripple * (0.3 + load) / 1.3)

        # Protection
        if state != "FAULT":
            heat = max(0.0, heat + ((i_mot / p.fla) ** 2 - 1.05) * dt)
            if i_mot > p.oc_pu * p.fla:
                state, code = "FAULT", "OC"
            elif heat > 60 * (1.5 ** 2 - 1.05):
                state, code = "FAULT", "OL"
            elif vdc > p.ov_v:
                state, code = "FAULT", "OV"
            elif vdc < p.uv_v and (run_cmd or f_out > 0):
                state, code = "FAULT", "UV"
            if name == "phase_loss" and active and load > 0.4:
                pl_timer += dt
                if pl_timer > flt.extra["trip_after"]:
                    state, code = "FAULT", "PHL"
            if state == "FAULT":
                i_mot, v_out = 0.0, 0.0

        # Transmitters
        p_disc_m = p_disc + rng.gauss(0, p.pt_noise) + (0.05 * cav * rng.gauss(0, 1) * r)
        p_suc_m = ps_gauge + rng.gauss(0, p.pt_noise * 0.5) + 0.03 * cav * rng.gauss(0, 1) * r
        if name == "sensor_stuck" and active:
            if p_disc_frozen is None:
                p_disc_frozen = round(p_disc_m, 2)
            p_disc_m = p_disc_frozen
        if name == "sensor_open" and active:
            p_disc_m = (3.6 - 4.0) / 16 * 10 + rng.gauss(0, 0.002)  # 3.6 mA on a 0-10 bar span
        cur_m = i_mot * (1 + rng.gauss(0, 0.004)) if i_mot > 0 else 0.0

        rows.append({
            "t_s": round(t, 3),
            "run_cmd": int(run_cmd),
            "freq_hz": round(f_out, 2),
            "out_voltage_v": round(v_out, 1),
            "dc_bus_v": round(vdc_meas, 1),
            "current_a": round(cur_m, 2),
            "suction_bar": round(p_suc_m, 3),
            "discharge_bar": round(p_disc_m, 3),
            "flow_m3h": round(q_prev, 1),
            "vfd_state": state,
            "fault_code": code,
            "label": name if active and name not in ("normal", "normal_pid") else "normal",
        })
    return rows


# --------------------------------------------------------------------------------------------
# Output


COLUMNS = ["t_s", "run_cmd", "freq_hz", "out_voltage_v", "dc_bus_v", "current_a", "suction_bar",
           "discharge_bar", "flow_m3h", "vfd_state", "fault_code", "label"]


def write_csv(path, rows):
    with open(path, "w", newline="") as fh:
        w = csv.DictWriter(fh, fieldnames=COLUMNS)
        w.writeheader()
        w.writerows(rows)


def summary(run, rows):
    """What happened, read off the signals (not the plant) for the manifest."""
    trip = next((r for r in rows if r["vfd_state"] == "FAULT"), None)
    return {
        "scenario": run.scenario,
        "description": SCENARIOS[run.scenario],
        "fault_onset_s": None if run.fault.onset > run.duration else round(run.fault.onset, 1),
        "dt_s": run.dt,
        "duration_s": run.duration,
        "control": run.plant.mode,
        "fla_a": round(run.plant.fla, 1),
        "motor_kw": round(run.plant.p_rated_kw, 1),
        "sg": round(run.plant.sg, 2),
        "trip": None if trip is None else {"t_s": trip["t_s"], "code": trip["fault_code"]},
    }


def plot(runs, path):
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    n = len(runs)
    fig, axes = plt.subplots(n, 3, figsize=(15, 2.0 * n), squeeze=False)
    for i, (run, rows) in enumerate(runs):
        hours = run.dt >= 1.0
        tx = [r["t_s"] / (86400 if hours else 1) for r in rows]
        unit = "days" if hours else "s"
        ax_i, ax_v, ax_p = axes[i]
        ax_i.plot(tx, [r["current_a"] for r in rows], color="#c2410c", lw=0.8)
        ax_i.axhline(run.plant.fla, color="#999", lw=0.6, ls=":")
        ax_i.set_ylabel(run.scenario, fontsize=9, rotation=0, ha="right", va="center")
        ax_v.plot(tx, [r["dc_bus_v"] for r in rows], color="#1d4ed8", lw=0.8, label="DC bus")
        ax_v.plot(tx, [r["out_voltage_v"] for r in rows], color="#60a5fa", lw=0.8, label="output")
        ax_p.plot(tx, [r["discharge_bar"] for r in rows], color="#15803d", lw=0.8, label="discharge")
        ax_p.plot(tx, [r["suction_bar"] for r in rows], color="#86efac", lw=0.8, label="suction")
        for ax in (ax_i, ax_v, ax_p):
            ax.tick_params(labelsize=7)
            ax.grid(alpha=0.2)
            if run.fault.onset < run.duration:
                ax.axvline(run.fault.onset / (86400 if hours else 1), color="#dc2626", lw=0.7, ls="--")
            trip = next((r for r in rows if r["vfd_state"] == "FAULT"), None)
            if trip:
                ax.axvline(trip["t_s"] / (86400 if hours else 1), color="#000", lw=0.7)
        if i == 0:
            ax_i.set_title("Motor current (A)  · dotted = FLA", fontsize=9)
            ax_v.set_title("Voltage (V)", fontsize=9)
            ax_p.set_title("Pressure (bar g)", fontsize=9)
            ax_v.legend(fontsize=7, loc="lower right")
            ax_p.legend(fontsize=7, loc="lower right")
        if i == n - 1 or hours:
            for ax in (ax_i, ax_v, ax_p):
                ax.set_xlabel(unit, fontsize=7)
    fig.text(0.5, 0.995, "Red dashed: fault starts · black: drive trips", ha="center", va="top", fontsize=9)
    fig.tight_layout(rect=(0, 0, 1, 0.99))
    fig.savefig(path, dpi=110)


def cowpoke_yaml(run, rows, path, port=5030):
    """A Modbus server whose tags replay this run (sequence generators), in a VFD register layout."""
    ms = max(50, int(run.dt * 1000))
    states = {"STOPPED": 0, "ACCEL": 1, "RUN": 2, "DECEL": 3, "FAULT": 4}
    codes = {"": 0, "OC": 1, "OV": 2, "UV": 3, "OL": 4, "PHL": 5}

    def seq(col, f=lambda v: v):
        return [f(r[col]) for r in rows]

    tags = [
        ("VFD1.freq", "float32", 0, "Hz", seq("freq_hz")),
        ("VFD1.out_voltage", "float32", 2, "V", seq("out_voltage_v")),
        ("VFD1.dc_bus", "float32", 4, "V", seq("dc_bus_v")),
        ("VFD1.current", "float32", 6, "A", seq("current_a")),
        ("PT-101.suction", "float32", 8, "bar", seq("suction_bar")),
        ("PT-102.discharge", "float32", 10, "bar", seq("discharge_bar")),
        ("FT-101.flow", "float32", 12, "m³/h", seq("flow_m3h")),
        ("VFD1.state", "uint16", 14, None, seq("vfd_state", states.get)),
        ("VFD1.fault", "uint16", 15, None, seq("fault_code", codes.get)),
    ]
    lines = [
        f"# Replays pump_sim scenario `{run.scenario}`: {SCENARIOS[run.scenario]}",
        f"# {len(rows)} samples every {ms} ms, then it loops. VFD1.state: 0 stopped, 1 accel, 2 run, 3 decel, 4 fault.",
        "# VFD1.fault: 0 none, 1 OC, 2 OV, 3 UV, 4 OL, 5 PHL.",
        f"# Run: COWPOKE_CONFIG={os.path.basename(path)} cargo run",
        "services:",
        f"  - id: pump-{run.scenario.replace('_', '-')}",
        "    kind: modbus",
        "    mode: server",
        f"    settings: {{ port: {port} }}",
        "    tags:",
    ]
    for name, dtype, reg, units, values in tags:
        u = f", units: \"{units}\"" if units else ""
        vals = ", ".join(str(v) for v in values)
        lines.append(
            f"      - {{ name: {name}, datatype: {dtype}, table: input_register, register: {reg}{u}, "
            f"initial: {values[0]}, simulate: {{ kind: sequence, interval_ms: {ms}, values: [{vals}] }} }}"
        )
    with open(path, "w") as fh:
        fh.write("\n".join(lines) + "\n")


# --------------------------------------------------------------------------------------------


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--list", action="store_true", help="list scenarios")
    ap.add_argument("--catalog", action="store_true", help="one nominal run of every scenario, plus a PNG")
    ap.add_argument("--scenario", help="run one scenario (nominal)")
    ap.add_argument("--dataset", type=int, help="this many randomised runs, scenarios evenly mixed")
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--out", default="out")
    ap.add_argument("--cowpoke", help="with --scenario: also write a Cowpoke replay YAML here")
    args = ap.parse_args()

    if args.list:
        for k, v in SCENARIOS.items():
            print(f"{k:16} {v}")
        return
    rng = random.Random(args.seed)
    os.makedirs(args.out, exist_ok=True)

    if args.scenario or args.catalog:
        names = [args.scenario] if args.scenario else list(SCENARIOS)
        done, manifest = [], []
        for name in names:
            run = build_run(name, rng)
            rows = simulate(run, rng)
            write_csv(os.path.join(args.out, f"{name}.csv"), rows)
            manifest.append(summary(run, rows) | {"file": f"{name}.csv"})
            done.append((run, rows))
            if args.cowpoke:
                cowpoke_yaml(run, rows, args.cowpoke)
        with open(os.path.join(args.out, "manifest.json"), "w") as fh:
            json.dump(manifest, fh, indent=2)
        plot(done, os.path.join(args.out, "catalog.png" if args.catalog else f"{names[0]}.png"))
        for m in manifest:
            print(f"{m['scenario']:16} trip={m['trip']}")
        return

    if args.dataset:
        names = list(SCENARIOS)
        with open(os.path.join(args.out, "manifest.jsonl"), "w") as man:
            for k in range(args.dataset):
                name = names[k % len(names)]
                run = build_run(name, rng, randomize=True)
                rows = simulate(run, rng)
                fn = f"{k:05d}-{name}.csv"
                write_csv(os.path.join(args.out, fn), rows)
                man.write(json.dumps(summary(run, rows) | {"file": fn}) + "\n")
        print(f"wrote {args.dataset} runs to {args.out}")
        return
    ap.print_help()


if __name__ == "__main__":
    main()
