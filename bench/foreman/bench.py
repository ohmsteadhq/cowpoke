#!/usr/bin/env python3
"""Foreman benchmark: can a small local model diagnose staged Cowpoke faults?

Start Cowpoke with faults.yaml, then for each scenario in scenarios.json give the model
the operator's symptom and four read-only tools over Cowpoke's REST API. The model must
call `report` with the broken service and the cause, picked from a fixed list.
A scenario passes when both are right.

  COWPOKE_CONFIG=bench/foreman/faults.yaml COWPOKE_ADMIN_PASSWORD=... cargo run   # or the binary
  python3 bench/foreman/bench.py --model qwen3:4b --cowpoke http://127.0.0.1:18090 --password ...

Models are served by Ollama (native /api/chat with tools). Results go to results/<label>.json.
"""

import argparse
import re
import subprocess
import http.cookiejar
import json
import os
import sys
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))

CAUSES = [
    "address_offset", "word_order", "unit_id", "scaling", "tag_name", "port_or_host",
    "credentials", "topic", "sparkplug_ids", "json_pointer", "bit_offset", "none",
]

SYSTEM = """You are the foreman of a Cowpoke box, an industrial gateway that runs protocol services \
(Modbus TCP, OPC UA, HTTP API, IO-Link, MQTT with Sparkplug B, EtherNet/IP). Each service is a \
server (a simulated device) or a client (talks to a device). You diagnose problems an operator reports.

Use the tools to look at the services before you answer. Your tools are read-only. \
In this plant each device is simulated by a Cowpoke server service, so the server's definition \
shows how the device is really set up; compare the client's settings and tags against it.

When you know the answer, call `report` exactly once with:
- service: the id of the misconfigured service, or "none" if nothing is wrong
- cause: one of """ + ", ".join(CAUSES) + """
- explanation: one or two sentences citing the evidence
- fix: the change to make"""

TOOLS = [
    {"type": "function", "function": {
        "name": "list_services",
        "description": "List services with kind, mode, state (running, degraded, error, stopped) and status detail.",
        "parameters": {"type": "object", "properties": {
            "prefix": {"type": "string", "description": "Only ids starting with this, e.g. 'f3-'"}}}}},
    {"type": "function", "function": {
        "name": "get_service",
        "description": "A service's full definition: settings and tags with their addresses.",
        "parameters": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}}},
    {"type": "function", "function": {
        "name": "read_tags",
        "description": "A service's live tag values with quality (good or bad) and timestamps.",
        "parameters": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}}},
    {"type": "function", "function": {
        "name": "report",
        "description": "Give your final diagnosis. Call once, at the end.",
        "parameters": {"type": "object", "properties": {
            "service": {"type": "string"},
            "cause": {"type": "string", "enum": CAUSES},
            "explanation": {"type": "string"},
            "fix": {"type": "string"}},
            "required": ["service", "cause", "explanation", "fix"]}}},
]


CAUSE_TEXT = {
    "address_offset": "The client reads the wrong register or address",
    "word_order": "Word or byte order of multi-register values differs",
    "unit_id": "The client asks the wrong Modbus unit id",
    "scaling": "A scale factor or offset is missing or wrong",
    "tag_name": "The PLC tag name is wrong or misspelled",
    "port_or_host": "The client connects to the wrong host or port",
    "credentials": "Wrong username or password",
    "topic": "Wrong MQTT topic",
    "sparkplug_ids": "Sparkplug group, edge node or device id mismatch",
    "json_pointer": "Wrong JSON pointer into the response",
    "bit_offset": "Wrong IO-Link bit offset or length",
    "none": "Nothing is wrong; the values match the device",
}


# Defaults Cowpoke applies when a field is left out, so "missing" and "default" compare equal.
TAG_DEFAULTS = {"unit": 1, "table": "holding_register", "scale": 1, "offset": 0, "bit_offset": 0, "pd": "in"}
TAG_FIELDS = ["register", "table", "unit", "length", "scale", "offset", "port", "pd", "bit_offset", "bit_length",
              "node_id", "datatype"]
SETTING_FIELDS = ["word_order"]


def _port_of(settings):
    if settings.get("port"):
        return settings["port"]
    m = re.match(r"https?://[^/:]+:(\d+)", settings.get("url") or "")
    return int(m.group(1)) if m else None


