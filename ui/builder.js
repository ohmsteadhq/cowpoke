// Cowpoke service builder: a form for every protocol and mode, kept in sync with YAML.
// Uses helpers from app.js (esc, api, flash, can, KINDS, S).
"use strict";

// ---------------------------------------------------------------- settings schemas
// Field types: text, number, bool, choice, multi, password, rows (a list of sub-forms), kv (name → value), body (JSON or text).
// `adv` fields sit under "More settings". Blank fields are left out, so Cowpoke's default (the placeholder) applies.

const ORDER = [["big", "Big (ABCD)"], ["little", "Little (CDAB)"]];
const POLICIES = [["none", "None"], ["basic256sha256", "Basic256Sha256"], ["aes128_sha256_rsa_oaep", "Aes128Sha256RsaOaep"], ["aes256_sha256_rsa_pss", "Aes256Sha256RsaPss"]];
const METHODS = [["GET", "GET"], ["POST", "POST"], ["PUT", "PUT"], ["PATCH", "PATCH"], ["DELETE", "DELETE"]];
const BIND = { k: "bind", label: "Listen address", type: "text", ph: "all interfaces (IPv6 and IPv4)", adv: true, help: "Leave blank to listen everywhere." };

const SCHEMAS = {
  "modbus/server": [
    { k: "port", label: "Port", type: "number", ph: "502", help: "502 is standard. Below 1024 may need extra rights outside Docker." },
    { k: "word_order", label: "Word order", type: "choice", options: ORDER, dflt: "big", help: "How 32- and 64-bit values are split across registers." },
    { k: "units", label: "Unit ids", type: "list-number", ph: "every unit a tag uses", help: "Unit (slave) ids this server answers as, comma-separated." },
    { k: "strict", label: "Strict addressing", type: "bool", adv: true, help: "Answer Illegal Data Address for registers no tag covers, instead of 0." },
    BIND,
  ],
  "modbus/client": [
    { k: "host", label: "Device address", type: "text", ph: "192.168.1.50", required: true },
    { k: "port", label: "Port", type: "number", ph: "502" },
    { k: "poll_ms", label: "Poll every (ms)", type: "number", ph: "1000" },
    { k: "word_order", label: "Word order", type: "choice", options: ORDER, dflt: "big" },
    { k: "timeout_ms", label: "Timeout (ms)", type: "number", ph: "3000", adv: true },
  ],
  "opcua/server": [
    { k: "port", label: "Port", type: "number", ph: "4840" },
    { k: "hostname", label: "Host name clients use", type: "text", ph: "this computer's name", help: "Goes into the endpoint URL and certificate." },
    { k: "security_policies", label: "Security policies", type: "multi", options: POLICIES, dflt: POLICIES.map((p) => p[0]), help: "Each secured policy offers Sign and Sign & Encrypt." },
    { k: "allow_anonymous", label: "Allow anonymous clients", type: "bool", dflt: true },
    { k: "users", label: "User logins", type: "rows", add: "Add a user", fields: [
      { k: "username", label: "Username", type: "text", required: true },
      { k: "password", label: "Password", type: "password", required: true },
    ] },
    { k: "trust_client_certs", label: "Trust every client certificate", type: "bool", adv: true, help: "Otherwise new client certificates wait on the Certificates tab." },
    { k: "namespace_uri", label: "Namespace URI", type: "text", ph: "urn:cowpoke:<id>", adv: true },
    BIND,
  ],
  "opcua/client": [
    { k: "url", label: "Server URL", type: "text", ph: "opc.tcp://192.168.1.60:4840/", required: true },
    { k: "security_policy", label: "Security policy", type: "choice", options: POLICIES, dflt: "none" },
    { k: "security_mode", label: "Message security", type: "choice", options: [["none", "None"], ["sign", "Sign"], ["sign_and_encrypt", "Sign & encrypt"]], dflt: "none", help: "None goes with policy None, and only with it." },
    { k: "username", label: "Username", type: "text", ph: "anonymous" },
    { k: "password", label: "Password", type: "password" },
    { k: "poll_ms", label: "Read every (ms)", type: "number", ph: "1000" },
    { k: "trust_server_certs", label: "Trust the server's certificate automatically", type: "bool", adv: true },
    { k: "timeout_ms", label: "Timeout (ms)", type: "number", ph: "10000", adv: true },
  ],
  "api/server": [
    { k: "port", label: "Own port", type: "number", ph: "none: served at /x/<id>/ on Cowpoke's port", help: "Give it a port to serve the routes at / like a real API." },
    { k: "expose_tags", label: "Serve the tags as JSON", type: "bool", dflt: true, help: "GET /tags, GET /tags/NAME, PUT /tags/NAME" },
    { k: "routes", label: "Routes", type: "rows", add: "Add a route", fields: [
      { k: "method", label: "Method", type: "choice", options: METHODS, dflt: "GET" },
      { k: "path", label: "Path", type: "text", ph: "/v1/pumps/:id", required: true },
      { k: "status", label: "Status", type: "number", ph: "200" },
      { k: "delay_ms", label: "Delay (ms)", type: "number", ph: "0" },
      { k: "body", label: "Response body", type: "body", ph: '{ "level": "{{tags.level}}", "id": "{{params.id}}" }', wide: true },
      { k: "set", label: "Set tags from the request", type: "kv", kph: "tag", vph: "/json/pointer", wide: true },
      { k: "headers", label: "Response headers", type: "kv", kph: "Header", vph: "value", wide: true },
    ] },
    BIND,
  ],
  "api/client": [
    { k: "url", label: "URL", type: "text", ph: "https://api.example.com/v1/current", required: true },
    { k: "method", label: "Method", type: "choice", options: METHODS, dflt: "GET" },
    { k: "interval_ms", label: "Call every (ms)", type: "number", ph: "5000" },
    { k: "headers", label: "Headers", type: "kv", kph: "Authorization", vph: "Bearer …" },
    { k: "body", label: "Request body", type: "body", ph: "JSON or text; {{tags.NAME}} and {{now}} work", adv: true },
    { k: "timeout_ms", label: "Timeout (ms)", type: "number", ph: "10000", adv: true },
    { k: "write", label: "Write request", type: "object", adv: true, help: "Sent when a tag is written. Without it, tags are read-only. {{tag}} and {{value}} work.", fields: [
      { k: "url", label: "URL", type: "text", ph: "https://api.example.com/v1/setpoints/{{tag}}", required: true },
      { k: "method", label: "Method", type: "choice", options: METHODS, dflt: "GET" },
      { k: "body", label: "Body", type: "body", ph: '{ "value": "{{value}}" }' },
      { k: "headers", label: "Headers", type: "kv", kph: "Header", vph: "value" },
    ] },
  ],
  "iolink/server": [
    { k: "port", label: "Port", type: "number", ph: "8080", help: "Clients POST IoT Core JSON requests here." },
    { k: "ports", label: "IO-Link ports", type: "number", ph: "8" },
    { k: "product_name", label: "Master name", type: "text", ph: "Cowpoke IO-Link master" },
    { k: "devices", label: "Devices", type: "rows", add: "Add a device", help: "What each port reports. Ports with tags but no entry get a generic device.", fields: [
      { k: "port", label: "Port", type: "number", required: true },
      { k: "product_name", label: "Product", type: "text", ph: "TV7105" },
      { k: "vendor_id", label: "Vendor id", type: "number", ph: "310" },
      { k: "device_id", label: "Device id", type: "number" },
      { k: "pdin_bytes", label: "PD in bytes", type: "number", ph: "from tags" },
      { k: "pdout_bytes", label: "PD out bytes", type: "number", ph: "from tags" },
      { k: "serial_number", label: "Serial", type: "text" },
    ] },
    BIND,
  ],
  "iolink/client": [
    { k: "host", label: "Master address", type: "text", ph: "192.168.1.70", required: true },
    { k: "port", label: "Port", type: "number", ph: "80" },
    { k: "poll_ms", label: "Poll every (ms)", type: "number", ph: "1000" },
    { k: "timeout_ms", label: "Timeout (ms)", type: "number", ph: "3000", adv: true },
  ],
  "mqtt/server": [
    { k: "port", label: "Port", type: "number", ph: "1883", help: "Any MQTT 3.1.1 client can connect, publish and subscribe." },
    { k: "prefix", label: "Topic prefix", type: "text", ph: "the service id", help: "Tags publish to <prefix>/<tag> on change; publish to <topic>/set to write one." },
    { k: "retain", label: "Publish retained", type: "bool", dflt: true, help: "New subscribers get each tag's last value at once." },
    { k: "username", label: "Username", type: "text", ph: "none: anyone can connect", adv: true },
    { k: "password", label: "Password", type: "password", adv: true },
    { k: "sparkplug", label: "Sparkplug B edge node", type: "object", help: "Also publish the tags as a Sparkplug B edge node (or a device under it): births, data on change, writes and rebirths by NCMD/DCMD.", fields: [
      { k: "group_id", label: "Group id", type: "text", ph: "Plant", required: true },
      { k: "edge_node", label: "Edge node id", type: "text", ph: "Line1", required: true },
      { k: "device", label: "Device id", type: "text", ph: "none: metrics on the node" },
      { k: "broker", label: "Also publish to an outside broker", type: "object", help: "Connect out as this edge node, with an NDEATH will. Births go out on every connect.", fields: [
        { k: "host", label: "Broker address", type: "text", ph: "broker.local", required: true },
        { k: "port", label: "Port", type: "number", ph: "1883, or 8883 with TLS" },
        { k: "tls", label: "Use TLS", type: "bool" },
        { k: "username", label: "Username", type: "text", ph: "anonymous" },
        { k: "password", label: "Password", type: "password" },
        { k: "client_id", label: "Client id", type: "text", ph: "cowpoke-<group>-<node>" },
        { k: "ca", label: "CA certificate (PEM)", type: "body", ph: "-----BEGIN CERTIFICATE-----" },
      ] },
    ] },
    BIND,
  ],
  "mqtt/client": [
    { k: "host", label: "Broker address", type: "text", ph: "broker.local", required: true },
    { k: "port", label: "Port", type: "number", ph: "1883, or 8883 with TLS" },
    { k: "tls", label: "Use TLS", type: "bool" },
    { k: "username", label: "Username", type: "text", ph: "anonymous" },
    { k: "password", label: "Password", type: "password" },
    { k: "qos", label: "QoS", type: "number", ph: "0", help: "0: at most once. 1: at least once." },
    { k: "write_topic", label: "Write topic", type: "text", ph: "{{topic}}/set", help: "Where writes are published. {{topic}} is the tag's topic, {{tag}} its name." },
    { k: "retain", label: "Publish writes retained", type: "bool", adv: true },
    { k: "client_id", label: "Client id", type: "text", ph: "cowpoke-<id>", adv: true },
    { k: "keep_alive_s", label: "Keep-alive (s)", type: "number", ph: "30", adv: true },
    { k: "sparkplug", label: "Sparkplug B", type: "object", help: "Read one edge node (or a device under it) instead of plain topics. Tags name a metric; writes go out as NCMD/DCMD.", fields: [
      { k: "group_id", label: "Group id", type: "text", ph: "Plant", required: true },
      { k: "edge_node", label: "Edge node id", type: "text", ph: "Line1", required: true },
      { k: "device", label: "Device id", type: "text", ph: "none: the node's own metrics" },
      { k: "rebirth", label: "Ask for a rebirth on connect", type: "bool", dflt: true },
    ] },
    { k: "ca", label: "CA certificate (PEM)", type: "body", ph: "-----BEGIN CERTIFICATE-----", adv: true, help: "For a broker with its own CA. Blank uses the system's roots." },
  ],
  "s7/client": [
    { k: "host", label: "PLC address", type: "text", ph: "192.168.0.1", required: true },
    { k: "rack", label: "Rack", type: "number", ph: "0" },
    { k: "slot", label: "Slot", type: "number", ph: "1", help: "S7-300: 2. S7-1200/1500: 1 (or 0)." },
    { k: "poll_ms", label: "Poll every (ms)", type: "number", ph: "1000" },
    { k: "port", label: "Port", type: "number", ph: "102", adv: true },
    { k: "timeout_ms", label: "Timeout (ms)", type: "number", ph: "3000", adv: true },
    { k: "connection_type", label: "Connection type", type: "number", ph: "1", adv: true, help: "1 PG, 2 OP, 3 basic." },
  ],
  "s7/server": [
    { k: "port", label: "Port", type: "number", ph: "102", help: "Answers any rack and slot. Below 1024 may need extra rights outside Docker." },
    { k: "pdu_size", label: "PDU size", type: "number", ph: "480", help: "S7-300: 240. S7-1500: 960." },
    BIND,
  ],
  "monitor/client": [
    { k: "interface", label: "Capture interface", type: "text", ph: "eth1", help: "The mirror (SPAN) port. In Docker: --user 0 --cap-add NET_RAW --cap-add NET_ADMIN --network host." },
    { k: "file", label: "Or replay a capture file", type: "text", ph: "capture.pcapng", help: "pcap or pcapng, relative to the service's data folder." },
    { k: "learn_minutes", label: "Learning period (min)", type: "number", ph: "60", help: "Talkers first seen after this are flagged as new." },
    { k: "report_reads", label: "List reads as events", type: "bool", adv: true, help: "Noisy: polling clients read constantly." },
    { k: "filter", label: "Extra capture filter (BPF)", type: "text", ph: "net 192.168.10.0/24", adv: true },
  ],
  "eip/server": [
    { k: "port", label: "Port", type: "number", ph: "44818", help: "Answers Logix Read Tag / Write Tag requests, routed to any slot." },
    { k: "product_name", label: "Product name", type: "text", ph: "Cowpoke Logix simulator", help: "What browse tools see in ListIdentity." },
    BIND,
  ],
  "eip/client": [
    { k: "host", label: "Controller address", type: "text", ph: "192.168.1.10", required: true },
    { k: "slot", label: "CPU slot", type: "number", ph: "0", help: "The controller's backplane slot. CompactLogix: 0." },
    { k: "direct", label: "No backplane routing", type: "bool", help: "For Micro800 and other controllers without a backplane." },
    { k: "poll_ms", label: "Poll every (ms)", type: "number", ph: "1000" },
    { k: "port", label: "Port", type: "number", ph: "44818", adv: true },
    { k: "timeout_ms", label: "Timeout (ms)", type: "number", ph: "3000", adv: true },
  ],
};

