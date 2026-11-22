// Cowpoke web UI: plain JS, no build step. Talks to the REST API under /api.
"use strict";

const $app = document.getElementById("app");
const S = { me: null, services: [], es: null, page: null, timers: [] };
const ROLE_RANK = { viewer: 0, operator: 1, admin: 2 };
const can = (role) => S.me && ROLE_RANK[S.me.role] >= ROLE_RANK[role];

const KINDS = {
  modbus: { label: "Modbus TCP", server: "A simulated device: registers and coils other software reads and writes.", client: "Poll a real Modbus TCP device or gateway." },
  opcua: { label: "OPC UA", server: "An OPC UA server with your tags as variables.", client: "Read and write a real OPC UA server's nodes." },
  api: { label: "API request", server: "Mock an HTTP API with canned routes, and serve tags as JSON.", client: "Call an HTTP endpoint on a timer and read values from the JSON." },
  iolink: { label: "IO-Link", server: "A simulated IO-Link master (ifm IoT Core JSON).", client: "Read sensors through a real ifm IoT Core master." },
};

// ---------------------------------------------------------------- helpers

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

async function api(method, path, body, type) {
  const opts = { method, headers: {} };
  if (body !== undefined) {
    if (type === "yaml") { opts.headers["Content-Type"] = "text/yaml"; opts.body = body; }
    else { opts.headers["Content-Type"] = "application/json"; opts.body = JSON.stringify(body); }
  }
  const r = await fetch(path, opts);
  if (r.status === 401 && path !== "/api/login" && path !== "/api/me") { S.me = null; route(); throw new Error("signed out"); }
  const ct = r.headers.get("content-type") || "";
  const data = r.status === 204 ? null : ct.includes("json") ? await r.json() : await r.text();
  if (!r.ok) throw new Error((data && data.error) || `HTTP ${r.status}`);
  return data;
}

function fmtValue(v) {
  if (v === null || v === undefined) return "—";
  if (typeof v === "number") return Number.isInteger(v) ? String(v) : String(Math.round(v * 1e6) / 1e6);
  return String(v);
}

function ago(ts) {
  if (!ts) return "";
  const s = Math.max(0, (Date.now() - new Date(ts).getTime()) / 1000);
  if (s < 2) return "now";
  if (s < 60) return `${Math.round(s)} s ago`;
  if (s < 3600) return `${Math.round(s / 60)} min ago`;
  return new Date(ts).toLocaleString();
}

function status(st) {
  const s = st || { state: "stopped", detail: "" };
  return `<span class="st-${esc(s.state)}"><span class="dot"></span>${esc(s.state)}</span>`;
}

function flash(el, text, kind = "err") {
  el.innerHTML = text ? `<div class="msg ${kind}">${esc(text)}</div>` : "";
}

function clearPage() {
  S.timers.forEach(clearInterval);
  S.timers = [];
  S.page = null;
}

// ---------------------------------------------------------------- shell

function shell(current, inner) {
  const links = [["#/", "Services", "services"]];
  if (can("admin")) links.push(["#/users", "Users", "users"]);
  links.push(["#/settings", "Settings", "settings"]);
  $app.innerHTML = `
    <header class="top">
      <a class="brand" href="#/"><span class="word">COWPOKE</span><span class="by">by Ohmstead</span></a>
      <nav>${links.map(([h, l, k]) => `<a href="${h}" class="${k === current ? "current" : ""}">${l}</a>`).join("")}</nav>
      <div class="who"><span class="name">${esc(S.me.username)} · ${esc(S.me.role)}</span>
        <button class="small" id="theme" title="Switch light and dark">Theme</button>
        <button class="small" id="logout">Sign out</button></div>
    </header>
    <main>${inner}</main>`;
  document.getElementById("logout").onclick = async () => { await api("POST", "/api/logout").catch(() => {}); S.me = null; route(); };
  document.getElementById("theme").onclick = () => {
    const t = document.documentElement.dataset.theme === "dark" ? "light" : "dark";
    document.documentElement.dataset.theme = t;
    try { localStorage.setItem("cowpoke-theme", t); } catch (e) {}
    if (S.page && S.page.redraw) S.page.redraw();
  };
}

