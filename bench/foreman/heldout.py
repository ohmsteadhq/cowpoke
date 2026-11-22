#!/usr/bin/env python3
"""Held-out test for the foreman: faults the fine-tuned models never trained on.

Training data came from gen.py with seed 1: one device and one client per scenario, and
gen.py's own symptom wording. This builds three sets with a different seed, stages them in
Cowpoke, snapshots the evidence, then asks each model:

- fresh:      gen.py scenarios, new seed. Same kind of fault, new instances.
- distractor: the same, plus two healthy device/client pairs under the same prefix, so the
              model must pick the broken service out of six instead of two.
- reworded:   fresh scenarios with the operator report reworded the way people talk.

  python3 bench/foreman/heldout.py stage --per-cause 4 --seed 777      # writes data/heldout-777.jsonl
  python3 bench/foreman/heldout.py ask --model cowpoke-foreman:4b-s500 [--rules]

Results go to results/heldout-<model>[-rules].json.
"""
import argparse
import json
import os
import random
import re
import shutil
import signal
import subprocess
import tempfile
import time
import urllib.request

import bench
import gen

HERE = os.path.dirname(os.path.abspath(__file__))
CAUSES = sorted({c for cs in gen.PROTOCOL_FAULTS.values() for c in cs})


def rename(services, old, new):
    """Move a scenario's services from prefix `old` to `new`."""
    for s in services:
        s["id"] = new + s["id"][len(old):]
    return services


def reword(text, rng):
    """The same facts, in the words an operator might use."""
    m = re.match(r"The (\S+) (\S+) never shows a value on the HMI\.", text)
    if m:
        a, n = m.groups()
        return rng.choice([f"Nothing comes through for {n} on the {a} screen. Blank since we started up.",
                           f"{a.capitalize()} {n} is empty on SCADA, no number at all."])
    m = re.match(r"The (\S+) (\S+) shows bad quality\.", text)
    if m:
        a, n = m.groups()
        return rng.choice([f"{a.capitalize()} {n} is flagged red on the panel, comms fault I think.",
                           f"SCADA has {n} on the {a} greyed out with a quality alarm."])
    m = re.match(r"The (\S+) (\S+) reads (.+), but the (?:local display|device) shows (.+)\.", text)
    if m:
        a, n, got, want = m.groups()
        return rng.choice([f"Panel at the {a} says {want} for {n}, SCADA is showing {got}. Which one is right?",
                           f"{n} on the {a}: the gauge on the machine reads {want} but we log {got}.",
                           f"Our {a} {n} trend sits at {got}. Walked out to the unit and it shows {want}."])
    m = re.match(r".*the (\S+) (\S+)", text)
    a, n = m.groups() if m else ("line", "value")
    return rng.choice([f"QA asked us to double-check the {a} {n}. Looks fine to me?",
                       f"Can someone confirm {a} {n} is reading right before the audit?"])


def stage(a):
    rng = random.Random(a.seed)
    password = f"ho-{rng.randrange(10**9)}"
    plan = []  # (set, faulty scenario, distractor pairs)
    n = 0
    for cause in CAUSES:
        for _ in range(a.per_cause):
            for which in ("fresh", "distractor", "reworded"):
                n += 1
                plan.append((which, n, cause))
    rng.shuffle(plan)
    out_path = os.path.join(HERE, "data", f"heldout-{a.seed}.jsonl")
    os.makedirs(os.path.dirname(out_path), exist_ok=True)
    written = 0
    with open(out_path, "w") as out:
        i = 0
        while i < len(plan):
            # Each pair takes a port and port + 50 as its wrong port: stay under 50 pairs a batch.
            batch, pairs = [], 0
            while i < len(plan) and pairs + 3 <= 45:
                batch.append(plan[i])
                pairs += 3 if plan[i][0] == "distractor" else 1
                i += 1
            port = a.base_port
            scen, meta = [], []
            for which, n, cause in batch:
                svcs, ans = gen.make_scenario(n, port, rng, fault=cause)
                port += 1
                if which == "distractor":
                    for j in range(2):
                        kind = rng.choice(sorted(gen.PROTOCOL_FAULTS))
                        extra, _ = gen.make_scenario(10_000 + n * 10 + j, port, rng, kind=kind, fault="none")
                        port += 1
                        rename(extra, f"g{10_000 + n * 10 + j}-", f"g{n}-x{j}")
                        svcs = svcs + extra
                scen.append((svcs, ans))
                meta.append(which)
            work = tempfile.mkdtemp(prefix="cowpoke-heldout-")
            proc = None
            try:
                runs, proc = gen.run_batch(scen, work, a.web_port, password, a.settle)
                for (cp, svcs, ans), which in zip(runs, meta):
                    sym = gen.symptom(cp, ans, svcs[:2], rng)
                    if which == "reworded":
                        sym = reword(sym, rng)
                    ev = {r: bench.evidence(cp, ans["prefix"], r) for r in (False, True)}
                    out.write(json.dumps({"set": which, "prefix": ans["prefix"], "kind": ans["kind"],
                                          "service": ans["service"], "cause": ans["cause"], "symptom": sym,
                                          "evidence": ev[False],
                                          "evidence_rules": ev[True]}) + "\n")
                    written += 1
            finally:
                if proc:
                    proc.send_signal(signal.SIGTERM)
                    try:
                        proc.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                shutil.rmtree(work, ignore_errors=True)
            print(f"{written}/{len(plan)} staged", flush=True)
    print(out_path)