const DATATYPES = ["bool", "int16", "uint16", "int32", "uint32", "int64", "uint64", "float32", "float64", "string"].map((d) => [d, d]);
const TABLES = [["holding_register", "Holding reg."], ["input_register", "Input reg."], ["coil", "Coil"], ["discrete_input", "Discrete in"]];
const REGS = { bool: 1, int16: 1, uint16: 1, int32: 2, uint32: 2, float32: 2, int64: 4, uint64: 4, float64: 4 };
const BITS = { bool: 1, int16: 16, uint16: 16, int32: 32, uint32: 32, float32: 32, int64: 64, uint64: 64, float64: 64, string: 128 };

// Address columns per kind (and mode). `w` is the column width.
function tagColumns(kind, mode, settings) {
  switch (kind) {
    case "modbus": return [
      { k: "table", label: "Table", type: "choice", options: TABLES, dflt: "holding_register", w: 140 },
      { k: "register", label: "Address", type: "number", w: 84, help: "Zero-based: 40001 is holding register 0" },
      { k: "unit", label: "Unit", type: "number", ph: "1", w: 60 },
    ];
    case "opcua": return [{ k: "node_id", label: mode === "client" ? "Node id" : "Node id (optional)", type: "text", ph: mode === "client" ? "ns=2;s=Line1.Temp" : "the tag name", w: 220 }];
    case "api": return mode === "client" ? [{ k: "pointer", label: "JSON pointer", type: "text", ph: "/<tag name>", w: 180 }] : [];
    case "mqtt":
      if (mode === "client" && settings?.sparkplug) return [{ k: "metric", label: "Metric", type: "text", ph: "the tag name", w: 220 }];
      if (mode === "server" && settings?.sparkplug) return [
        { k: "topic", label: "Topic (optional)", type: "text", ph: "<prefix>/<tag name>", w: 180 },
        { k: "metric", label: "Metric (optional)", type: "text", ph: "the tag name", w: 160 },
      ];
      return [
      { k: "topic", label: mode === "client" ? "Topic" : "Topic (optional)", type: "text", ph: mode === "client" ? "plant/tank1/state" : "<prefix>/<tag name>", w: 200 },
      ...(mode === "client" ? [{ k: "pointer", label: "JSON pointer", type: "text", ph: "whole payload", w: 140 }] : []),
    ];
    case "s7": return [
      { k: "s7_address", label: "Address", type: "text", ph: "DB1.DBD0", w: 150, help: "DB1.DBD0, DB1.DBX4.0, MW10, I0.1, QW4" },
      { k: "length", label: "Chars", type: "number", ph: "16", w: 64, help: "Strings only" },
    ];
    case "eip": return [{ k: "cip_tag", label: mode === "client" ? "Logix tag" : "Logix tag (optional)", type: "text", ph: mode === "client" ? "Program:Main.Level" : "the tag name", w: 200 }];
    case "iolink": return [
      { k: "port", label: "Port", type: "number", w: 56 },
      { k: "pd", label: "Data", type: "choice", options: [["in", "In"], ["out", "Out"]], dflt: "in", w: 76 },
      { k: "bit_offset", label: "Bit offset", type: "number", ph: "0", w: 76 },
      { k: "bit_length", label: "Bits", type: "number", ph: "type", w: 60 },
    ];
  }
  return [];
}