// ---------------------------------------------------------------- live events

function connectEvents() {
  if (S.es) return;
  S.es = new EventSource("/api/events");
  S.es.addEventListener("tag", (e) => { if (S.page && S.page.onTag) S.page.onTag(JSON.parse(e.data)); });
  S.es.addEventListener("status", (e) => { if (S.page && S.page.onStatus) S.page.onStatus(JSON.parse(e.data)); });
  S.es.onerror = () => { if (!S.me) { S.es.close(); S.es = null; } };
}

// ---------------------------------------------------------------- login

function loginPage() {
  clearPage();
  if (S.es) { S.es.close(); S.es = null; }
  $app.innerHTML = `
    <form class="panel login" id="f">
      <div class="word">COWPOKE</div>
      <div class="hatch"></div>
      <label>Username <input name="u" autocomplete="username" required autofocus></label>
      <label>Password <input name="p" type="password" autocomplete="current-password" required></label>
      <div id="m"></div>
      <button class="primary" type="submit">Sign in</button>
    </form>`;
  const f = document.getElementById("f");
  f.onsubmit = async (ev) => {
    ev.preventDefault();
    try {
      const r = await api("POST", "/api/login", { username: f.u.value, password: f.p.value });
      S.me = r.user;
      route();
    } catch (e) { flash(document.getElementById("m"), e.message); }
  };
}

// ---------------------------------------------------------------- services list

async function servicesPage() {
  clearPage();
  shell("services", `
    <div class="row"><div><h1>Services</h1><p class="sub">Each service is a client or a server for one protocol.</p></div>
      <span class="spacer"></span>${can("admin") ? `<a class="btn" href="#/new">New service</a>` : ""}</div>
    <div id="m"></div>
    <div class="panel scroll"><table>
      <thead><tr><th>Service</th><th>Protocol</th><th>Mode</th><th class="num">Tags</th><th>State</th><th class="hide-sm">Detail</th><th></th></tr></thead>
      <tbody id="rows"><tr><td colspan="7" class="muted">Loading…</td></tr></tbody></table></div>`);
  const draw = (list) => {
    S.services = list;
    const rows = document.getElementById("rows");
    if (!rows) return;
    if (!list.length) {
      rows.innerHTML = `<tr><td colspan="7" class="muted">No services yet.${can("admin") ? ` <a href="#/new">Create one</a>.` : ""}</td></tr>`;
      return;
    }
    rows.innerHTML = list.map((s) => `
      <tr class="link" data-id="${esc(s.id)}">
        <td><b>${esc(s.name)}</b><br><span class="tag">${esc(s.id)}</span></td>
        <td>${esc(KINDS[s.kind]?.label || s.kind)}</td>
        <td>${esc(s.mode)}</td>
        <td class="num">${s.tag_count}</td>
        <td class="st-${esc(s.status.state)}">${status(s.status)}</td>
        <td class="hide-sm st-${esc(s.status.state)}"><span class="detail">${esc(s.status.detail)}</span></td>
        <td>${can("operator") ? `<button class="small" data-act="${s.enabled ? "stop" : "start"}">${s.enabled ? "Stop" : "Start"}</button>` : ""}</td>
      </tr>`).join("");
    rows.querySelectorAll("tr.link").forEach((tr) => {
      tr.onclick = (e) => {
        const act = e.target.dataset.act;
        if (act) {
          e.stopPropagation();
          api("POST", `/api/services/${tr.dataset.id}/${act}`).then(load).catch((er) => flash(document.getElementById("m"), er.message));
        } else location.hash = `#/s/${tr.dataset.id}`;
      };
    });
  };
  const load = () => api("GET", "/api/services").then(draw).catch((e) => flash(document.getElementById("m"), e.message));
  S.page = { onStatus: () => load() };
  load();
}

// ---------------------------------------------------------------- new service

