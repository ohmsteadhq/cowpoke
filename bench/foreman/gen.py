#!/usr/bin/env python3
"""Fault generator: Cowpoke as a data factory for training a foreman decision model.

Each scenario is a random device (a Cowpoke server/simulator) and a client set up to talk
to it, with one injected misconfiguration or none. A batch of scenarios runs in a private
Cowpoke instance; once the clients settle, the generator records what Cowpoke can see
(status, definitions, live values, rule findings) and the right answer.

Output is JSON Lines in the tev1 decision format (state, question, options, answer):
two decisions per scenario (which service, what cause), each written with and without
the rule findings so the model learns to use them but doesn't depend on them.

  python3 bench/foreman/gen.py --scenarios 200 --out data/faults.jsonl --seed 1

The symptom text is derived from what actually happened (bad quality, no value, a wrong
value), not from the injected fault, so the model can't read the answer off the question.
"""

import argparse
import json
import os
import random
import shutil
import signal
import subprocess
import tempfile
import time

import yaml  # PyYAML

import bench

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
BIN = os.path.join(ROOT, "target", "debug", "cowpoke")

WORDS = ["level", "temp", "flow", "pressure", "speed", "count", "run", "valve", "setpoint", "current",
         "weight", "ph", "humidity", "torque", "position", "batch"]
AREAS = ["tank", "pump", "mixer", "boiler", "conveyor", "press", "dryer", "filler", "chiller", "kiln"]
PROTOCOL_FAULTS = {
    "modbus": ["address_offset", "word_order", "unit_id", "scaling", "port_or_host", "none"],
    "eip": ["tag_name", "port_or_host", "none"],
    "mqtt": ["credentials", "topic", "port_or_host", "none"],
    "sparkplug": ["sparkplug_ids", "none"],
    "api": ["json_pointer", "port_or_host", "none"],
    "iolink": ["bit_offset", "port_or_host", "none"],
}


def typo(s, rng):
    """A plausible one-character mistake."""
    i = rng.randrange(len(s))
    ops = [lambda: s[:i] + s[i + 1:] if len(s) > 2 else s + "x",       # drop
           lambda: s[:i] + s[i] + s[i:],                                 # double
           lambda: s[:i] + s[i + 1:i + 2] + s[i:i + 1] + s[i + 2:]]     # swap
    for _ in range(10):
        t = rng.choice(ops)()
        if t != s:
            return t
    return s + "1"


def value_for(dt, rng):
    if dt == "bool":
        return rng.random() < 0.5
    if dt == "string":
        return f"B-{rng.randrange(10000):04d}"
    if dt in ("float32", "float64"):
        return round(rng.uniform(1, 500), 1)
    if dt == "int16":
        return rng.randrange(-500, 500)
    return rng.randrange(1, 5000)