def rule_checks(defs):
    """Compare each client with the server (simulated device) of the same ticket, field by field.
    Returns plain findings; secrets are compared but never shown."""
    out = []
    clients = [d for d in defs if d["mode"] == "client"]
    servers = [d for d in defs if d["mode"] == "server"]
    for c in clients:
        for sv in [x for x in servers if x["kind"] == c["kind"]]:
            cs, ss = c.get("settings") or {}, sv.get("settings") or {}
            cp_, sp_ = _port_of(cs), _port_of(ss)
            if cp_ and sp_ and cp_ != sp_:
                out.append(f"{c['id']} connects to port {cp_}, but {sv['id']} listens on port {sp_}.")
            for k in SETTING_FIELDS:
                a, b = cs.get(k, "big"), ss.get(k, "big")
                if a != b:
                    out.append(f"{c['id']} uses {k} {a}, but {sv['id']} uses {b}.")
            for k in ("username", "password"):
                if ss.get(k) is not None and cs.get(k) != ss.get(k):
                    out.append(f"{c['id']}'s {k} does not match the one {sv['id']} requires.")
            csp, ssp = cs.get("sparkplug") or {}, ss.get("sparkplug") or {}
            for k in ("group_id", "edge_node", "device"):
                if (csp or ssp) and csp.get(k) != ssp.get(k):
                    out.append(f"Sparkplug {k}: {c['id']} expects {csp.get(k)!r}, {sv['id']} publishes {ssp.get(k)!r}.")
            stags = {t["name"]: t for t in sv.get("tags") or []}
            for t in c.get("tags") or []:
                st = stags.get(t["name"])
                if not st:
                    continue
                for k in TAG_FIELDS:
                    a, b = t.get(k, TAG_DEFAULTS.get(k)), st.get(k, TAG_DEFAULTS.get(k))
                    if a != b:
                        out.append(f"Tag {t['name']}: {c['id']} has {k} {a}, but {sv['id']} has {b}.")
                if c["kind"] == "eip":
                    a, b = (t.get("cip_tag") or t["name"]), (st.get("cip_tag") or st["name"])
                    if a.lower() != b.lower():
                        out.append(f"Tag {t['name']}: {c['id']} reads Logix tag {a}, but {sv['id']} has {b}.")
                if c["kind"] == "mqtt" and not csp and t.get("topic"):
                    want = st.get("topic") or f"{(ss.get('prefix') or sv['id']).rstrip('/')}/{st['name']}"
                    if t["topic"] != want:
                        out.append(f"Tag {t['name']}: {c['id']} subscribes to {t['topic']}, but {sv['id']} publishes it on {want}.")
                if c["kind"] == "mqtt" and csp:
                    a, b = t.get("metric") or t["name"], st.get("metric") or st["name"]
                    if a != b:
                        out.append(f"Tag {t['name']}: {c['id']} reads metric {a}, but {sv['id']} publishes {b}.")
                if c["kind"] == "api":
                    a, b = t.get("pointer") or f"/{t['name']}", f"/{st['name']}"
                    if a != b:
                        out.append(f"Tag {t['name']}: {c['id']} reads JSON pointer {a}, but {sv['id']} serves it at {b}.")
    return out


def evidence(cp, prefix, rules=False):
    """What Cowpoke itself gathers before asking a model: every service of the ticket,
    its definition, status and live tags, as compact text."""
    parts, defs = [], []
    for s in cp.tool("list_services", {"prefix": prefix}):
        d = cp.tool("get_service", {"id": s["id"]})
        defs.append(d)
        tags = [{k: t.get(k) for k in ("name", "value", "quality", "units")} for t in cp.tool("read_tags", {"id": s["id"]})]
        parts.append(json.dumps({"id": s["id"], "kind": s["kind"], "mode": s["mode"], "state": s["state"],
                                 "detail": s["detail"], "settings": d.get("settings"), "tags_config": d.get("tags"),
                                 "live_tags": tags}))
    text = "\n".join(parts)
    if rules:
        found = rule_checks(defs)
        text += "\n\nRule checks (client compared with the device's simulator):\n" + (
            "\n".join("- " + f for f in found) if found else "- No differences between the client and the device settings.")
    return text


ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")


def ask(model, question, choices, text):
    """One decision with Ollama's --ask/--choice: returns (label, confidence %)."""
    cmd = ["ollama", "run", model, "--ask", question]
    for k, v in choices.items():
        cmd += ["--choice", f"{k}={v}"]
    try:
        out = subprocess.run(cmd, input=text, capture_output=True, text=True, timeout=600).stdout
    except subprocess.TimeoutExpired:
        return (None, None)
    out = ANSI.sub("", out).replace("\r", "\n")
    hits = re.findall(r"([A-Za-z0-9_.-]+)\s+(\d+)%", out)
    return (hits[-1][0], int(hits[-1][1])) if hits else (None, None)