async function newPage() {
  clearPage();
  if (!can("admin")) return (location.hash = "#/");
  let kind = "modbus", mode = "server";
  shell("services", `
    <h1>New service</h1><p class="sub">Pick a protocol and a mode, then edit the starter definition.</p>
    <h2>Protocol</h2><div class="grid2" id="kinds"></div>
    <h2>Mode</h2><div class="grid2" id="modes"></div>
    <h2>Definition</h2>
    <textarea id="yaml" spellcheck="false"></textarea>
    <div id="m"></div>
    <div class="row"><button class="primary" id="create">Create and start</button><a class="btn" href="#/">Cancel</a></div>`);
  const draw = async () => {
    document.getElementById("kinds").innerHTML = Object.entries(KINDS).map(([k, v]) =>
      `<button class="choice ${k === kind ? "on" : ""}" data-k="${k}"><b>${esc(v.label)}</b><span>${esc(v[mode])}</span></button>`).join("");
    document.getElementById("modes").innerHTML = ["server", "client"].map((m) =>
      `<button class="choice ${m === mode ? "on" : ""}" data-m="${m}"><b>${m === "server" ? "Server" : "Client"}</b><span>${esc(KINDS[kind][m])}</span></button>`).join("");
    document.querySelectorAll("[data-k]").forEach((b) => (b.onclick = () => { kind = b.dataset.k; draw(); }));
    document.querySelectorAll("[data-m]").forEach((b) => (b.onclick = () => { mode = b.dataset.m; draw(); }));
    document.getElementById("yaml").value = await api("GET", `/api/templates/${kind}/${mode}`);
  };
  document.getElementById("create").onclick = async () => {
    try {
      const s = await api("POST", "/api/services", document.getElementById("yaml").value, "yaml");
      location.hash = `#/s/${s.id}`;
    } catch (e) { flash(document.getElementById("m"), e.message); }
  };
  draw();
}

// ---------------------------------------------------------------- one service

async function servicePage(id, tab = "tags") {
  clearPage();
  let svc;
  try { svc = await api("GET", `/api/services/${encodeURIComponent(id)}`); }
  catch (e) { shell("services", `<div class="msg err">${esc(e.message)}</div>`); return; }
  const tabs = [["tags", "Tags"]];
  if (can("admin")) tabs.push(["definition", "Definition"]);
  if (can("admin") && svc.kind === "opcua") tabs.push(["certs", "Certificates"]);
  const mount = svc.kind === "api" && svc.mode === "server" && !(svc.definition.settings || {}).port
    ? `<p class="detail">Routes: <a href="/x/${esc(svc.id)}/">${esc(location.origin)}/x/${esc(svc.id)}/</a></p>` : "";
  shell("services", `
    <div class="row"><div>
      <h1>${esc(svc.name)}</h1>
      <p class="sub"><span class="tag">${esc(svc.id)}</span> ${esc(KINDS[svc.kind]?.label || svc.kind)} ${esc(svc.mode)}</p></div>
      <span class="spacer"></span>
      ${can("operator") ? `<button id="toggle">${svc.enabled ? "Stop" : "Start"}</button><button id="restart" ${svc.enabled ? "" : "disabled"}>Restart</button>` : ""}
      ${can("admin") ? `<button class="danger" id="del">Delete</button>` : ""}
    </div>
    <div class="panel pad" id="st"></div>${mount}
    <div id="m"></div>
    <div class="tabs">${tabs.map(([k, l]) => `<button class="${k === tab ? "on" : ""}" data-tab="${k}">${l}</button>`).join("")}</div>
    <div id="body"></div>`);
  const drawStatus = (st) => {
    const el = document.getElementById("st");
    if (el) el.innerHTML = `<div class="st-${esc(st.state)}">${status(st)} <span class="detail">${esc(st.detail)}</span></div>`;
  };
  drawStatus(svc.status);
  const m = document.getElementById("m");
  document.querySelectorAll("[data-tab]").forEach((b) => (b.onclick = () => (location.hash = `#/s/${id}/${b.dataset.tab}`)));
  if (can("operator")) {
    document.getElementById("toggle").onclick = () => api("POST", `/api/services/${id}/${svc.enabled ? "stop" : "start"}`).then(() => servicePage(id, tab)).catch((e) => flash(m, e.message));
    document.getElementById("restart").onclick = () => api("POST", `/api/services/${id}/restart`).then(() => servicePage(id, tab)).catch((e) => flash(m, e.message));
  }
  if (can("admin")) {
    document.getElementById("del").onclick = async () => {
      if (!confirm(`Delete ${svc.name}? Its definition is removed from cowpoke.yaml. History files stay on disk.`)) return;
      await api("DELETE", `/api/services/${id}`).catch((e) => flash(m, e.message));
      location.hash = "#/";
    };
  }
  const page = { onStatus: (all) => all[id] && drawStatus(all[id]) };
  S.page = page;
  const body = document.getElementById("body");
  if (tab === "tags") tagsTab(svc, body, page, m);
  else if (tab === "definition") definitionTab(svc, body, m);
  else if (tab === "certs") certsTab(svc, body, m);
}