def make_scenario(n, port, rng, kind=None, fault=None):
    """Return (services, answer). Ports: port (device) and port + 50 (a wrong port)."""
    if kind is None:
        # Pick the cause first so causes are balanced (port faults fit every protocol),
        # then a protocol that can have it.
        fault = fault or rng.choice(sorted({c for cs in PROTOCOL_FAULTS.values() for c in cs}))
        kind = rng.choice([k for k, cs in PROTOCOL_FAULTS.items() if fault in cs])
    fault = fault or rng.choice(PROTOCOL_FAULTS[kind])
    area = rng.choice(AREAS)
    sid, cid = f"g{n}-{area}-sim", f"g{n}-{area}-cli"
    names = rng.sample(WORDS, rng.randint(1, 3))
    victim = names[0]  # the tag a tag-level fault hits
    wrong_port = port + 50
    srv_tags, cli_tags = [], []
    ss, cs = {"bind": "127.0.0.1", "port": port}, {}
    k = "mqtt" if kind == "sparkplug" else kind

    if kind == "modbus":
        cs = {"host": "127.0.0.1", "port": port, "poll_ms": 300, "timeout_ms": 800}
        order = rng.choice(["big", "little"])
        ss["word_order"] = order
        cs["word_order"] = order
        reg = 0
        unit = rng.choice([1, 1, 1, 2, 5])
        for nm in names:
            dt = rng.choice(["float32", "int32", "uint16", "int16"]) if nm != victim or fault != "word_order" \
                else rng.choice(["float32", "int32"])
            if fault == "scaling" and nm == victim:
                dt = "int16"
            st = {"name": nm, "datatype": dt, "register": reg, "unit": unit}
            if fault == "scaling" and nm == victim:
                st["scale"] = rng.choice([0.1, 0.01])
                st["initial"] = round(rng.uniform(5, 300) * 1.0, 1) if st["scale"] == 0.1 else round(rng.uniform(1, 30), 2)
            else:
                st["initial"] = value_for(dt, rng)
            ct = {kk: v for kk, v in st.items() if kk != "initial"}
            reg += 2 if dt in ("float32", "int32") else 1
            srv_tags.append(st)
            cli_tags.append(ct)
        vt = cli_tags[0]
        if fault == "address_offset":
            vt["register"] = vt["register"] + rng.choice([1, 2, 1, -1 if vt["register"] > 0 else 3])
        elif fault == "word_order":
            cs["word_order"] = "big" if order == "little" else "little"
        elif fault == "unit_id":
            vt["unit"] = rng.choice([u for u in (1, 2, 3, 5, 10) if u != unit])
            for t in cli_tags:
                t["unit"] = vt["unit"]
        elif fault == "scaling":
            vt.pop("scale", None)
    elif kind == "eip":
        cs = {"host": "127.0.0.1", "port": port, "poll_ms": 300, "timeout_ms": 800}
        prog = rng.choice(["", "Program:Main.", "Program:Line1.", f"{area.capitalize()}."])
        for nm in names:
            dt = rng.choice(["float32", "int32", "int16", "bool", "string"])
            cip = prog + nm.capitalize() + rng.choice(["", "_PV", "_Act"])
            srv_tags.append({"name": nm, "datatype": dt, "cip_tag": cip, "initial": value_for(dt, rng)})
            cli_tags.append({"name": nm, "datatype": dt, "cip_tag": cip})
        if fault == "tag_name":
            cip = cli_tags[0]["cip_tag"]
            base = cip.rsplit(".", 1)[-1]
            cli_tags[0]["cip_tag"] = cip[: len(cip) - len(base)] + typo(base, rng)
    elif kind in ("mqtt", "sparkplug"):
        cs = {"host": "127.0.0.1", "port": port}
        if kind == "mqtt":
            prefix = f"plant/{area}{rng.randrange(1, 9)}"
            ss["prefix"] = prefix
            if fault == "credentials" or rng.random() < 0.3:
                ss["username"], ss["password"] = "cowpoke", f"pw{rng.randrange(10**6)}"
                cs["username"], cs["password"] = ss["username"], ss["password"]
            for nm in names:
                dt = rng.choice(["float64", "int32", "bool", "string"])
                srv_tags.append({"name": nm, "datatype": dt, "initial": value_for(dt, rng)})
                cli_tags.append({"name": nm, "datatype": dt, "topic": f"{prefix}/{nm}"})
            if fault == "credentials":
                cs["password"] = typo(cs["password"], rng)
            elif fault == "topic":
                parts = cli_tags[0]["topic"].split("/")
                j = rng.randrange(len(parts))
                parts[j] = typo(parts[j], rng)
                cli_tags[0]["topic"] = "/".join(parts)
        else:
            sp = {"group_id": rng.choice(["Plant", "SiteA", "Line"]), "edge_node": f"{area.capitalize()}{rng.randrange(1, 9)}"}
            if rng.random() < 0.4:
                sp["device"] = f"Dev{rng.randrange(1, 9)}"
            ss["sparkplug"] = dict(sp)
            cs["sparkplug"] = dict(sp)
            for nm in names:
                dt = rng.choice(["float64", "int32", "bool"])
                srv_tags.append({"name": nm, "datatype": dt, "initial": value_for(dt, rng)})
                cli_tags.append({"name": nm, "datatype": dt})
            if fault == "sparkplug_ids":
                key = rng.choice([x for x in ("group_id", "edge_node", "device") if x in sp])
                cs["sparkplug"][key] = typo(sp[key], rng)
    elif kind == "api":
        ss["port"] = port
        cs = {"url": f"http://127.0.0.1:{port}/tags", "interval_ms": 500}
        for nm in names:
            dt = rng.choice(["float64", "int32", "bool", "string"])
            srv_tags.append({"name": nm, "datatype": dt, "initial": value_for(dt, rng)})
            cli_tags.append({"name": nm, "datatype": dt, "pointer": f"/{nm}"})
        if fault == "json_pointer":
            cli_tags[0]["pointer"] = "/" + typo(victim, rng)
        elif fault == "port_or_host":
            cs["url"] = f"http://127.0.0.1:{wrong_port}/tags"
    elif kind == "iolink":
        cs = {"host": "127.0.0.1", "port": port, "poll_ms": 300, "timeout_ms": 800}
        off = 0
        p = rng.randint(1, 4)
        for nm in names:
            dt = rng.choice(["uint16", "int16", "bool"])
            bits = 1 if dt == "bool" else 16
            srv_tags.append({"name": nm, "datatype": dt, "port": p, "bit_offset": off, "bit_length": bits,
                             "initial": value_for(dt, rng) if dt != "int16" else rng.randrange(-300, 300)})
            cli_tags.append({"name": nm, "datatype": dt, "port": p, "bit_offset": off, "bit_length": bits})
            off += bits
        if fault == "bit_offset":
            cli_tags[0]["bit_offset"] += rng.choice([1, 4, 8, -1 if cli_tags[0]["bit_offset"] else 2])

    if fault == "port_or_host" and kind != "api":
        cs["port"] = wrong_port
    services = [
        {"id": sid, "kind": k, "mode": "server", "settings": ss, "tags": srv_tags},
        {"id": cid, "kind": k, "mode": "client", "settings": cs, "tags": cli_tags},
    ]
    answer = {"service": cid if fault != "none" else "none", "cause": fault, "kind": kind,
              "victim": victim, "prefix": f"g{n}-"}
    return services, answer


