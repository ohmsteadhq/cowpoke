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
BIN = os.path.join(ROOT, "target", "release", "cowpoke")
if not os.path.exists(BIN):
    BIN = os.path.join(ROOT, "target", "debug", "cowpoke")

WORDS = ["level", "temp", "flow", "pressure", "speed", "count", "run", "valve", "setpoint", "current",
         "weight", "ph", "humidity", "torque", "position", "batch"]
AREAS = ["tank", "pump", "mixer", "boiler", "conveyor", "press", "dryer", "filler", "chiller", "kiln"]
PROTOCOL_FAULTS = {
    "modbus": ["address_offset", "word_order", "unit_id", "scaling", "port_or_host", "register_table", "datatype",
               "string_length", "device_offline", "protocol_mismatch", "none"],
    "eip": ["tag_name", "port_or_host", "device_offline", "protocol_mismatch", "none"],
    "mqtt": ["credentials", "topic", "port_or_host", "device_offline", "other", "none"],
    "sparkplug": ["sparkplug_ids", "metric_name", "none"],
    "api": ["json_pointer", "port_or_host", "url_path", "none"],
    "iolink": ["bit_offset", "address_offset", "port_or_host", "other", "none"],
    "opcua": ["node_id", "datatype", "security_policy", "credentials", "port_or_host", "device_offline", "none"],
    "s7": ["address_offset", "datatype", "string_length", "device_offline", "port_or_host", "protocol_mismatch", "none"],
}

# Share of faulty scenarios whose rule findings are hidden in the "with rules" variant, so the
# model learns that silent rule checks are not proof of health (real sites have gaps).
HIDE_RULES = 0.35


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


def recase(s, rng):
    """A case-only mistake (Line5 vs line5), the kind rule checks catch but people miss."""
    t = rng.choice([s.lower(), s.upper(), s.capitalize(), s.swapcase()])
    return t if t != s else (s[:1].swapcase() + s[1:] if s[:1].isalpha() else s + "x")


def slip(s, rng):
    """A typo, a case-only change or a dash/underscore change, at random (v6)."""
    r = rng.random()
    if r < 0.25:
        return recase(s, rng)
    if r < 0.4 and any(c.isdigit() for c in s) and "-" not in s:
        i = next(j for j, c in enumerate(s) if c.isdigit())
        if i:
            return s[:i] + rng.choice(["-", "_"]) + s[i:]
    return typo(s, rng)


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


DECOYS = 0.3  # v6: share of scenarios with 1-2 healthy devices of other protocols beside the faulty one


def make_scenario(n, port, rng, kind=None, fault=None):
    """A scenario, sometimes with healthy decoy devices so the model must find the faulty one."""
    svcs, ans = make_one(n, port, rng, kind, fault)
    if kind is None and rng.random() < DECOYS:
        used = {svcs[0]["id"].split("-")[1]}
        for j in range(rng.randint(1, 2)):
            dk = rng.choice([k for k in ("modbus", "eip", "mqtt", "api", "iolink", "s7") if k != svcs[0]["kind"]])
            d, _ = make_one(n, port + 100 + 40 * j, rng, dk, "none")
            area = rng.choice([a for a in AREAS if a not in used])
            used.add(area)
            d[0]["id"], d[1]["id"] = f"g{n}-{area}-sim", f"g{n}-{area}-cli"
            svcs += d
    return svcs, ans