async function tagsTab(svc, body, page, m) {
  const tags = await api("GET", `/api/services/${svc.id}/tags`);
  const writable = (t) => can("operator") && (svc.mode === "server" || t.writable);
  body.innerHTML = `<div class="panel tabbody scroll"><table>
    <thead><tr><th>Tag</th><th>Type</th><th class="num">Value</th><th class="hide-sm">Updated</th><th>${can("operator") ? "Write" : ""}</th><th></th></tr></thead>
    <tbody>${tags.length ? tags.map((t) => `
      <tr data-tag="${esc(t.name)}">
        <td><span class="mono">${esc(t.name)}</span>${t.description ? `<br><span class="muted">${esc(t.description)}</span>` : ""}</td>
        <td><span class="tag">${esc(t.datatype)}</span></td>
        <td class="num mono val q-${esc(t.quality || "")}">${esc(fmtValue(t.value))}${t.units ? ` <span class="muted">${esc(t.units)}</span>` : ""}</td>
        <td class="hide-sm muted ts" data-ts="${esc(t.ts || "")}">${esc(ago(t.ts))}</td>
        <td>${writable(t) ? `<form class="row w"><input class="mono" name="v" size="9" aria-label="New value for ${esc(t.name)}"><button class="small">Set</button></form>` : `<span class="muted">${can("operator") ? "read-only" : ""}</span>`}</td>
        <td>${t.historized ? `<button class="small" data-trend>Trend</button>` : ""}</td>
      </tr>`).join("") : `<tr><td colspan="6" class="muted">No tags. Add them in the definition.</td></tr>`}</tbody></table></div>
    <div id="trend"></div>`;
  const units = Object.fromEntries(tags.map((t) => [t.name, t.units || ""]));
  page.onTag = (e) => {
    if (e.service !== svc.id) return;
    const tr = body.querySelector(`tr[data-tag="${CSS.escape(e.tag)}"]`);
    if (!tr) return;
    const v = tr.querySelector(".val");
    v.className = `num mono val q-${e.quality}`;
    v.innerHTML = `${esc(fmtValue(e.value))}${units[e.tag] ? ` <span class="muted">${esc(units[e.tag])}</span>` : ""}`;
    const ts = tr.querySelector(".ts");
    ts.dataset.ts = e.ts;
    ts.textContent = ago(e.ts);
  };
  S.timers.push(setInterval(() => body.querySelectorAll(".ts").forEach((td) => (td.textContent = ago(td.dataset.ts))), 1000));
  body.querySelectorAll("form.w").forEach((f) => {
    f.onsubmit = async (ev) => {
      ev.preventDefault();
      const tag = f.closest("tr").dataset.tag;
      let v = f.v.value.trim();
      try { v = JSON.parse(v); } catch (e) { /* plain text */ }
      try { await api("PUT", `/api/services/${svc.id}/tags/${encodeURIComponent(tag)}`, { value: v }); f.v.value = ""; flash(m, ""); }
      catch (e) { flash(m, `${tag}: ${e.message}`); }
    };
  });
  body.querySelectorAll("[data-trend]").forEach((b) => (b.onclick = () => trend(svc, b.closest("tr").dataset.tag, page)));
}