def run_decide(sc, cp, model, rules=False):
    """Decision-model mode: Cowpoke gathers the evidence; the model makes two choices."""
    t0 = time.time()
    text = f"Operator report: {sc['symptom']}\n\nServices:\n" + evidence(cp, sc["id"] + "-", rules)
    ids = [s["id"] for s in cp.tool("list_services", {"prefix": sc["id"] + "-"})]
    svc_choices = {i: f"Service {i} is misconfigured" for i in ids}
    svc_choices["none"] = "Nothing is misconfigured"
    service, sconf = ask(model, "Which service is misconfigured?", svc_choices, text)
    cause, cconf = ask(model, "What is the cause of the problem?", CAUSE_TEXT, text)
    report = {"service": service, "cause": cause, "confidence": {"service": sconf, "cause": cconf}}
    return report, round(time.time() - t0, 1)


def first_json(text):
    """The first JSON object in a reply (after any <think> block)."""
    text = re.sub(r"<think>.*?</think>", "", text or "", flags=re.S)
    start = text.find("{")
    while start >= 0:
        depth = 0
        for i in range(start, len(text)):
            depth += {"{": 1, "}": -1}.get(text[i], 0)
            if depth == 0:
                try:
                    return json.loads(text[start:i + 1])
                except json.JSONDecodeError:
                    break
        start = text.find("{", start + 1)
    return None


FREE_FORM = False  # --no-format: runners without structured output (Ollama's MLX runner)
OPENAI = None  # --openai URL: an OpenAI-compatible server (e.g. mlx_lm.server) instead of Ollama


def run_evidence(sc, cp, url, model, think, rules=False):
    """The same pre-gathered evidence for a chat model, answered as JSON in one call."""
    t0 = time.time()
    text = f"Operator report: {sc['symptom']}\n\nServices:\n" + evidence(cp, sc["id"] + "-", rules)
    schema = TOOLS[3]["function"]["parameters"]
    # A diagnosis is ~100 tokens; the cap stops a model that keeps writing after its JSON.
    body = {"model": model, "stream": False, "options": {"temperature": 0, "num_ctx": 8192, "num_predict": 400},
            "messages": [{"role": "system", "content": SYSTEM.split("When you know")[0] +
                          "Answer with JSON: service (id or none), cause (one of " + ", ".join(CAUSES) + "), explanation, fix."},
                         {"role": "user", "content": text}]}
    if OPENAI:
        # OpenAI-compatible chat completions: no schema; the first JSON object is read.
        # mlx_lm.server serves its --model (with --adapter-path) as "default_model"; naming the
        # repo instead would make it load the base model without the adapter.
        oa = {"model": "default_model", "messages": body["messages"], "temperature": 0, "max_tokens": 400}
        req = urllib.request.Request(OPENAI.rstrip("/") + "/v1/chat/completions", data=json.dumps(oa).encode(),
                                     headers={"content-type": "application/json"})
        with urllib.request.urlopen(req, timeout=600) as r:
            res = json.loads(r.read())
        content = res["choices"][0]["message"].get("content") or ""
        n = res.get("usage", {}).get("completion_tokens", 0)
        secs = round(time.time() - t0, 1)
        return first_json(content), secs, round(n / secs, 1) if secs else None
    if not FREE_FORM:
        body["format"] = schema
    if think is not None:
        body["think"] = think
    req = urllib.request.Request(url.rstrip("/") + "/api/chat", data=json.dumps(body).encode(),
                                 headers={"content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=600) as r:
        res = json.loads(r.read())
    report = first_json(res.get("message", {}).get("content", ""))
    tok_s = round(res.get("eval_count", 0) / (res["eval_duration"] / 1e9), 1) if res.get("eval_duration") else None
    return report, round(time.time() - t0, 1), tok_s


class Cowpoke:
    def __init__(self, base, user, password):
        self.base = base.rstrip("/")
        self.opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))
        self.call("POST", "/api/login", {"username": user, "password": password})

    def call(self, method, path, body=None, timeout=10):
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(self.base + path, data=data, method=method,
                                     headers={"content-type": "application/json"})
        with self.opener.open(req, timeout=timeout) as r:
            text = r.read().decode()
            return json.loads(text) if text else None

    def tool(self, name, args):
        try:
            if name == "list_services":
                prefix = args.get("prefix") or ""
                out = []
                for s in self.call("GET", "/api/services"):
                    if not s["id"].startswith(prefix):
                        continue
                    st = s.get("status") or {}
                    out.append({"id": s["id"], "kind": s["kind"], "mode": s["mode"],
                                "state": st.get("state"), "detail": st.get("detail")})
                return out
            if name == "get_service":
                d = self.call("GET", "/api/services/" + args["id"])
                df = d.get("definition") or {}
                return {"id": d["id"], "kind": d["kind"], "mode": d["mode"], "status": d.get("status"),
                        "settings": df.get("settings"), "tags": df.get("tags")}
            if name == "read_tags":
                return self.call("GET", f"/api/services/{args['id']}/tags")
            return {"error": f"unknown tool {name}"}
        except KeyError as e:
            return {"error": f"missing argument {e}"}
        except urllib.error.HTTPError as e:
            return {"error": f"HTTP {e.code}: {e.read().decode()[:200]}"}