def symptom(cp, ans, services, rng):
    """Describe what an operator would notice, from the client's actual tag states."""
    cid = services[1]["id"]
    tags = {t["name"]: t for t in cp.tool("read_tags", {"id": cid})}
    truth = {t["name"]: t.get("initial") for t in services[0]["tags"]}
    area = cid.split("-")[1]
    order = [ans["victim"]] + [n for n in tags if n != ans["victim"]]
    for nm in order:
        t = tags.get(nm) or {}
        want = truth.get(nm)
        if t.get("quality") is None:
            return f"The {area} {nm} never shows a value on the HMI."
        if t.get("quality") == "bad":
            return f"The {area} {nm} shows bad quality."
        v = t.get("value")
        if isinstance(want, float) and isinstance(v, (int, float)) and abs(v - want) > max(0.01, abs(want) * 1e-3):
            return f"The {area} {nm} reads {v:g}, but the local display shows {want:g}."
        if not isinstance(want, float) and v != want:
            return f"The {area} {nm} reads {json.dumps(v)}, but the device shows {json.dumps(want)}."
    nm = ans["victim"]
    return rng.choice([f"An operator thinks the {area} {nm} is off; the local display shows {json.dumps(truth.get(nm))}.",
                       f"Please check the {area} {nm}; someone says it looks wrong.",
                       f"Is the {area} {nm} reading correctly? The device shows {json.dumps(truth.get(nm))}."])


def letters(opts):
    return {chr(65 + i): (k, v) for i, (k, v) in enumerate(opts.items())}