def ask_model(url, model, text):
    body = {"model": model, "stream": False, "options": {"temperature": 0, "num_ctx": 8192, "num_predict": 400},
            "messages": [{"role": "system", "content": bench.SYSTEM.split("When you know")[0] +
                          "Answer with JSON: service (id or none), cause (one of " + ", ".join(bench.CAUSES) +
                          "), explanation, fix."},
                         {"role": "user", "content": text}]}
    t0 = time.time()
    req = urllib.request.Request(url.rstrip("/") + "/api/chat", data=json.dumps(body).encode(),
                                 headers={"content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=600) as r:
        res = json.loads(r.read())
    return bench.first_json(res.get("message", {}).get("content", "")), round(time.time() - t0, 2)


def ask(a):
    rows = [json.loads(l) for l in open(os.path.join(HERE, "data", f"heldout-{a.seed}.jsonl"))]
    results = []
    for k, row in enumerate(rows):
        text = f"Operator report: {row['symptom']}\n\nServices:\n" + row["evidence_rules" if a.rules else "evidence"]
        rep, secs = ask_model(a.ollama, a.model, text)
        ok_s = bool(rep) and rep.get("service") == row["service"]
        ok_c = bool(rep) and rep.get("cause") == row["cause"]
        results.append({**{x: row[x] for x in ("set", "prefix", "kind", "service", "cause", "symptom")},
                        "report": rep, "service_ok": ok_s, "cause_ok": ok_c, "pass": ok_s and ok_c, "seconds": secs})
        print(f"{k + 1:>3}/{len(rows)} {row['set']:<10} {'PASS' if ok_s and ok_c else 'fail'} "
              f"{row['cause']:<15} -> {(rep or {}).get('cause', '-')}", flush=True)
    summary = {"model": a.model, "rules": a.rules, "seed": a.seed}
    for which in ("fresh", "distractor", "reworded", "all"):
        rs = [r for r in results if which == "all" or r["set"] == which]
        summary[which] = {"n": len(rs), "passed": sum(r["pass"] for r in rs),
                          "service_ok": sum(r["service_ok"] for r in rs), "cause_ok": sum(r["cause_ok"] for r in rs)}
    summary["avg_seconds"] = round(sum(r["seconds"] for r in results) / len(results), 2)
    by_cause = {}
    for r in results:
        c = by_cause.setdefault(r["cause"], [0, 0])
        c[0] += r["pass"]
        c[1] += 1
    summary["by_cause"] = {c: f"{p}/{t}" for c, (p, t) in sorted(by_cause.items())}
    print(json.dumps(summary, indent=1))
    label = "heldout-" + a.model.replace(":", "-").replace("/", "-") + ("-rules" if a.rules else "")
    with open(os.path.join(HERE, "results", label + ".json"), "w") as f:
        json.dump({"summary": summary, "results": results}, f, indent=1)


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("stage")
    s.add_argument("--per-cause", type=int, default=4)
    s.add_argument("--seed", type=int, default=777)
    s.add_argument("--base-port", type=int, default=32000)
    s.add_argument("--web-port", type=int, default=18191)
    s.add_argument("--settle", type=float, default=6.0)
    q = sub.add_parser("ask")
    q.add_argument("--model", required=True)
    q.add_argument("--rules", action="store_true")
    q.add_argument("--seed", type=int, default=777)
    q.add_argument("--ollama", default="http://127.0.0.1:11434")
    a = ap.parse_args()
    if a.seed == 1:
        raise SystemExit("seed 1 is the training seed; pick another")
    stage(a) if a.cmd == "stage" else ask(a)


if __name__ == "__main__":
    main()