def chat(url, model, messages, think):
    # Cap each reply: a model stuck writing (instead of calling a tool) would run for hours.
    body = {"model": model, "messages": messages, "tools": TOOLS, "stream": False,
            "options": {"temperature": 0, "num_ctx": 8192, "num_predict": 600}}
    if think is not None:
        body["think"] = think
    req = urllib.request.Request(url.rstrip("/") + "/api/chat", data=json.dumps(body).encode(),
                                 headers={"content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=600) as r:
        return json.loads(r.read())


def run_scenario(sc, cp, url, model, think, max_steps):
    messages = [
        {"role": "system", "content": SYSTEM},
        {"role": "user", "content": f"{sc['symptom']}\nThe services for this ticket have ids starting with '{sc['id']}-'."},
    ]
    trace, invalid, report = [], 0, None
    gen_tokens, gen_ns, t0 = 0, 0, time.time()
    for _ in range(max_steps):
        r = chat(url, model, messages, think)
        gen_tokens += r.get("eval_count", 0)
        gen_ns += r.get("eval_duration", 0)
        msg = r["message"]
        messages.append({k: v for k, v in msg.items() if k in ("role", "content", "tool_calls")})
        calls = msg.get("tool_calls") or []
        if not calls:
            # No tool call: nudge once per step toward the report.
            invalid += 1
            trace.append({"text": (msg.get("content") or "")[:300]})
            messages.append({"role": "user", "content": "Use the tools, then call `report`."})
            continue
        for c in calls:
            name = c["function"]["name"]
            args = c["function"].get("arguments") or {}
            if isinstance(args, str):
                try:
                    args = json.loads(args)
                except json.JSONDecodeError:
                    args = {}
            trace.append({"tool": name, "args": args})
            if name == "report":
                report = args
                break
            known = name in ("list_services", "get_service", "read_tags")
            if not known or (name != "list_services" and "id" not in args):
                invalid += 1
            result = cp.tool(name, args)
            messages.append({"role": "tool", "content": json.dumps(result)[:6000]})
        if report:
            break
    ok_service = bool(report) and report.get("service") == sc["service"]
    ok_cause = bool(report) and report.get("cause") == sc["cause"]
    inspected = any(t.get("tool") == "get_service" and t["args"].get("id") == sc["service"] for t in trace)
    return {
        "id": sc["id"], "expected": {"service": sc["service"], "cause": sc["cause"]},
        "report": report, "pass": ok_service and ok_cause, "service_ok": ok_service, "cause_ok": ok_cause,
        "inspected_faulty": inspected, "steps": sum(1 for t in trace if "tool" in t), "invalid": invalid,
        "seconds": round(time.time() - t0, 1),
        "tok_s": round(gen_tokens / (gen_ns / 1e9), 1) if gen_ns else None,
        "trace": trace,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", required=True)
    ap.add_argument("--label")
    ap.add_argument("--ollama", default="http://127.0.0.1:11434")
    ap.add_argument("--cowpoke", default="http://127.0.0.1:18090")
    ap.add_argument("--user", default="admin")
    ap.add_argument("--password", default=os.environ.get("COWPOKE_ADMIN_PASSWORD", ""))
    ap.add_argument("--think", choices=["on", "off", "default"], default="off",
                    help="thinking for models that have it (qwen3); default leaves it to the model")
    ap.add_argument("--only", help="comma-separated scenario ids")
    ap.add_argument("--max-steps", type=int, default=8)
    ap.add_argument("--openai", help="evidence mode via an OpenAI-compatible server at this URL (e.g. mlx_lm.server)")
    ap.add_argument("--no-format", action="store_true",
                    help="evidence mode: don't constrain the reply to the JSON schema; read the first JSON object")
    ap.add_argument("--rules", action="store_true", help="add Cowpoke's rule checks to the evidence (evidence/decide)")
    ap.add_argument("--mode", choices=["agent", "evidence", "decide", "foreman"], default="agent",
                    help="agent: the model calls tools itself; evidence: Cowpoke gathers the evidence and a chat "
                         "model answers once; decide: the same evidence to a decision model (--ask/--choice)")
    a = ap.parse_args()

    think = {"on": True, "off": False, "default": None}[a.think]
    global FREE_FORM, OPENAI
    FREE_FORM = a.no_format
    OPENAI = a.openai
    scenarios = json.load(open(os.path.join(HERE, "scenarios.json")))
    if a.only:
        keep = set(a.only.split(","))
        scenarios = [s for s in scenarios if s["id"] in keep]
    cp = Cowpoke(a.cowpoke, a.user, a.password)
    results = []
    for sc in scenarios:
        if a.mode != "agent":
            tok_s = None
            if a.mode == "foreman":
                # Cowpoke's own foreman: it gathers the evidence and asks its configured model.
                t0 = time.time()
                r0 = cp.call("POST", "/api/foreman/diagnose",
                             {"prefix": sc["id"] + "-", "symptom": sc["symptom"], "rules": a.rules}, timeout=600)
                report, secs = r0.get("diagnosis"), round(time.time() - t0, 1)
            elif a.mode == "decide":
                report, secs = run_decide(sc, cp, a.model, a.rules)
            else:
                report, secs, tok_s = run_evidence(sc, cp, a.ollama, a.model, think, a.rules)
            ok_s = bool(report) and report.get("service") == sc["service"]
            ok_c = bool(report) and report.get("cause") == sc["cause"]
            r = {"id": sc["id"], "expected": {"service": sc["service"], "cause": sc["cause"]}, "report": report,
                 "pass": ok_s and ok_c, "service_ok": ok_s, "cause_ok": ok_c, "inspected_faulty": True,
                 "steps": 0, "invalid": 0 if report else 1, "seconds": secs, "tok_s": tok_s, "trace": []}
            results.append(r)
            rep = report or {}
            print(f"{r['id']:>4}  {'PASS' if r['pass'] else 'fail'}  {str(rep.get('service', '-')):>8} {str(rep.get('cause', '-')):<15} {secs}s", flush=True)
            continue
        try:
            r = run_scenario(sc, cp, a.ollama, a.model, think, a.max_steps)
        except urllib.error.HTTPError as e:
            if think is False and e.code == 400:
                # The model has no thinking switch: retry without it.
                think = None
                r = run_scenario(sc, cp, a.ollama, a.model, think, a.max_steps)
            else:
                raise
        results.append(r)
        rep = r["report"] or {}
        print(f"{r['id']:>4}  {'PASS' if r['pass'] else 'fail'}  {rep.get('service', '-'):>8} {rep.get('cause', '-'):<15}"
              f" steps {r['steps']} invalid {r['invalid']} {r['seconds']}s {r['tok_s']} tok/s", flush=True)
    n = len(results)
    summary = {
        "model": a.model, "mode": a.mode, "rules": a.rules, "think": a.think, "scenarios": n,
        "passed": sum(r["pass"] for r in results),
        "service_ok": sum(r["service_ok"] for r in results),
        "cause_ok": sum(r["cause_ok"] for r in results),
        "inspected_faulty": sum(r["inspected_faulty"] for r in results),
        "invalid_calls": sum(r["invalid"] for r in results),
        "no_report": sum(r["report"] is None for r in results),
        "avg_seconds": round(sum(r["seconds"] for r in results) / n, 1),
        "avg_tok_s": round(sum(r["tok_s"] or 0 for r in results) / n, 1),
    }
    print(json.dumps(summary))
    os.makedirs(os.path.join(HERE, "results"), exist_ok=True)
    label = a.label or (a.model.replace(":", "-").replace("/", "-") + ("" if a.mode == "agent" else "-" + a.mode) + ("-rules" if a.rules else "") + ("-free" if a.no_format else ""))
    with open(os.path.join(HERE, "results", label + ".json"), "w") as f:
        json.dump({"summary": summary, "results": results}, f, indent=1)


if __name__ == "__main__":
    sys.exit(main())