def make_one(n, port, rng, kind=None, fault=None):
    """Return (services, answer). Ports: port (device) and port + 50 (a wrong port)."""
    if kind is None:
        # Pick the cause first so causes are balanced (port faults fit every protocol),
        # then a protocol that can have it.
        fault = fault or rng.choice(sorted({c for cs in PROTOCOL_FAULTS.values() for c in cs}))
        kind = rng.choice([k for k, cs in PROTOCOL_FAULTS.items() if fault in cs])
    fault = fault or rng.choice(PROTOCOL_FAULTS[kind])
    if fault == "protocol_mismatch":
        svcs, ans = make_one(n, port, rng, kind=kind, fault="none")
        other = rng.choice([k for k in ("modbus", "eip", "s7", "mqtt") if k != kind])
        osvcs, _ = make_one(n, port, rng, kind=other, fault="none")
        osvcs[0]["id"] = svcs[0]["id"]
        svcs[0] = osvcs[0]
        ans.update(service=svcs[1]["id"], cause="protocol_mismatch", why=explain("protocol_mismatch", svcs, None, rng))
        return svcs, ans
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
        double = fault == "scaling" and rng.random() < 0.5  # client scales a value already in units
        coil = fault == "register_table" and rng.random() < 0.35  # coil read as a discrete input
        for nm in names:
            dt = rng.choice(["float32", "int32", "uint16", "int16"]) if nm != victim or fault != "word_order" \
                else rng.choice(["float32", "int32", "float64", "float64"])
            if fault == "scaling" and nm == victim:
                dt = "float32" if double else "int16"
            if fault == "datatype" and nm == victim:
                dt = "float32"
            if coil and nm == victim:
                dt = "bool"
            if fault == "string_length" and nm == victim:
                dt = "string"
            st = {"name": nm, "datatype": dt, "register": reg, "unit": unit}
            if coil and nm == victim:
                st["table"] = "coil"
            if dt == "string":
                st["length"] = 6
                st["initial"] = f"B-{rng.randrange(2020, 2030)}-{rng.randrange(10000):04d}"
            elif double and nm == victim:
                st["initial"] = round(rng.uniform(5, 300), 1)
            elif fault == "scaling" and nm == victim:
                st["scale"] = rng.choice([0.1, 0.01])
                st["initial"] = round(rng.uniform(5, 300) * 1.0, 1) if st["scale"] == 0.1 else round(rng.uniform(1, 30), 2)
            else:
                st["initial"] = value_for(dt, rng)
            ct = {kk: v for kk, v in st.items() if kk != "initial"}
            reg += 6 if dt == "string" else 4 if dt == "float64" else 2 if dt in ("float32", "int32") else 1
            srv_tags.append(st)
            cli_tags.append(ct)
        vt = cli_tags[0]
        same = [t for t in cli_tags[1:] if t["datatype"] == vt["datatype"]]
        if fault == "address_offset" and same and rng.random() < 0.5:
            vt["register"], same[0]["register"] = same[0]["register"], vt["register"]  # two tags swapped
        elif fault == "address_offset":
            vt["register"] = vt["register"] + rng.choice([1, 2, 1, -1 if vt["register"] > 0 else 3])
        elif fault == "word_order":
            cs["word_order"] = "big" if order == "little" else "little"
        elif fault == "unit_id":
            vt["unit"] = rng.choice([u for u in (1, 2, 3, 5, 10) if u != unit])
            for t in cli_tags:
                t["unit"] = vt["unit"]
        elif fault == "scaling" and double:
            vt["scale"] = rng.choice([0.1, 0.01, 10])
        elif fault == "scaling":
            vt.pop("scale", None)
        elif fault == "register_table":
            vt["table"] = "discrete_input" if coil else "input_register"
            if coil:
                srv_tags[0]["initial"] = True
        elif fault == "datatype":
            vt["datatype"] = rng.choice(["int32", "uint32"])
        elif fault == "string_length":
            vt["length"] = rng.choice([2, 3, 4])
    elif kind == "eip":
        cs = {"host": "127.0.0.1", "port": port, "poll_ms": 300, "timeout_ms": 800}
        prog = rng.choice(["", "Program:Main.", "Program:Line1.", f"{area.capitalize()}."])
        for nm in names:
            dt = rng.choice(["float32", "int32", "int16", "bool", "string"])
            cip = prog + nm.capitalize() + rng.choice(["", "_PV", "_Act", f"[{rng.randrange(1, 9)}]", ".Value"])
            srv_tags.append({"name": nm, "datatype": dt, "cip_tag": cip, "initial": value_for(dt, rng)})
            cli_tags.append({"name": nm, "datatype": dt, "cip_tag": cip})
        if fault == "tag_name":
            cip = cli_tags[0]["cip_tag"]
            base = cip.rsplit(".", 1)[-1]
            r = rng.random()
            if r < 0.25 and "[" in cip:
                i = int(cip.split("[")[1].split("]")[0])
                cli_tags[0]["cip_tag"] = cip.replace(f"[{i}]", f"[{rng.choice([j for j in range(0, 10) if j != i])}]")
            elif r < 0.45 and cip.startswith("Program:"):
                cli_tags[0]["cip_tag"] = cip.split(".", 1)[1]  # program scope dropped
            else:
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
            if fault == "other":
                cli_tags[0]["pointer"] = rng.choice(["/value", "/data/" + victim, "/" + victim])
                why = rng.choice([
                    "the device publishes a plain value on that topic, but the client expects JSON and reads it with a JSON pointer",
                    "the payload is a bare number, not a JSON object, so the client's JSON pointer finds nothing",
                    "the client applies a JSON pointer to a payload that isn't JSON",
                ])
            if fault == "credentials":
                r = rng.random()
                if r < 0.25:
                    cs.pop("password")  # username without a password
                elif r < 0.4:
                    cs.pop("username"); cs.pop("password")  # no login at all
                elif r < 0.55:
                    cs["username"] = slip(cs["username"], rng)
                else:
                    cs["password"] = typo(cs["password"], rng)
            elif fault == "topic":
                parts = cli_tags[0]["topic"].split("/")
                r = rng.random()
                if r < 0.2 and len(parts) > 2:
                    parts[1] = "#"  # multi-level wildcard not last: matches nothing
                elif r < 0.35:
                    parts = parts[:1] + parts[2:]  # a level dropped
                else:
                    j = rng.randrange(len(parts))
                    parts[j] = slip(parts[j], rng)
                cli_tags[0]["topic"] = "/".join(parts)
        else:
            sp = {"group_id": rng.choice(["Plant", "SiteA", "Line"]), "edge_node": f"{area.capitalize()}{rng.randrange(1, 9)}"}
            if rng.random() < 0.4:
                sp["device"] = f"Dev{rng.randrange(1, 9)}"
            ss["sparkplug"] = dict(sp)
            cs["sparkplug"] = dict(sp)
            for nm in names:
                dt = rng.choice(["float64", "int32", "bool"])
                metric = f"{area.capitalize()}/{nm.capitalize()}"
                srv_tags.append({"name": nm, "datatype": dt, "metric": metric, "initial": value_for(dt, rng)})
                cli_tags.append({"name": nm, "datatype": dt, "metric": metric})
            if fault == "metric_name":
                m = cli_tags[0]["metric"]
                head, tail = m.split("/", 1)
                cli_tags[0]["metric"] = rng.choice([f"{head}/{typo(tail, rng)}", m.replace("/", "_"), tail])
            if fault == "sparkplug_ids":
                key = rng.choice([x for x in ("group_id", "edge_node", "device") if x in sp])
                cs["sparkplug"][key] = slip(sp[key], rng)
    elif kind == "api":
        ss["port"] = port
        cs = {"url": f"http://127.0.0.1:{port}/tags", "interval_ms": 500}
        for nm in names:
            dt = rng.choice(["float64", "int32", "bool", "string"])
            srv_tags.append({"name": nm, "datatype": dt, "initial": value_for(dt, rng)})
            cli_tags.append({"name": nm, "datatype": dt, "pointer": f"/{nm}"})
        if fault == "json_pointer":
            cli_tags[0]["pointer"] = "/" + (slip(victim, rng) if rng.random() < 0.8 else f"data/{victim}")
        elif fault == "port_or_host":
            cs["url"] = f"http://127.0.0.1:{wrong_port}/tags"
        elif fault == "url_path":
            cs["url"] = f"http://127.0.0.1:{port}" + rng.choice(["/tag", "/api/tags", "/Tags", "/values", "/tags.json"])
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
        if fault == "address_offset":
            for t in cli_tags:
                t["port"] = p % 4 + 1
        if fault == "other":
            cli_tags[0]["pd"] = "out"
            why = rng.choice([
                "the client reads process data out (master to device) instead of process data in, where the sensor value is",
                "the tag is set to pd out, but the sensor's value travels in process data in",
                "the client looks at the output direction of the IO-Link port; the measured value is in the input data",
            ])
        if fault == "bit_offset":
            cli_tags[0]["bit_offset"] += rng.choice([1, 4, 8, 16, -1 if cli_tags[0]["bit_offset"] else 2])

    elif kind == "opcua":
        ss = {"port": port, "hostname": "localhost", "security_policies": ["none"], "allow_anonymous": True}
        cs = {"url": f"opc.tcp://localhost:{port}/", "security_policy": "none", "security_mode": "none", "poll_ms": 500}
        if fault != "credentials" and rng.random() < 0.4:
            pw = f"pw{rng.randrange(10**6)}"
            ss.update(allow_anonymous=False, users=[{"username": "operator", "password": pw}])
            cs.update(username="operator", password=pw)
        for nm in names:
            dt = rng.choice(["float64", "int32", "bool", "string"])
            srv_tags.append({"name": nm, "datatype": dt, "initial": value_for(dt, rng)})
            cli_tags.append({"name": nm, "datatype": dt, "node_id": f"ns=2;s={nm}"})
        if fault == "node_id":
            r = rng.random()
            cli_tags[0]["node_id"] = (f"ns=2;s={slip(victim, rng)}" if r < 0.6 else
                                      f"ns={rng.choice([0, 1, 3])};s={victim}" if r < 0.85 else f"ns=2;i={rng.randrange(1000, 6000)}")
        elif fault == "datatype":
            srv_tags[0].update(datatype="float64", initial=round(rng.uniform(1, 500), 2) + 0.25)
            cli_tags[0]["datatype"] = rng.choice(["int32", "bool"])
        elif fault == "security_policy":
            cs.update(security_policy=rng.choice(["basic256sha256", "aes256_sha256_rsa_pss"]),
                      security_mode=rng.choice(["sign", "sign_and_encrypt"]), trust_server_certs=True)
        elif fault == "port_or_host":
            cs["url"] = f"opc.tcp://localhost:{wrong_port}/"
        elif fault == "credentials":
            ss.update(allow_anonymous=False, users=[{"username": "operator", "password": f"pw{rng.randrange(10**6)}"}])

    elif kind == "s7":
        cs = {"host": "127.0.0.1", "port": port, "poll_ms": 300, "timeout_ms": 800}
        db = rng.choice([1, 2, 3, 5, 10, 20])
        off = 0
        layout = []
        for nm in names:
            dt = rng.choice(["float32", "int32", "int16", "bool"])
            if nm == victim and fault == "datatype":
                dt = "float32"
            if nm == victim and fault == "string_length":
                dt = "string"
            if dt in ("float32", "int32"):
                addr, size = f"DB{db}.DBD{off}", 4
            elif dt == "int16":
                addr, size = f"DB{db}.DBW{off}", 2
            elif dt == "bool":
                addr, size = f"DB{db}.DBX{off}.{rng.randrange(8)}", 2
            else:
                addr, size = f"DB{db}.DBB{off}", 14
            st = {"name": nm, "datatype": dt, "s7_address": addr}
            if dt == "string":
                st["length"] = 12
                st["initial"] = f"B-{rng.randrange(2020, 2030)}-{rng.randrange(10000):04d}"
            else:
                st["initial"] = value_for(dt, rng)
            srv_tags.append(st)
            cli_tags.append({k2: v for k2, v in st.items() if k2 != "initial"})
            layout.append((dt, off, size))
            off += size
        vt, (vdt, voff, vsize) = cli_tags[0], layout[0]
        if fault == "address_offset":
            if rng.random() < 0.25 and vdt != "string":
                vt["s7_address"] = vt["s7_address"].replace(f"DB{db}.DB", "M", 1)  # merker instead of the DB
            elif rng.random() < 0.3:
                vt["s7_address"] = vt["s7_address"].replace(f"DB{db}.DB", f"DB{db + 1}.DB", 1)
            else:
                shifted = voff + rng.choice([2, 4, 8])
                vt["s7_address"] = {
                    "float32": f"DB{db}.DBD{shifted}", "int32": f"DB{db}.DBD{shifted}",
                    "int16": f"DB{db}.DBW{shifted}", "string": f"DB{db}.DBB{shifted}",
                    "bool": f"DB{db}.DBX{shifted}." + vt["s7_address"].rsplit(".", 1)[1],
                }[vdt]
        elif fault == "datatype":
            vt["datatype"] = "int32"
        elif fault == "string_length":
            vt["length"] = rng.choice([2, 4, 6])

    if fault == "port_or_host" and kind not in ("api", "opcua"):
        if "host" in cs and rng.random() < 0.4:
            cs["host"] = rng.choice(["127.0.0.2", "127.0.0.3", "127.0.1.1"])
        else:
            cs["port"] = wrong_port
    services = [
        {"id": sid, "kind": k, "mode": "server", "enabled": fault != "device_offline", "settings": ss, "tags": srv_tags},
        {"id": cid, "kind": k, "mode": "client", "settings": cs, "tags": cli_tags},
    ]
    who = {"none": "none", "device_offline": sid}.get(fault, cid)
    why = explain(fault, services, locals().get("why"), rng)
    answer = {"service": who, "cause": fault, "kind": kind, "why": why,
              "victim": victim, "prefix": f"g{n}-"}
    return services, answer


