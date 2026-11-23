#!/usr/bin/env python3
"""Turn gen.py's decision data into chat fine-tuning data for the evidence mode.

Each scenario variant (with or without rule findings) becomes one conversation:
system prompt, the evidence as the user turn, and the JSON diagnosis bench.py's
evidence mode expects as the assistant turn. The explanation cites the rule findings
when they are present, otherwise the symptom, so the model learns to quote evidence.

  python3 bench/foreman/to_chat.py data/faults-s1.jsonl data/train.jsonl data/valid.jsonl --valid 0.05
"""

import argparse
import json
import random
import re

import bench
import gen

SYSTEM = bench.SYSTEM.split("When you know")[0] + (
    "Answer with JSON: service (id or none), cause (one of " + ", ".join(bench.CAUSES) + "), explanation, fix."
)

FIX = {
    "address_offset": "Set the client's register or address to match the device.",
    "word_order": "Set the client's word order to match the device.",
    "unit_id": "Set the client's unit id to the device's.",
    "scaling": "Give the client the device's scale and offset.",
    "tag_name": "Correct the PLC tag name in the client.",
    "port_or_host": "Point the client at the device's host and port.",
    "credentials": "Give the client the username and password the device requires.",
    "topic": "Correct the topic the client subscribes to.",
    "sparkplug_ids": "Make the client's Sparkplug group, edge node and device ids match the publisher's.",
    "json_pointer": "Correct the client's JSON pointer to where the device serves the value.",
    "bit_offset": "Set the client's bit offset and length to match the device's process data.",
    "register_table": "Set the client's table to the one the device uses.",
    "datatype": "Set the client's data type to the device's.",
    "string_length": "Set the string length to the device's.",
    "node_id": "Correct the OPC UA node id in the client.",
    "security_policy": "Choose a security policy and mode the server offers.",
    "device_offline": "Start the device (server) or restore its power and network.",
    "metric_name": "Correct the Sparkplug metric name in the client.",
    "url_path": "Correct the URL path the client calls.",
    "protocol_mismatch": "Point the client at the right device, or use a client of the device's protocol.",
    "other": "See the explanation.",
    "none": "No change needed; the client matches the device.",
}


def diagnosis(state, service, cause):
    rules = []
    if "Rule checks" in state:
        rules = [l[2:] for l in state.split("Rule checks", 1)[1].splitlines()[1:] if l.startswith("- ")]
        rules = [r for r in rules if not r.startswith("No differences")]
    symptom = state.split("\n", 1)[0].removeprefix("Operator report: ")
    if cause == "none":
        why = "The client's settings match the device and its values are good; " + (
            "the rule checks found no differences." if "Rule checks" in state else "nothing points to a fault.")
    elif rules:
        why = " ".join(rules[:2])
    else:
        why = f"{symptom} The client's settings differ from the device's ({cause.replace('_', ' ')})."
    return {"service": service, "cause": cause, "explanation": why, "fix": FIX[cause]}


def grounded(state, cause, default, rng):
    """v5 explanation: cite a finding only when the evidence shows it, else stay short and generic."""
    if cause in ("none", "other"):
        return default
    lines = []
    if "Rule checks" in state:
        lines = [l[2:].rstrip(".") for l in state.split("Rule checks", 1)[1].splitlines()[1:] if l.startswith("- ")]
        lines = [l for l in lines if not l.startswith("No differences")]
    effect = rng.choice(gen.EFFECT.get(cause, ["which explains the symptom"]))
    if lines:
        return f"{lines[0]}, {effect}."
    if cause == "device_offline":
        return f"The device's server is not running, {effect}."
    if cause == "protocol_mismatch":
        return f"The client speaks a different protocol than the device on that port, {effect}."
    return f"The client's {cause.replace('_', ' ')} setting does not match the device, {effect}."


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("src")
    ap.add_argument("train")
    ap.add_argument("valid")
    ap.add_argument("--valid-frac", type=float, default=0.05)
    ap.add_argument("--limit", type=int, default=0, help="keep at most this many conversations")
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--grounded", action="store_true",
                    help="v5: quote only rule findings shown in the evidence; otherwise one short sentence")
    a = ap.parse_args()

    # gen.py writes, per scenario and variant, the service question then the cause question.
    rng = random.Random(a.seed)
    pending, convs = {}, []
    for line in open(a.src):
        e = json.loads(line)
        key = (e["state"], e["meta"]["rules"])
        if e["question"].startswith("Which service"):
            pending[key] = e["answer_key"]
        elif key in pending:
            service = pending.pop(key)
            d = diagnosis(e["state"], service, e["answer_key"])
            # gen.py v4 writes a varied explanation per fault; keep the rule lines sometimes.
            why = e["meta"].get("why")
            has_lines = "Rule checks" in e["state"] and "- No differences" not in e["state"]
            if a.grounded:
                d["explanation"] = grounded(e["state"], e["answer_key"], d["explanation"], rng)
            elif why and not (has_lines and e["answer_key"] not in ("other", "none") and rng.random() < 0.3):
                d["explanation"] = why if why.endswith(".") else why[0].upper() + why[1:] + "."
            convs.append({"messages": [
                {"role": "system", "content": SYSTEM},
                {"role": "user", "content": e["state"]},
                {"role": "assistant", "content": json.dumps(d)},
            ], "meta": e["meta"]})
    rng.shuffle(convs)
    if a.limit:
        convs = convs[: a.limit]
    nv = max(1, int(len(convs) * a.valid_frac))
    with open(a.valid, "w") as f:
        for c in convs[:nv]:
            f.write(json.dumps({"messages": c["messages"]}) + "\n")
    with open(a.train, "w") as f:
        for c in convs[nv:]:
            f.write(json.dumps({"messages": c["messages"]}) + "\n")
    print(f"{len(convs) - nv} train, {nv} valid conversations")


if __name__ == "__main__":
    main()