// Value generators for server-mode tags.
const GENS = [
  ["sine", "Sine", "num"], ["triangle", "Triangle", "num"], ["ramp", "Ramp", "num"], ["square", "Square", "numbool"],
  ["random", "Random", "numbool"], ["walk", "Walk", "num"], ["counter", "Counter", "num"], ["toggle", "Toggle", "numbool"],
  ["sequence", "Sequence", "all"],
];
const WAVES = new Set(["sine", "triangle", "ramp", "square"]);
function gensFor(dt) {
  return GENS.filter(([, , k]) => k === "all" || (dt === "bool" ? k === "numbool" : dt === "string" ? false : true));
}
function genLabel(g, dt) {
  if (!g) return "";
  const name = GENS.find((x) => x[0] === g.kind)?.[1] || g.kind;
  if (g.kind === "sequence") return `${name} (${(g.values || []).length})`;
  if (dt === "bool") return name;
  const lo = g.min ?? 0, hi = g.max ?? (g.kind === "counter" ? 65535 : 100);
  return `${name} ${lo}–${hi}`;
}

// ---------------------------------------------------------------- small helpers

function getPath(o, path) {
  return path.split(".").reduce((x, k) => (x == null ? undefined : x[k]), o);
}
function setPath(o, path, v) {
  const keys = path.split(".");
  let x = o;
  keys.slice(0, -1).forEach((k, i) => {
    if (x[k] == null || typeof x[k] !== "object") x[k] = /^\d+$/.test(keys[i + 1]) ? [] : {};
    x = x[k];
  });
  const last = keys[keys.length - 1];
  const empty = v === undefined || v === "" || (Array.isArray(v) && !v.length) || (v && typeof v === "object" && !Array.isArray(v) && !Object.keys(v).length);
  if (empty && !Array.isArray(x)) delete x[last];
  else x[last] = v;
}
const slug = (s) => s.toLowerCase().normalize("NFKD").replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 64);
const clone = (o) => JSON.parse(JSON.stringify(o));

// A dropdown drawn as our own popover (DESIGN.md: never the OS menu).
let openDD = null;
function closeDD() { if (openDD) { openDD.remove(); openDD = null; } }
document.addEventListener("click", (e) => { if (openDD && !openDD.contains(e.target) && !e.target.closest(".dd")) closeDD(); });
document.addEventListener("keydown", (e) => { if (e.key === "Escape") closeDD(); });

function ddButton(path, options, value, dflt) {
  const cur = options.find((o) => o[0] === (value ?? dflt));
  return `<button type="button" class="dd" data-dd="${esc(path)}" data-dflt="${esc(dflt ?? "")}" aria-haspopup="listbox">${esc(cur ? cur[1] : "—")}</button>`;
}

function openDropdown(btn, options, current, onPick) {
  closeDD();
  const r = btn.getBoundingClientRect();
  const pop = document.createElement("div");
  pop.className = "dd-pop";
  pop.setAttribute("role", "listbox");
  pop.style.left = `${r.left + window.scrollX}px`;
  pop.style.top = `${r.bottom + window.scrollY + 2}px`;
  pop.style.minWidth = `${r.width}px`;
  pop.innerHTML = options.map(([v, l]) => `<button type="button" role="option" data-v="${esc(v)}" class="${v === current ? "on" : ""}">${esc(l)}</button>`).join("");
  document.body.appendChild(pop);
  openDD = pop;
  const items = [...pop.querySelectorAll("button")];
  (items.find((b) => b.classList.contains("on")) || items[0]).focus();
  pop.addEventListener("keydown", (e) => {
    const i = items.indexOf(document.activeElement);
    if (e.key === "ArrowDown") { e.preventDefault(); items[Math.min(i + 1, items.length - 1)].focus(); }
    if (e.key === "ArrowUp") { e.preventDefault(); items[Math.max(i - 1, 0)].focus(); }
    if (e.key === "Escape" || e.key === "Tab") { closeDD(); btn.focus(); }
  });
  items.forEach((b) => (b.onclick = () => { closeDD(); onPick(b.dataset.v); btn.focus(); }));
}

// ---------------------------------------------------------------- the builder