def decisions(state_text, ans, ids, rng):
    """Two tev1-style examples: which service, and what cause (options shuffled)."""
    out = []
    svc = {i: f"Service {i} is misconfigured" for i in ids}
    svc["none"] = "Nothing is misconfigured"
    causes = dict(bench.CAUSE_TEXT)
    for q, opts, right in (("Which service is misconfigured?", svc, ans["service"]),
                           ("What is the cause of the problem?", causes, ans["cause"])):
        items = list(opts.items())
        rng.shuffle(items)
        lab = letters(dict(items))
        out.append({
            "state": state_text,
            "question": q,
            "options": {L: f"{k}: {v}" for L, (k, v) in lab.items()},
            "answer": next(L for L, (k, _) in lab.items() if k == right),
            "answer_key": right,
        })
    return out


def run_batch(scen, workdir, web_port, password, settle):
    cfg = {"services": [s for svcs, _ in scen for s in svcs]}
    path = os.path.join(workdir, "cowpoke.yaml")
    with open(path, "w") as f:
        yaml.safe_dump(cfg, f, sort_keys=False)
    env = dict(os.environ, COWPOKE_DATA=workdir, COWPOKE_CONFIG=path, COWPOKE_BIND=f"127.0.0.1:{web_port}",
               COWPOKE_ADMIN_PASSWORD=password, COWPOKE_LOG="warn")
    proc = subprocess.Popen([BIN], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        url = f"http://127.0.0.1:{web_port}"
        for _ in range(100):
            try:
                cp = bench.Cowpoke(url, "admin", password)
                break
            except Exception:
                time.sleep(0.1)
        else:
            raise RuntimeError("Cowpoke didn't start")
        time.sleep(settle)
        return [(cp, svcs, ans) for svcs, ans in scen], proc
    except Exception:
        proc.send_signal(signal.SIGTERM)
        raise


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--scenarios", type=int, default=100)
    ap.add_argument("--batch", type=int, default=40, help="scenarios per Cowpoke instance")
    ap.add_argument("--out", default=os.path.join(os.path.dirname(__file__), "data", "faults.jsonl"))
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--base-port", type=int, default=31000)
    ap.add_argument("--web-port", type=int, default=18190)
    ap.add_argument("--settle", type=float, default=6.0, help="seconds to let clients connect and poll")
    a = ap.parse_args()
    if not os.path.exists(BIN):
        raise SystemExit(f"build Cowpoke first: {BIN}")

    rng = random.Random(a.seed)
    password = f"gen-{rng.randrange(10**9)}"
    os.makedirs(os.path.dirname(os.path.abspath(a.out)), exist_ok=True)
    counts, written = {}, 0
    with open(a.out, "w") as out:
        done = 0
        while done < a.scenarios:
            n = min(a.batch, a.scenarios - done)
            scen = [make_scenario(done + i + 1, a.base_port + i, rng) for i in range(n)]
            work = tempfile.mkdtemp(prefix="cowpoke-gen-")
            proc = None
            try:
                runs, proc = run_batch(scen, work, a.web_port, password, a.settle)
                for cp, svcs, ans in runs:
                    sym = symptom(cp, ans, svcs, rng)
                    ids = [s["id"] for s in svcs]
                    for rules in (True, False):
                        state = f"Operator report: {sym}\n\nServices:\n" + bench.evidence(cp, ans["prefix"], rules)
                        for ex in decisions(state, ans, ids, rng):
                            ex["meta"] = {"kind": ans["kind"], "cause": ans["cause"], "rules": rules, "seed": a.seed}
                            out.write(json.dumps(ex) + "\n")
                            written += 1
                    counts[(ans["kind"], ans["cause"])] = counts.get((ans["kind"], ans["cause"]), 0) + 1
            finally:
                if proc:
                    proc.send_signal(signal.SIGTERM)
                    try:
                        proc.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                shutil.rmtree(work, ignore_errors=True)
            done += n
            print(f"{done}/{a.scenarios} scenarios, {written} examples", flush=True)
    print(json.dumps({f"{k}/{c}": v for (k, c), v in sorted(counts.items())}, indent=0))


if __name__ == "__main__":
    main()