EFFECT = {
    "address_offset": ["so it reads a different value than the device holds", "so the value comes from the wrong place",
                       "which is why the reading is wrong"],
    "word_order": ["so multi-register values come out scrambled", "which garbles every 32-bit value",
                   "so the two halves of the value are swapped"],
    "unit_id": ["so the device ignores the requests", "so nothing answers for that unit", "and no device answers as that unit"],
    "scaling": ["so the value is off by the scale factor", "so the engineering value is wrong by a factor",
                "which is why the number is too big or too small"],
    "tag_name": ["so the controller doesn't know the tag", "and the controller has no tag by that name",
                 "so the read fails"],
    "port_or_host": ["so the client can't reach the device", "so it connects to nothing", "and the connection fails"],
    "credentials": ["so the server refuses the connection", "and the login is rejected", "so it never gets data"],
    "topic": ["so the client never receives the value", "and nothing is published there", "so the subscription stays empty"],
    "sparkplug_ids": ["so the host never matches the node's births", "so the metrics never arrive",
                      "and the host ignores the node"],
    "json_pointer": ["so the pointer finds nothing in the response", "and the value isn't at that place in the JSON",
                     "so the client reads nothing"],
    "bit_offset": ["so the value is cut from the wrong bits", "which shifts the process data",
                   "so the decoded value is wrong"],
    "register_table": ["so it reads a different table than the one holding the value", "and that table holds nothing there",
                       "so the value comes from the wrong table"],
    "datatype": ["so the bytes are decoded as the wrong type", "which turns the value into garbage",
                 "so the number is misread"],
    "string_length": ["so the string is cut short", "and part of the text is missing", "so only part of the string arrives"],
    "node_id": ["so the server has no such node", "and the read fails", "so the client gets no value"],
    "security_policy": ["so no matching endpoint exists", "and the connection can't be secured that way",
                        "so the client can't connect"],
    "metric_name": ["so the host never matches the metric", "and the value never arrives", "so the metric is missing"],
    "url_path": ["so the server answers 404", "and that path doesn't exist", "so the request fails"],
    "protocol_mismatch": ["so the two never understand each other", "and the device can't answer it",
                          "so the connection fails"],
    "device_offline": ["so every client of it loses its data", "and nothing answers on its port",
                       "so the client has nothing to talk to"],
}