async function trend(svc, tag, page) {
  const box = document.getElementById("trend");
  const ranges = [["15m", 15], ["1h", 60], ["24h", 1440], ["7d", 10080], ["30d", 43200]];
  let minutes = 60;
  box.innerHTML = `<div class="panel trend"><div class="row"><b class="mono">${esc(tag)}</b><span class="spacer"></span>
    ${ranges.map(([l, n]) => `<button class="small" data-min="${n}">${l}</button>`).join("")}</div>
    <canvas></canvas><div class="muted" id="tinfo"></div></div>`;
  const canvas = box.querySelector("canvas");
  let pts = [];
  const load = async () => {
    const to = new Date(), from = new Date(to - minutes * 60000);
    const r = await api("GET", `/api/services/${svc.id}/history/${encodeURIComponent(tag)}?from=${from.toISOString()}&to=${to.toISOString()}&points=600`);
    pts = r.points;
    box.querySelectorAll("[data-min]").forEach((b) => b.classList.toggle("primary", +b.dataset.min === minutes));
    document.getElementById("tinfo").textContent = pts.length ? `${pts.length} points` : "No history in this range yet.";
    draw(from.getTime(), to.getTime());
  };
  const draw = (from, to) => {
    const css = getComputedStyle(document.documentElement);
    const dpr = window.devicePixelRatio || 1;
    const W = canvas.clientWidth, H = canvas.clientHeight;
    canvas.width = W * dpr; canvas.height = H * dpr;
    const g = canvas.getContext("2d");
    g.scale(dpr, dpr);
    g.clearRect(0, 0, W, H);
    const padL = 56, padB = 22, padT = 8, padR = 8;
    const lo0 = Math.min(...pts.map((p) => p.min ?? p.v)), hi0 = Math.max(...pts.map((p) => p.max ?? p.v));
    const span = hi0 - lo0 || Math.abs(hi0) || 1;
    const lo = lo0 - span * 0.08, hi = hi0 + span * 0.08;
    const x = (t) => padL + ((t - from) / (to - from)) * (W - padL - padR);
    const y = (v) => padT + (1 - (v - lo) / (hi - lo)) * (H - padT - padB);
    g.font = `11px ${css.getPropertyValue("--font-code")}`;
    g.fillStyle = css.getPropertyValue("--muted");
    g.strokeStyle = css.getPropertyValue("--rule");
    g.lineWidth = 1;
    for (let i = 0; i <= 4; i++) {
      const v = lo + ((hi - lo) * i) / 4, yy = Math.round(y(v)) + 0.5;
      g.beginPath(); g.moveTo(padL, yy); g.lineTo(W - padR, yy); g.stroke();
      if (pts.length) g.fillText(fmtValue(Math.round(v * 1000) / 1000), 4, yy + 4);
    }
    for (let i = 0; i <= 4; i++) {
      const t = from + ((to - from) * i) / 4;
      const label = minutes > 1440 ? new Date(t).toLocaleDateString() : new Date(t).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
      g.fillText(label, Math.min(x(t), W - 60), H - 6);
    }
    if (!pts.length) return;
    if (pts[0].min !== undefined) {
      g.fillStyle = css.getPropertyValue("--teal");
      g.globalAlpha = 0.18;
      g.beginPath();
      pts.forEach((p, i) => (i ? g.lineTo(x(p.t), y(p.max)) : g.moveTo(x(p.t), y(p.max))));
      [...pts].reverse().forEach((p) => g.lineTo(x(p.t), y(p.min)));
      g.fill();
      g.globalAlpha = 1;
    }
    g.strokeStyle = css.getPropertyValue("--teal");
    g.lineWidth = 1.6;
    g.beginPath();
    pts.forEach((p, i) => (i ? g.lineTo(x(p.t), y(p.v)) : g.moveTo(x(p.t), y(p.v))));
    g.stroke();
  };
  box.querySelectorAll("[data-min]").forEach((b) => (b.onclick = () => { minutes = +b.dataset.min; load(); }));
  page.redraw = () => load();
  S.timers.push(setInterval(load, 10000));
  load();
  box.scrollIntoView({ behavior: "smooth", block: "nearest" });
}