// opts: { def, editing (id or null), agents: [...], onSaved(svc) }
function serviceBuilder(root, opts) {
  const st = {
    def: clone(opts.def),
    editing: opts.editing || null,
    idTouched: !!opts.editing,
    view: "form",
    adv: false,
    open: new Set(),
    check: null,
    test: null,
    yaml: "",
    busy: false,
  };
  const key = () => `${st.def.kind}/${st.def.mode}`;
  let checkTimer = null;

  const field = (f, base, value) => {
    const path = base ? `${base}.${f.k}` : f.k;
    const id = `f-${path.replace(/\./g, "-")}`;
    const help = f.help ? `<span class="hint">${esc(f.help)}</span>` : "";
    const label = `${esc(f.label)}${f.required ? ' <span class="req" aria-hidden="true">*</span>' : ""}`;
    switch (f.type) {
      case "text": case "password": case "number": case "list-number":
        return `<label class="fld ${f.wide ? "wide" : ""}" for="${id}"><span class="lbl">${label}</span>
          <input id="${id}" data-path="${esc(path)}" data-type="${f.type}" type="${f.type === "password" ? "password" : "text"}"
            ${f.type === "number" ? 'inputmode="numeric"' : ""} class="${f.type === "text" || f.type === "password" ? "" : "mono"}"
            placeholder="${esc(f.ph || "")}" value="${esc(Array.isArray(value) ? value.join(", ") : value ?? "")}" ${f.required ? "required" : ""} autocomplete="off">${help}</label>`;
      case "bool": {
        const on = value ?? f.dflt ?? false;
        return `<div class="fld ${f.wide ? "wide" : ""}"><span class="lbl">${label}</span>
          <span class="seg small-seg" role="group"><button type="button" data-bool="${esc(path)}" data-v="true" class="${on ? "on" : ""}">Yes</button><button type="button" data-bool="${esc(path)}" data-v="false" class="${on ? "" : "on"}">No</button></span>${help}</div>`;
      }
      case "choice":
        if (f.options.length <= 3) {
          const cur = value ?? f.dflt;
          return `<div class="fld"><span class="lbl">${label}</span><span class="seg small-seg" role="group">${f.options.map(([v, l]) =>
            `<button type="button" data-choice="${esc(path)}" data-v="${esc(v)}" class="${v === cur ? "on" : ""}">${esc(l)}</button>`).join("")}</span>${help}</div>`;
        }
        return `<div class="fld"><span class="lbl">${label}</span>${ddButton(path, f.options, value, f.dflt)}${help}</div>`;
      case "multi": {
        const cur = value ?? f.dflt ?? [];
        return `<div class="fld wide"><span class="lbl">${label}</span><span class="checks">${f.options.map(([v, l]) =>
          `<button type="button" class="chk ${cur.includes(v) ? "on" : ""}" data-multi="${esc(path)}" data-v="${esc(v)}" aria-pressed="${cur.includes(v)}">${esc(l)}</button>`).join("")}</span>${help}</div>`;
      }
      case "body": {
        const text = value === undefined ? "" : typeof value === "string" ? value : JSON.stringify(value, null, 2);
        return `<label class="fld wide" for="${id}"><span class="lbl">${label}</span><textarea id="${id}" class="body" data-path="${esc(path)}" data-type="body" rows="${text.split("\n").length > 3 ? 6 : 3}" placeholder="${esc(f.ph || "")}" spellcheck="false">${esc(text)}</textarea>${help}</label>`;
      }
      case "kv": {
        const entries = Object.entries(value || {});
        return `<div class="fld wide"><span class="lbl">${label}</span>
          <div class="kv">${entries.map(([k, v], i) => `<div class="kv-row">
            <input class="mono" data-kv="${esc(path)}" data-i="${i}" data-part="k" value="${esc(k)}" placeholder="${esc(f.kph || "name")}" aria-label="${esc(f.label)} name">
            <input class="mono" data-kv="${esc(path)}" data-i="${i}" data-part="v" value="${esc(v)}" placeholder="${esc(f.vph || "value")}" aria-label="${esc(f.label)} value">
            <button type="button" class="small icon" data-kv-del="${esc(path)}" data-i="${i}" title="Remove">×</button></div>`).join("")}
          <button type="button" class="small" data-kv-add="${esc(path)}">+ Add</button></div>${help}</div>`;
      }
      case "rows": {
        const rows = value || [];
        return `<div class="fld wide"><span class="lbl">${label}</span>${help}
          <div class="subrows">${rows.map((row, i) => `<div class="subrow">
            <div class="subrow-h"><b>${esc(f.label.replace(/s$/, ""))} ${i + 1}</b><span class="spacer"></span><button type="button" class="small danger" data-row-del="${esc(path)}" data-i="${i}">Remove</button></div>
            <div class="fgrid">${f.fields.map((sf) => field(sf, `${path}.${i}`, row[sf.k])).join("")}</div></div>`).join("")}
          <button type="button" class="small" data-row-add="${esc(path)}">+ ${esc(f.add || "Add")}</button></div></div>`;
      }
      case "object": {
        const on = value !== undefined;
        return `<div class="fld wide"><span class="lbl">${label}</span>${help}
          <span class="seg small-seg"><button type="button" data-obj="${esc(path)}" data-v="true" class="${on ? "on" : ""}">On</button><button type="button" data-obj="${esc(path)}" data-v="false" class="${on ? "" : "on"}">Off</button></span>
          ${on ? `<div class="subrow"><div class="fgrid">${f.fields.map((sf) => field(sf, path, value[sf.k])).join("")}</div></div>` : ""}</div>`;
      }
    }
    return "";
  };

  const schema = () => SCHEMAS[key()] || [];
  const fieldByPath = (path) => {
    // Find the schema entry for a settings path like settings.routes.0.body
    const parts = path.split(".").filter((p) => !/^\d+$/.test(p));
    if (parts[0] === "settings") {
      let list = schema(), f;
      for (const p of parts.slice(1)) { f = list.find((x) => x.k === p); list = f?.fields || []; }
      return f;
    }
    if (parts[0] === "tags") {
      const all = [...tagColumns(st.def.kind, st.def.mode, st.def.settings), ...TAG_DETAIL];
      return all.find((x) => x.k === parts[parts.length - 1]) || (parts[1] === "history" ? HIST.find((x) => x.k === parts[2]) : null);
    }
    return null;
  };

  // ---- import from Ohmstead (POST /api/import/ohmstead previews; "Add" merges into the form, Save saves)

  const OHM_OPTS = {
    modbus: [{ k: "start", label: "First address", ph: "0", num: true, help: "Per table. Tags already here keep their address." }, { k: "unit", label: "Unit id", ph: "1", num: true }],
    opcua: [{ k: "ns", label: "Namespace index", ph: "2", num: true, client: true }, { k: "prefix", label: "Node id prefix", ph: "none, e.g. PLC1.", client: true }],
    iolink: [{ k: "port_start", label: "First port", ph: "1", num: true, help: "One port per device." }],
  };
  const ohmOpts = () => (OHM_OPTS[st.def.kind] || []).filter((o) => !o.client || st.def.mode === "client");
  const ohmPanel = () => {
    const o = st.ohm, r = o.result;
    const fld = (id, label, val, ph, extra = "") => `<label class="fld" for="${id}"><span class="lbl">${label}</span><input id="${id}" value="${esc(val || "")}" placeholder="${esc(ph)}" autocomplete="off" ${extra}></label>`;
    return `<div class="pad b-ohm">
      <p class="muted">Turn the PLC I/O drawn in an Ohmstead project into tags: one per wired channel, named by its device. Tags with the same name are updated, not duplicated.</p>
      <span class="seg small-seg"><button type="button" data-ohm-src="paste" class="${o.src === "paste" ? "on" : ""}">Paste the export</button><button type="button" data-ohm-src="fetch" class="${o.src === "fetch" ? "on" : ""}">Fetch from Ohmstead</button></span>
      ${o.src === "paste"
        ? `<label class="fld wide" for="ohm-text"><span class="lbl">Ohmstead I/O export</span><textarea id="ohm-text" class="mono" rows="6" spellcheck="false" placeholder='{ "format": "ohmstead-io", "points": [ … ] }'>${esc(o.text)}</textarea>
            <span class="hint">Ask your assistant to run <code>export_io_points</code> on the project (Ohmstead MCP), and paste the JSON it returns.</span></label>`
        : `<div class="fgrid">${fld("ohm-project", "Project id", o.project, "from the project's URL", 'class="mono"')}
            ${fld("ohm-token", "Access token", o.token, "ohm_…", 'type="password" class="mono"')}
            ${fld("ohm-url", "MCP address", o.url, "https://mcp.ohmstead.io/mcp", 'class="mono"')}</div>
            <span class="hint">Create a token in Ohmstead → Settings → MCP access tokens. Cowpoke uses it once and doesn't keep it.</span>`}
      ${ohmOpts().length ? `<div class="fgrid">${ohmOpts().map((x) => `<label class="fld" for="ohm-o-${x.k}"><span class="lbl">${esc(x.label)}</span><input id="ohm-o-${x.k}" data-ohm-opt="${x.k}" class="mono" value="${esc(o.opts[x.k] ?? "")}" placeholder="${esc(x.ph)}">${x.help ? `<span class="hint">${esc(x.help)}</span>` : ""}</label>`).join("")}</div>` : ""}
      <div class="row"><button type="button" class="small" id="ohm-preview">Preview</button>
        ${r && r.preview.rows.length ? `<button type="button" class="small primary" id="ohm-add">Add ${r.preview.added.length} and update ${r.preview.updated.length} tag${r.preview.added.length + r.preview.updated.length === 1 ? "" : "s"}</button>` : ""}
        <span class="spacer"></span><button type="button" class="small" id="ohm-close">Close</button></div>
      <div id="ohm-msg">${o.error ? `<div class="msg err">${esc(o.error)}</div>` : ""}</div>
      ${r ? `<p class="muted">${r.points} point${r.points === 1 ? "" : "s"}: ${r.preview.added.length} new, ${r.preview.updated.length} updated, ${r.preview.unchanged.length} unchanged${r.preview.not_in_import.length ? `. Not in the project, kept: ${esc(r.preview.not_in_import.join(", "))}` : ""}.</p>
        ${r.preview.skipped.length ? `<div class="msg err">Skipped: ${r.preview.skipped.map((s) => `${esc(s.name)} (${esc(s.reason)})`).join("; ")}</div>` : ""}
        <table class="mini"><thead><tr><th>Tag</th><th>Device</th><th>Channel</th><th>I/O</th><th>Type</th><th>Address</th><th></th></tr></thead>
        <tbody>${r.preview.rows.map((x) => `<tr><td class="mono">${esc(x.name)}</td><td class="mono">${esc(x.device)}</td><td class="mono">${esc(x.channel)}</td><td>${esc(x.kind)} ${esc(x.direction)}</td><td class="mono">${esc(x.datatype)}</td><td class="mono">${esc(x.address || "—")}</td><td class="muted">${esc(x.status)}</td></tr>`).join("")}</tbody></table>` : ""}
    </div>`;
  };
  const ohmRequest = () => {
    const o = st.ohm;
    const options = {};
    for (const x of ohmOpts()) {
      const v = String(o.opts[x.k] ?? "").trim();
      if (v !== "") options[x.k] = x.num ? Number(v) : v;
    }
    const body = { definition: cleanDef(), options };
    if (o.src === "paste") body.io = o.text;
    else body.source = { url: o.url, token: o.token, project_id: o.project.trim() };
    return body;
  };
  const wireOhm = () => {
    root.querySelector("#b-ohm")?.addEventListener("click", () => {
      st.ohm = st.ohm ? null : { src: "paste", text: "", url: "", token: "", project: "", opts: {}, result: null, error: null };
      render();
    });
    if (!st.ohm) return;
    const o = st.ohm;
    root.querySelectorAll("[data-ohm-src]").forEach((b) => (b.onclick = () => { o.src = b.dataset.ohmSrc; o.result = null; o.error = null; render(); }));
    const bind = (id, k) => root.querySelector(id)?.addEventListener("input", (e) => { o[k] = e.target.value; });
    bind("#ohm-text", "text"); bind("#ohm-project", "project"); bind("#ohm-token", "token"); bind("#ohm-url", "url");
    root.querySelectorAll("[data-ohm-opt]").forEach((el) => el.addEventListener("input", () => { o.opts[el.dataset.ohmOpt] = el.value; }));
    root.querySelector("#ohm-close").onclick = () => { st.ohm = null; render(); };
    root.querySelector("#ohm-preview").onclick = async (e) => {
      e.target.disabled = true;
      try { o.result = await api("POST", "/api/import/ohmstead", ohmRequest()); o.error = null; }
      catch (err) { o.result = null; o.error = err.message; }
      render();
    };
    root.querySelector("#ohm-add")?.addEventListener("click", () => {
      st.def.tags = o.result.definition.tags;
      st.ohm = null; st.open.clear();
      render(); scheduleCheck();
    });
  };

  // ---- render

  const render = () => {
    const d = st.def;
    const adv = schema().filter((f) => f.adv);
    root.innerHTML = `
      <div class="builder">
        <div class="b-main">
          <div class="tabs b-tabs">
            <button type="button" data-view="form" class="${st.view === "form" ? "on" : ""}">Form</button>
            <button type="button" data-view="yaml" class="${st.view === "yaml" ? "on" : ""}">YAML</button>
          </div>
          ${st.view === "yaml" ? `
          <div class="panel pad tabbody">
            <p class="muted">The same service as YAML. Changes here carry back to the form.</p>
            <textarea id="b-yaml" spellcheck="false">${esc(st.yaml)}</textarea>
          </div>` : `
          <section class="panel pad b-sec">
            <h3>Basics</h3>
            <div class="fgrid">
              <label class="fld" for="b-name"><span class="lbl">Name</span><input id="b-name" data-path="name" data-type="text" value="${esc(d.name || "")}" placeholder="Line 1 PLC" autocomplete="off"></label>
              <label class="fld" for="b-id"><span class="lbl">Id <span class="req" aria-hidden="true">*</span></span><input id="b-id" class="mono" data-path="id" data-type="text" value="${esc(d.id || "")}" placeholder="line1-plc" required autocomplete="off">
                <span class="hint">Used in URLs and the API: a-z, 0-9, - and _.</span></label>
              <div class="fld"><span class="lbl">Runs on</span><span class="seg small-seg">
                <button type="button" data-agent="" class="${d.agent ? "" : "on"}">Hub</button>
                ${(opts.agents || []).map((a) => `<button type="button" data-agent="${esc(a.id)}" class="${d.agent === a.id ? "on" : ""}" title="${a.online ? "online" : "offline"}">${esc(a.name)}${a.online ? "" : " ·off"}</button>`).join("")}
              </span><span class="hint">${opts.agents?.length ? "An agent runs it on the plant network." : "Add edge agents on the Agents page to run services on a plant network."}</span></div>
              <div class="fld"><span class="lbl">Start it</span><span class="seg small-seg">
                <button type="button" data-enabled="true" class="${d.enabled !== false ? "on" : ""}">Yes</button><button type="button" data-enabled="false" class="${d.enabled === false ? "on" : ""}">Not yet</button></span></div>
            </div>
          </section>
          <section class="panel pad b-sec">
            <h3>${d.mode === "server" ? "Server" : "Connection"} settings</h3>
            <div class="fgrid">${schema().filter((f) => !f.adv).map((f) => field(f, "settings", d.settings?.[f.k])).join("")}</div>
            ${adv.length ? `<button type="button" class="linkish" id="b-adv">${st.adv ? "Fewer settings" : `More settings (${adv.length})`}</button>
              ${st.adv ? `<div class="fgrid">${adv.map((f) => field(f, "settings", d.settings?.[f.k])).join("")}</div>` : ""}` : ""}
          </section>
          <section class="panel b-sec b-tags">
            <div class="row pad-x"><h3>Tags <span class="muted count">${(d.tags || []).length}</span></h3><span class="spacer"></span>
              ${d.mode === "server" && (d.tags || []).length ? `<button type="button" class="small" id="b-simall" title="Give every tag without one a generator">∿ Simulate all</button>
                ${(d.tags || []).some((t) => t.simulate) ? `<button type="button" class="small" id="b-simnone">Stop all</button>` : ""}` : ""}
              <button type="button" class="small" id="b-example">Load example tags</button>
              <button type="button" class="small${st.ohm ? " on" : ""}" id="b-ohm" aria-expanded="${!!st.ohm}">Import from Ohmstead</button>
              <button type="button" class="small primary" id="b-addtag">+ Add tag</button></div>
            ${st.ohm ? ohmPanel() : ""}
            ${tagsTable()}
          </section>`}
        </div>
        <aside class="b-side">
          <div class="panel pad b-check">${checkHtml()}</div>
          <div class="panel pad b-test">${testHtml()}</div>
          <div class="b-actions">
            <button type="button" class="primary" id="b-save" ${st.busy ? "disabled" : ""}>${st.editing ? "Save changes" : d.enabled === false ? "Create" : "Create and start"}</button>
            ${opts.cancel ? `<a class="btn" href="${opts.cancel}">Cancel</a>` : ""}
          </div>
          <div id="b-msg"></div>
        </aside>
      </div>`;
    wire();
  };

  const HIST = [
    { k: "mode", label: "Record", type: "choice", options: [["off", "Off"], ["deadband", "On change"], ["interval", "Every interval"]] },
    { k: "deadband", label: "Deadband", type: "number", ph: "0" },
    { k: "interval_ms", label: "Interval (ms)", type: "number", ph: "1000" },
    { k: "retention_days", label: "Keep (days)", type: "number", ph: "30" },
  ];
  const TAG_DETAIL = [
    { k: "units", label: "Units", type: "text", ph: "°C" },
    { k: "description", label: "Description", type: "text", wide: true },
    { k: "scale", label: "Scale", type: "number", ph: "1", help: "value = raw × scale + offset" },
    { k: "offset", label: "Offset", type: "number", ph: "0" },
    { k: "initial", label: "Initial value", type: "text", ph: "0", serverOnly: true },
    { k: "length", label: "String length (registers)", type: "number", ph: "10", kind: "modbus", stringOnly: true },
  ];

  const simulateHtml = (t, i) => {
    const g = t.simulate;
    const opts = gensFor(t.datatype);
    const base = `tags.${i}.simulate`;
    const num = (k, label, ph, help) => field({ k, label, type: "number", ph, help }, base, g?.[k]);
    let fields = "";
    if (g) {
      const isBool = t.datatype === "bool";
      if (g.kind === "sequence") {
        fields = `<label class="fld wide" for="seq-${i}"><span class="lbl">Values, in order</span>
          <input id="seq-${i}" class="mono" data-seq="${i}" value="${esc((g.values || []).map((v) => (typeof v === "string" ? v : JSON.stringify(v))).join(", "))}" placeholder="idle, running, fault">
          <span class="hint">Comma-separated. One per interval, then it starts over.</span></label>`;
      } else if (!isBool && g.kind !== "toggle") {
        fields += num("min", "Min", "0") + num("max", "Max", g.kind === "counter" ? "65535" : "100");
      } else if (!isBool) {
        fields += num("min", "Low", "0") + num("max", "High", "100");
      }
      if (WAVES.has(g.kind)) fields += num("period_ms", "Period (ms)", "60000", "One full cycle.");
      if (g.kind === "counter" || g.kind === "walk") fields += num("step", g.kind === "counter" ? "Step" : "Largest step", g.kind === "counter" ? "1" : "span ÷ 50");
      if (!isBool && ["sine", "triangle", "ramp", "square", "random", "toggle"].includes(g.kind)) fields += num("noise", "Noise (±)", "0");
      fields += num("interval_ms", "Update every (ms)", "1000");
    }
    return `<div class="sim">
      <div class="sim-h"><span class="lbl">Simulate</span>
        ${opts.length ? `<span class="gen-btns">
          <button type="button" class="${g ? "" : "on"}" data-gen="${i}" data-v="">None</button>
          ${opts.map(([k, l]) => `<button type="button" class="${g?.kind === k ? "on" : ""}" data-gen="${i}" data-v="${k}">${esc(l)}</button>`).join("")}
        </span>` : `<span class="muted small">Not for this data type.</span>`}</div>
      ${g ? `<div class="fgrid">${fields}</div>` : `<p class="hint">Without a generator the value only changes when something writes it.</p>`}
    </div>`;
  };

  // A sensible generator for a tag, staggered so tags don't move in step.
  const defaultGen = (t, i, kind) => {
    const dt = t.datatype || "uint16";
    kind = kind || (dt === "bool" ? "toggle" : dt === "string" ? "sequence" : /int/.test(dt) ? "walk" : "sine");
    const g = { kind };
    if (kind === "sequence") g.values = dt === "string" ? ["idle", "running", "stopped"] : dt === "bool" ? [true, false] : [0, 1, 2];
    if (dt === "bool") { if (kind === "toggle") g.interval_ms = 5000; return g; }
    if (dt === "string") return g;
    const init = typeof t.initial === "number" ? t.initial : null;
    if (init !== null && init !== 0 && kind !== "counter") { g.min = +(init * 0.8).toFixed(2); g.max = +(init * 1.2).toFixed(2); }
    else { g.min = 0; g.max = kind === "counter" ? 1000 : 100; }
    if (/int/.test(dt) && dt !== "int16" && g.min < 0 && dt.startsWith("u")) g.min = 0;
    if (WAVES.has(kind)) g.period_ms = [60000, 45000, 90000, 30000, 120000][i % 5];
    if (kind === "sine" && !/int/.test(dt)) g.noise = +((g.max - g.min) / 100).toFixed(3);
    return g;
  };

  const tagsTable = () => {
    const d = st.def, cols = tagColumns(d.kind, d.mode, d.settings);
    const tags = d.tags || [];
    if (!tags.length) return `<div class="pad muted b-empty">No tags yet. Add one, or load the example tags for ${esc(KINDS[d.kind].label)} ${esc(d.mode)}.</div>`;
    return `<div class="scroll"><table class="b-table">
      <thead><tr><th style="width:28px"></th><th>Name</th><th style="width:120px">Type</th>${cols.map((c) => `<th style="width:${c.w}px" title="${esc(c.help || "")}">${esc(c.label)}</th>`).join("")}
        <th style="width:84px">Writable</th>${d.mode === "server" ? '<th style="width:130px">Simulate</th>' : ""}<th style="width:90px">History</th><th style="width:72px"></th></tr></thead>
      <tbody>${tags.map((t, i) => {
        const open = st.open.has(i);
        const hist = t.history ? (t.history.mode || "deadband") : "off";
        return `<tr class="${open ? "open" : ""}">
          <td><button type="button" class="small icon" data-tag-open="${i}" aria-expanded="${open}" title="More for this tag">${open ? "▾" : "▸"}</button></td>
          <td><input class="mono" data-path="tags.${i}.name" data-type="text" value="${esc(t.name || "")}" placeholder="tag_name" aria-label="Tag name"></td>
          <td>${ddButton(`tags.${i}.datatype`, DATATYPES, t.datatype, "uint16")}</td>
          ${cols.map((c) => `<td>${c.type === "choice" ? ddButton(`tags.${i}.${c.k}`, c.options, t[c.k], c.dflt)
            : `<input class="mono" data-path="tags.${i}.${c.k}" data-type="${c.type}" value="${esc(t[c.k] ?? "")}" placeholder="${esc(c.ph || "")}" aria-label="${esc(c.label)}">`}</td>`).join("")}
          <td><button type="button" class="chk ${t.writable === false ? "" : "on"}" data-writable="${i}" aria-pressed="${t.writable !== false}">${t.writable === false ? "No" : "Yes"}</button></td>
          ${d.mode === "server" ? `<td>${t.simulate ? `<button type="button" class="gen-chip" data-tag-open="${i}" title="Change the generator">∿ ${esc(genLabel(t.simulate, t.datatype))}</button>` : `<button type="button" class="linkish small-link" data-tag-open="${i}">add</button>`}</td>` : ""}
          <td><span class="hist-tag ${hist === "off" ? "muted" : ""}">${hist === "off" ? "off" : hist === "interval" ? "interval" : "on change"}</span></td>
          <td class="row-acts"><button type="button" class="small icon" data-tag-dup="${i}" title="Duplicate">⧉</button><button type="button" class="small icon danger" data-tag-del="${i}" title="Delete">×</button></td>
        </tr>
        ${open ? `<tr class="detail-row"><td></td><td colspan="${cols.length + (d.mode === "server" ? 6 : 5)}">
          <div class="fgrid">${TAG_DETAIL.filter((f) => (!f.serverOnly || d.mode === "server") && (!f.kind || f.kind === d.kind) && (!f.stringOnly || t.datatype === "string"))
            .map((f) => field(f, `tags.${i}`, t[f.k])).join("")}</div>
          ${d.mode === "server" ? simulateHtml(t, i) : ""}
          <div class="fgrid hist">${field(HIST[0], `tags.${i}.history`, hist)}
            ${hist === "deadband" ? field(HIST[1], `tags.${i}.history`, t.history?.deadband) : ""}
            ${hist === "interval" ? field(HIST[2], `tags.${i}.history`, t.history?.interval_ms) : ""}
            ${hist !== "off" ? field(HIST[3], `tags.${i}.history`, t.history?.retention_days) : ""}</div>
        </td></tr>` : ""}`;
      }).join("")}</tbody></table></div>`;
  };

  const checkHtml = () => {
    const c = st.check;
    if (!c) return `<b>Checks</b><p class="muted small">Checking…</p>`;
    const items = [
      ...c.errors.map((e) => `<li class="err">${esc(e)}</li>`),
      ...c.warnings.map((w) => `<li class="warn">${esc(w)}</li>`),
    ];
    return `<b>Checks</b>${c.errors.length ? "" : `<p class="ok-line"><span class="dot"></span>Ready to save</p>`}
      ${items.length ? `<ul class="checks-list">${items.join("")}</ul>` : ""}`;
  };

  const testHtml = () => {
    const t = st.test;
    const where = st.def.agent ? `<p class="muted small">Tests run on the hub; this service runs on an agent.</p>` : "";
    let out = "";
    if (t === "running") out = `<p class="muted small">Running it for up to 8 seconds…</p>`;
    else if (t && t.error) out = `<p class="err-text small">${esc(t.error)}</p>`;
    else if (t) {
      const ok = t.state === "running" || t.state === "degraded";
      out = `<p class="small st-${esc(t.state)}"><span class="dot"></span><b>${ok ? "It works" : t.state === "skipped" ? "Skipped" : "It failed"}</b></p>
        <p class="detail">${esc(t.detail)}</p>
        ${t.tags?.length ? `<table class="mini"><tbody>${t.tags.map((x) => `<tr><td class="mono">${esc(x.name)}</td><td class="num mono q-${esc(x.quality || "")}">${esc(fmtValue(x.value))}</td></tr>`).join("")}</tbody></table>` : ""}`;
    }
    return `<div class="row"><b>Test</b><span class="spacer"></span><button type="button" class="small" id="b-test" ${t === "running" || st.def.agent ? "disabled" : ""}>Run test</button></div>
      <p class="muted small">${st.def.mode === "client" ? "Connects and reads every tag once, without saving." : "Starts it briefly to check the port and settings, without saving."}</p>${where}${out}`;
  };

  const refreshSide = () => {
    const c = root.querySelector(".b-check"); if (c) c.innerHTML = checkHtml();
    const t = root.querySelector(".b-test"); if (t) { t.innerHTML = testHtml(); wireTest(); }
  };

  // ---- data flow

  const cleanDef = () => {
    const d = clone(st.def);
    if (!d.agent) delete d.agent;
    (d.tags || []).forEach((t) => { if (t.history && t.history.mode === "off") delete t.history; });
    return d;
  };

  const runCheck = async () => {
    try {
      const q = st.editing ? `?editing=${encodeURIComponent(st.editing)}` : "";
      const r = await api("POST", `/api/services/check${q}`, cleanDef());
      st.check = r;
      if (r.yaml) st.yaml = r.yaml;
    } catch (e) { st.check = { errors: [e.message], warnings: [] }; }
    refreshSide();
  };
  const scheduleCheck = () => { clearTimeout(checkTimer); checkTimer = setTimeout(runCheck, 350); st.test = null; };

  const coerce = (type, raw) => {
    if (type === "number") { const s = raw.trim(); if (s === "") return undefined; const n = Number(s); return Number.isFinite(n) ? n : s; }
    if (type === "list-number") return raw.split(/[\s,]+/).filter(Boolean).map(Number).filter(Number.isFinite);
    if (type === "body") {
      const s = raw.trim(); if (!s) return undefined;
      if (/^[{[]/.test(s)) { try { return JSON.parse(s); } catch (e) { return raw; } }
      return raw;
    }
    return raw === "" ? undefined : raw;
  };

  const wire = () => {
    // Text-like inputs: update in place, keep focus.
    root.querySelectorAll("[data-path]").forEach((el) => {
      el.addEventListener("input", () => {
        const path = el.dataset.path;
        let v = coerce(el.dataset.type, el.value);
        if (/^tags\.\d+\.initial$/.test(path) && typeof v === "string") {
          // Initial values: numbers and true/false as such, the rest as text.
          if (/^-?\d+(\.\d+)?$/.test(v.trim())) v = Number(v); else if (/^(true|false)$/i.test(v.trim())) v = /^true$/i.test(v.trim());
        }
        if (path === "id") st.idTouched = true;
        setPath(st.def, path, v);
        if (path === "name" && !st.idTouched) {
          st.def.id = slug(el.value);
          const idEl = root.querySelector("#b-id"); if (idEl) idEl.value = st.def.id;
        }
        scheduleCheck();
      });
    });
    root.querySelectorAll("[data-dd]").forEach((b) => (b.onclick = () => {
      const path = b.dataset.dd;
      const f = path.startsWith("tags.") && path.endsWith(".datatype") ? { options: DATATYPES } : fieldByPath(path);
      openDropdown(b, f.options, getPath(st.def, path) ?? b.dataset.dflt, (v) => {
        setPath(st.def, path, v);
        // Coils and discrete inputs only hold bools.
        const dm = path.match(/^tags\.(\d+)\.datatype$/);
        if (dm) {
          const t = st.def.tags[+dm[1]];
          if (t.simulate && !gensFor(v).some(([k]) => k === t.simulate.kind)) delete t.simulate;
        }
        const m = path.match(/^tags\.(\d+)\.table$/);
        if (m && (v === "coil" || v === "discrete_input")) st.def.tags[+m[1]].datatype = "bool";
        render(); scheduleCheck();
      });
    }));
    root.querySelectorAll("[data-bool]").forEach((b) => (b.onclick = () => { setPath(st.def, b.dataset.bool, b.dataset.v === "true"); render(); scheduleCheck(); }));
    root.querySelectorAll("[data-choice]").forEach((b) => (b.onclick = () => {
      const path = b.dataset.choice;
      if (/\.history\.mode$/.test(path)) {
        const i = +path.split(".")[1];
        const t = st.def.tags[i];
        if (b.dataset.v === "off") delete t.history; else t.history = { ...(t.history || {}), mode: b.dataset.v };
      } else setPath(st.def, path, b.dataset.v);
      render(); scheduleCheck();
    }));
    root.querySelectorAll("[data-multi]").forEach((b) => (b.onclick = () => {
      const path = b.dataset.multi;
      const f = fieldByPath(path);
      const cur = new Set(getPath(st.def, path) ?? f.dflt ?? []);
      cur.has(b.dataset.v) ? cur.delete(b.dataset.v) : cur.add(b.dataset.v);
      setPath(st.def, path, f.options.map((o) => o[0]).filter((v) => cur.has(v)));
      render(); scheduleCheck();
    }));
    root.querySelectorAll("[data-obj]").forEach((b) => (b.onclick = () => {
      setPath(st.def, b.dataset.obj, b.dataset.v === "true" ? { url: "" } : undefined);
      render(); scheduleCheck();
    }));
    root.querySelectorAll("[data-row-add]").forEach((b) => (b.onclick = () => {
      const path = b.dataset.rowAdd;
      const rows = getPath(st.def, path) || [];
      const f = fieldByPath(path);
      const blank = {};
      if (path === "settings.devices") blank.port = rows.length + 1;
      if (path === "settings.routes") blank.method = "GET";
      if (f) rows.push(blank);
      setPath(st.def, path, rows);
      render(); scheduleCheck();
    }));
    root.querySelectorAll("[data-row-del]").forEach((b) => (b.onclick = () => {
      const rows = getPath(st.def, b.dataset.rowDel) || [];
      rows.splice(+b.dataset.i, 1);
      setPath(st.def, b.dataset.rowDel, rows);
      render(); scheduleCheck();
    }));
    root.querySelectorAll("[data-kv-add]").forEach((b) => (b.onclick = () => {
      const o = getPath(st.def, b.dataset.kvAdd) || {};
      let k = "", n = 1; while (k === "" || k in o) k = `name${n++}`;
      o[k] = ""; setPath(st.def, b.dataset.kvAdd, o); render(); scheduleCheck();
    }));
    root.querySelectorAll("[data-kv-del]").forEach((b) => (b.onclick = () => {
      const o = getPath(st.def, b.dataset.kvDel) || {};
      delete o[Object.keys(o)[+b.dataset.i]]; setPath(st.def, b.dataset.kvDel, o); render(); scheduleCheck();
    }));
    root.querySelectorAll("[data-kv]").forEach((el) => el.addEventListener("change", () => {
      const path = el.dataset.kv;
      const entries = Object.entries(getPath(st.def, path) || {});
      const i = +el.dataset.i;
      if (el.dataset.part === "k") entries[i][0] = el.value; else entries[i][1] = el.value;
      setPath(st.def, path, Object.fromEntries(entries.filter(([k]) => k !== "")));
      scheduleCheck();
    }));
    root.querySelectorAll("[data-agent]").forEach((b) => (b.onclick = () => { st.def.agent = b.dataset.agent || undefined; render(); scheduleCheck(); }));
    root.querySelectorAll("[data-enabled]").forEach((b) => (b.onclick = () => { st.def.enabled = b.dataset.enabled === "true"; render(); scheduleCheck(); }));
    root.querySelectorAll("[data-view]").forEach((b) => (b.onclick = () => switchView(b.dataset.view)));
    root.querySelector("#b-adv")?.addEventListener("click", () => { st.adv = !st.adv; render(); });

    // Tags
    root.querySelector("#b-addtag")?.addEventListener("click", () => {
      const tags = st.def.tags || (st.def.tags = []);
      tags.push(newTag(tags));
      render();
      const inputs = root.querySelectorAll('[data-path$=".name"][data-path^="tags."]');
      inputs[inputs.length - 1]?.select();
      scheduleCheck();
    });
    root.querySelectorAll("[data-gen]").forEach((b) => (b.onclick = () => {
      const i = +b.dataset.gen, t = st.def.tags[i];
      if (!b.dataset.v) delete t.simulate;
      else if (t.simulate?.kind !== b.dataset.v) {
        // Keep min and max when switching between kinds.
        const keep = t.simulate ? { min: t.simulate.min, max: t.simulate.max } : {};
        t.simulate = { ...defaultGen(t, i, b.dataset.v), ...Object.fromEntries(Object.entries(keep).filter(([, v]) => v !== undefined)) };
      }
      render(); scheduleCheck();
    }));
    root.querySelectorAll("[data-seq]").forEach((el) => el.addEventListener("input", () => {
      const t = st.def.tags[+el.dataset.seq];
      t.simulate.values = el.value.split(",").map((x) => x.trim()).filter((x) => x !== "").map((x) => {
        if (/^-?\d+(\.\d+)?$/.test(x)) return Number(x);
        if (/^(true|false)$/i.test(x)) return /^true$/i.test(x);
        return x;
      });
      scheduleCheck();
    }));
    root.querySelector("#b-simall")?.addEventListener("click", () => {
      st.def.tags.forEach((t, i) => { if (!t.simulate && gensFor(t.datatype || "uint16").length) t.simulate = defaultGen(t, i); });
      render(); scheduleCheck();
    });
    root.querySelector("#b-simnone")?.addEventListener("click", () => {
      st.def.tags.forEach((t) => delete t.simulate);
      render(); scheduleCheck();
    });
    root.querySelector("#b-example")?.addEventListener("click", async () => {
      const yaml = await api("GET", `/api/templates/${st.def.kind}/${st.def.mode}`);
      const r = await api("POST", "/api/services/check", yaml, "yaml");
      if (!r.definition) return;
      const have = new Set((st.def.tags || []).map((t) => t.name));
      st.def.tags = [...(st.def.tags || []), ...r.definition.tags.filter((t) => !have.has(t.name))];
      render(); scheduleCheck();
    });
    root.querySelectorAll("[data-tag-open]").forEach((b) => (b.onclick = () => { const i = +b.dataset.tagOpen; st.open.has(i) ? st.open.delete(i) : st.open.add(i); render(); }));
    root.querySelectorAll("[data-tag-del]").forEach((b) => (b.onclick = () => { st.def.tags.splice(+b.dataset.tagDel, 1); st.open.clear(); render(); scheduleCheck(); }));
    root.querySelectorAll("[data-tag-dup]").forEach((b) => (b.onclick = () => {
      const i = +b.dataset.tagDup, t = clone(st.def.tags[i]);
      t.name = uniqueName(st.def.tags, t.name);
      if (st.def.kind === "modbus" && typeof t.register === "number") t.register = nextRegister(st.def.tags, t.table);
      st.def.tags.splice(i + 1, 0, t); st.open.clear(); render(); scheduleCheck();
    }));
    root.querySelectorAll("[data-writable]").forEach((b) => (b.onclick = () => {
      const t = st.def.tags[+b.dataset.writable];
      if (t.writable === false) delete t.writable; else t.writable = false;
      render(); scheduleCheck();
    }));
    root.querySelector("#b-yaml")?.addEventListener("input", (e) => { st.yaml = e.target.value; });
    wireOhm();

    root.querySelector("#b-save").onclick = save;
    wireTest();
  };

  const wireTest = () => {
    const b = root.querySelector("#b-test");
    if (b) b.onclick = async () => {
      if (st.view === "yaml" && !(await fromYaml())) return;
      st.test = "running"; refreshSide();
      try {
        const q = st.editing ? `?editing=${encodeURIComponent(st.editing)}` : "";
        st.test = await api("POST", `/api/services/test${q}`, cleanDef());
      } catch (e) { st.test = { error: e.message }; }
      refreshSide();
    };
  };

  const fromYaml = async () => {
    const r = await api("POST", "/api/services/check", st.yaml, "yaml");
    if (!r.definition) { st.check = r; refreshSide(); return false; }
    st.def = r.definition;
    st.check = r;
    return true;
  };

  const switchView = async (v) => {
    if (v === st.view) return;
    if (v === "form") {
      if (!(await fromYaml())) return;
    } else {
      clearTimeout(checkTimer);
      await runCheck();
    }
    st.view = v;
    render();
  };

  const save = async () => {
    const msg = root.querySelector("#b-msg");
    if (st.view === "yaml" && !(await fromYaml())) { flash(msg, "Fix the YAML first."); return; }
    st.busy = true;
    try {
      const body = cleanDef();
      const svc = st.editing
        ? await api("PUT", `/api/services/${encodeURIComponent(st.editing)}`, body)
        : await api("POST", "/api/services", body);
      opts.onSaved(svc);
    } catch (e) { flash(msg, e.message); }
    st.busy = false;
  };

  const uniqueName = (tags, base) => {
    const names = new Set(tags.map((t) => t.name));
    const stem = base.replace(/_\d+$/, "");
    let n = 2, name = base;
    while (names.has(name)) name = `${stem}_${n++}`;
    return name;
  };
  const nextRegister = (tags, table) => tags
    .filter((t) => (t.table || "holding_register") === (table || "holding_register") && typeof t.register === "number")
    .reduce((m, t) => Math.max(m, t.register + (t.datatype === "string" ? t.length || 10 : REGS[t.datatype || "uint16"])), 0);
  const newTag = (tags) => {
    const d = st.def, t = { name: uniqueName(tags, `tag_${tags.length + 1}`) };
    if (d.kind === "modbus") { t.datatype = "uint16"; t.register = nextRegister(tags, "holding_register"); }
    if (d.kind === "opcua" || d.kind === "api" || d.kind === "mqtt") t.datatype = "float64";
    if (d.kind === "eip") t.datatype = "float32";
    if (d.kind === "s7") {
      // Next free double word in DB1 after the tags so far.
      const used = tags.map((x) => /^DB1\.DB[BWD](\d+)/i.exec(x.s7_address || "")).filter(Boolean).map((m) => +m[1] + 4);
      t.datatype = "float32"; t.s7_address = `DB1.DBD${used.length ? Math.max(...used) : 0}`;
    }
    if (d.kind === "iolink") {
      const last = tags[tags.length - 1];
      t.datatype = "uint16"; t.port = last?.port || 1;
      t.bit_offset = last ? (last.bit_offset || 0) + (last.bit_length || BITS[last.datatype || "uint16"]) : 0;
    }
    return t;
  };

  render();
  runCheck();
}