def explain(fault, services, why, rng):
    """A one- or two-sentence explanation in varied wording, citing the actual difference."""
    if fault == "none":
        return rng.choice([
            "The client's settings match the device and its values are good, so nothing is misconfigured.",
            "Nothing is wrong: the client reads the same values the device reports.",
            "The reading is correct and the client matches the device; no change is needed.",
        ])
    if why:
        return why[0].upper() + why[1:] + "."
    defs = [dict(s, status={"state": "running" if s.get("enabled", True) else "stopped"}) for s in services]
    found = bench.rule_checks(defs)
    effect = rng.choice(EFFECT.get(fault, ["which explains the symptom"]))
    if not found:
        return f"The client's settings differ from the device's ({fault.replace('_', ' ')}), {effect}."
    f = found[0].rstrip(".")
    style = rng.randrange(3)
    if style == 0:
        return f"{f}, {effect}."
    if style == 1:
        return f"{effect[0].upper() + effect[1:]}: {f[0].lower() + f[1:]}."
    return f"The problem is here: {f[0].lower() + f[1:]}. That is why the reading is wrong."


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
                        hide = rules and ans["cause"] != "none" and rng.random() < HIDE_RULES
                        if hide:
                            state = (f"Operator report: {sym}\n\nServices:\n" + bench.evidence(cp, ans["prefix"], False)
                                     + "\n\nRule checks (client compared with the device's simulator):\n"
                                     + "- No differences between the client and the device settings.")
                        else:
                            state = f"Operator report: {sym}\n\nServices:\n" + bench.evidence(cp, ans["prefix"], rules)
                        for ex in decisions(state, ans, ids, rng):
                            ex["meta"] = {"kind": ans["kind"], "cause": ans["cause"], "rules": rules, "seed": a.seed,
                                          "rules_hidden": hide, "why": ans.get("why")}
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