async function definitionTab(svc, body, m) {
  const yaml = await api("GET", `/api/services/${svc.id}/yaml`);
  body.innerHTML = `<div class="panel pad tabbody">
    <p class="muted">The whole service as YAML: settings, tags and history. Saving restarts the service.</p>
    <textarea id="yaml" spellcheck="false"></textarea>
    <div class="row"><button class="primary" id="save">Save</button><button id="tpl">Show the starter for this protocol</button></div></div>`;
  const ta = document.getElementById("yaml");
  ta.value = yaml;
  document.getElementById("save").onclick = async () => {
    try {
      const s = await api("PUT", `/api/services/${svc.id}`, ta.value, "yaml");
      flash(m, "Saved.", "ok");
      if (s.id !== svc.id) location.hash = `#/s/${s.id}/definition`;
    } catch (e) { flash(m, e.message); }
  };
  document.getElementById("tpl").onclick = async () => flash(m, await api("GET", `/api/templates/${svc.kind}/${svc.mode}`), "ok");
}

async function certsTab(svc, body, m) {
  const load = async () => {
    const r = await api("GET", `/api/services/${svc.id}/certs`);
    body.innerHTML = `<div class="panel pad tabbody">
      <p class="muted">${svc.mode === "server" ? "Client certificates this server has seen." : "Server certificates this client has seen."}
        New ones arrive as rejected; trust one to let it connect. This service's own certificate thumbprint:
        <span class="mono">${esc(r.own_thumbprint || "not created yet (start the service once)")}</span></p>
      <div class="scroll"><table><thead><tr><th>Certificate</th><th>Thumbprint</th><th>State</th><th></th></tr></thead><tbody>
      ${r.certificates.length ? r.certificates.map((c) => `<tr data-f="${esc(c.file)}">
        <td>${esc(c.common_name || c.file)}</td><td class="mono detail">${esc(c.thumbprint)}</td>
        <td>${c.trusted ? `<span class="st-running"><span class="dot"></span>trusted</span>` : `<span class="st-degraded"><span class="dot"></span>rejected</span>`}</td>
        <td class="row">${c.trusted ? `<button class="small" data-a="reject">Reject</button>` : `<button class="small primary" data-a="trust">Trust</button>`}
          <button class="small danger" data-a="delete">Delete</button></td></tr>`).join("")
        : `<tr><td colspan="4" class="muted">None yet.</td></tr>`}
      </tbody></table></div></div>`;
    body.querySelectorAll("[data-a]").forEach((b) => (b.onclick = async () => {
      const f = b.closest("tr").dataset.f;
      try { await api("POST", `/api/services/${svc.id}/certs/${encodeURIComponent(f)}/${b.dataset.a}`); load(); }
      catch (e) { flash(m, e.message); }
    }));
  };
  load().catch((e) => flash(m, e.message));
}

// ---------------------------------------------------------------- users

async function usersPage() {
  clearPage();
  if (!can("admin")) return (location.hash = "#/");
  shell("users", `<h1>Users</h1>
    <p class="sub">Viewers read everything. Operators also write tag values and start or stop services. Admins also edit services, users and certificates.</p>
    <div id="m"></div>
    <div class="panel scroll"><table><thead><tr><th>User</th><th>Role</th><th>New password</th><th></th></tr></thead><tbody id="rows"></tbody></table></div>
    <h2>Add a user</h2>
    <form class="panel pad row" id="add">
      <label>Username <input name="u" required></label>
      <label>Password <input name="p" type="password" minlength="8" required></label>
      <label>Role ${roleRadios("new", "viewer")}</label>
      <button class="primary">Add</button>
    </form>`);
  const m = document.getElementById("m");
  const load = async () => {
    const users = await api("GET", "/api/users");
    document.getElementById("rows").innerHTML = users.map((u) => `
      <tr data-u="${esc(u.username)}"><td class="mono">${esc(u.username)}</td>
      <td>${roleRadios(u.username, u.role)}</td>
      <td><input type="password" class="pw" minlength="8" placeholder="unchanged" aria-label="New password for ${esc(u.username)}"></td>
      <td class="row"><button class="small" data-save>Save</button><button class="small danger" data-del>Delete</button></td></tr>`).join("");
    document.querySelectorAll("tr[data-u]").forEach((tr) => {
      const u = tr.dataset.u;
      tr.querySelector("[data-save]").onclick = async () => {
        const role = tr.querySelector("input[type=radio]:checked").value;
        const pw = tr.querySelector(".pw").value;
        try { await api("PUT", `/api/users/${encodeURIComponent(u)}`, { role, password: pw || null }); flash(m, `Saved ${u}.`, "ok"); load(); }
        catch (e) { flash(m, e.message); }
      };
      tr.querySelector("[data-del]").onclick = async () => {
        if (!confirm(`Delete ${u}?`)) return;
        try { await api("DELETE", `/api/users/${encodeURIComponent(u)}`); load(); } catch (e) { flash(m, e.message); }
      };
    });
  };
  const f = document.getElementById("add");
  f.onsubmit = async (ev) => {
    ev.preventDefault();
    const role = f.querySelector("input[type=radio]:checked").value;
    try { await api("PUT", `/api/users/${encodeURIComponent(f.u.value)}`, { role, password: f.p.value }); f.reset(); flash(m, "User added.", "ok"); load(); }
    catch (e) { flash(m, e.message); }
  };
  load().catch((e) => flash(m, e.message));
}

// Radio buttons, not a native <select> (DESIGN.md: never the OS menu).
function roleRadios(name, current) {
  return `<span class="row">${["viewer", "operator", "admin"].map((r) =>
    `<label class="row" style="display:flex;gap:4px"><input type="radio" name="role-${esc(name)}" value="${r}" ${r === current ? "checked" : ""}>${r}</label>`).join("")}</span>`;
}

// ---------------------------------------------------------------- settings

async function settingsPage() {
  clearPage();
  shell("settings", `<h1>Settings</h1><div id="m"></div>
    <h2>Your password</h2>
    <form class="panel pad row" id="pw">
      <label>New password <input name="p" type="password" minlength="8" autocomplete="new-password" required></label>
      <button class="primary">Change</button>
    </form>
    ${can("admin") ? `
    <h2>Configuration</h2>
    <div class="panel pad">
      <p class="muted">Every service lives in <span class="mono">cowpoke.yaml</span>. Download it to back it up or move it to another Cowpoke; importing replaces all services.</p>
      <div class="row"><a class="btn" href="/api/config" download="cowpoke.yaml">Download cowpoke.yaml</a>
      <label class="btn">Import… <input type="file" id="imp" accept=".yaml,.yml" hidden></label></div>
    </div>` : ""}
    <h2>About</h2>
    <div class="panel pad"><p class="muted">Cowpoke by Ohmstead. Free to self-host under the GNU AGPL v3.</p></div>`);
  const m = document.getElementById("m");
  const f = document.getElementById("pw");
  f.onsubmit = async (ev) => {
    ev.preventDefault();
    try { await api("PUT", `/api/users/${encodeURIComponent(S.me.username)}`, { password: f.p.value }); f.reset(); flash(m, "Password changed.", "ok"); }
    catch (e) { flash(m, e.message); }
  };
  const imp = document.getElementById("imp");
  if (imp) imp.onchange = async () => {
    const file = imp.files[0];
    if (!file || !confirm(`Replace every service with the ones in ${file.name}?`)) return;
    try { await api("PUT", "/api/config", await file.text(), "yaml"); flash(m, "Imported.", "ok"); }
    catch (e) { flash(m, e.message); }
  };
}

// ---------------------------------------------------------------- router

async function route() {
  if (!S.me) {
    try { S.me = await api("GET", "/api/me"); } catch (e) { S.me = null; }
  }
  if (!S.me) return loginPage();
  connectEvents();
  const parts = location.hash.replace(/^#\/?/, "").split("/").map(decodeURIComponent);
  if (parts[0] === "s" && parts[1]) return servicePage(parts[1], parts[2] || "tags");
  if (parts[0] === "new") return newPage();
  if (parts[0] === "users") return usersPage();
  if (parts[0] === "settings") return settingsPage();
  return servicesPage();
}

window.addEventListener("hashchange", route);
route();
