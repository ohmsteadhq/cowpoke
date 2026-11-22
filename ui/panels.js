// Cowpoke boards (DIY dashboards) and alarms. Loaded after app.js and uses its helpers:
// S, can, api, esc, flash, shell, clearPage, fmtValue, ago, LIVE, liveKey, liveSet.
"use strict";

const P = {
  es: null,          // alarm event stream
  alarms: [],        // last /api/alarms answer
  services: null,    // [{id, name, tags: [{name, datatype, units, historized, writable, ...}]}]
  pillTimer: null,
};

const SEVERITIES = ["low", "medium", "high", "critical"];
const RANGES = [["15m", 15], ["1h", 60], ["24h", 1440], ["7d", 10080], ["30d", 43200]];
const WIDGET_TYPES = [
  ["trend", "Trend", "Up to 8 tags on a time chart."],
  ["value", "Value", "One big number."],
  ["gauge", "Gauge", "A dial with alarm limits marked."],
  ["lamp", "Lamp", "A bool as a light, optionally a switch."],
  ["alarms", "Alarms", "Active alarms with acknowledge."],
];
const ALARM_KINDS = [
  ["high", "High limit"], ["low", "Low limit"], ["equals", "Equals"],
  ["bad_quality", "Bad quality"], ["stale", "Stale"], ["rate", "Rate of change"],
];
const HOOK_KINDS = [["webhook", "Webhook"], ["write", "Write a tag"], ["email", "Email"], ["mqtt", "MQTT"]];
const SERIES_VARS = ["--s1", "--s2", "--s3", "--s4", "--s5", "--s6", "--s7", "--s8"];

// ---------------------------------------------------------------- shared bits

// Radio buttons, not a native <select> (DESIGN.md: never the OS menu).
function radios(name, options, current) {
  return `<span class="pn-radios">${options.map(([v, l]) =>
    `<label><input type="radio" name="${esc(name)}" value="${esc(v)}" ${String(v) === String(current) ? "checked" : ""}>${esc(l)}</label>`).join("")}</span>`;
}
const picked = (root, name) => root.querySelector(`input[name="${CSS.escape(name)}"]:checked`)?.value;
const pnSlug = (s) => s.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "").slice(0, 64);

async function loadServices(force) {
  if (P.services && !force) return P.services;
  const list = await api("GET", "/api/services");
  P.services = await Promise.all(list.map(async (s) => {
    const tags = await api("GET", `/api/services/${encodeURIComponent(s.id)}/tags`).catch(() => []);
    for (const t of tags) if (t.value !== undefined && t.value !== null) liveSet(s.id, t.name, t.value, t.quality, t.ts);
    return { id: s.id, name: s.name, kind: s.kind, mode: s.mode, tags };
  }));
  return P.services;
}

function tagInfo(service, tag) {
  return P.services?.find((s) => s.id === service)?.tags.find((t) => t.name === tag);
}

const liveOf = (service, tag) => LIVE.get(liveKey(service, tag));
const cssVar = (name) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();
const shownAlarms = () => P.alarms.filter((a) => a.shown);
const sevChip = (sev) => `<span class="pn-sev pn-sev-${esc(sev)}">${esc(sev)}</span>`;

function alarmStateText(a) {
  const st = a.state || {};
  if (st.active && !st.acked) return "active";
  if (st.active) return "active · acknowledged";
  return "cleared · not acknowledged";
}

/// A text field for `service.tag` with a list of matching tags under it (no OS menu).
function wireTagField(input, list, filterFn) {
  const show = () => {
    const q = input.value.toLowerCase();
    const rows = [];
    for (const s of P.services || []) for (const t of s.tags) {
      if (filterFn && !filterFn(t)) continue;
      const key = `${s.id}.${t.name}`;
      if (q && !key.toLowerCase().includes(q)) continue;
      rows.push(`<button type="button" class="pn-tagopt" data-k="${esc(key)}"><span class="mono">${esc(key)}</span><span class="muted">${esc(t.datatype)}${t.units ? ` · ${esc(t.units)}` : ""}</span></button>`);
      if (rows.length >= 8) break;
    }
    list.innerHTML = rows.join("");
    list.hidden = !rows.length || (rows.length === 1 && list.querySelector("[data-k]")?.dataset.k === input.value);
    list.querySelectorAll("[data-k]").forEach((b) => b.addEventListener("mousedown", (e) => {
      e.preventDefault();
      input.value = b.dataset.k;
      list.hidden = true;
      input.dispatchEvent(new Event("change"));
    }));
  };
  input.addEventListener("focus", show);
  input.addEventListener("input", show);
  input.addEventListener("blur", () => setTimeout(() => (list.hidden = true), 100));
  list.hidden = true;
}

// ---------------------------------------------------------------- alarm stream + top-bar count

async function refreshAlarms() {
  if (!S.me) return;
  try { P.alarms = await api("GET", "/api/alarms"); } catch (e) { return; }
  updatePill();
  if (S.page && S.page.onAlarms) S.page.onAlarms(P.alarms);
}

function updatePill() {
  const el = document.getElementById("alarm-pill");
  if (!el) return;
  const shown = shownAlarms();
  const unacked = shown.filter((a) => !a.state.acked).length;
  el.hidden = shown.length === 0;
  el.className = `alarm-pill ${unacked ? "hot" : ""}`;
  el.textContent = unacked ? `${unacked} alarm${unacked === 1 ? "" : "s"}` : `${shown.length} acknowledged`;
}

function ensureAlarmStream() {
  if (!S.me) return;
  if (!P.pillTimer) {
    P.pillTimer = setInterval(refreshAlarms, 10000);
    refreshAlarms();
  }
  if (P.es) return;
  P.es = new EventSource("/api/alarms/events");
  P.es.addEventListener("alarm", (e) => {
    const n = JSON.parse(e.data);
    if (n.type === "alarm") refreshAlarms();
    if (S.page && S.page.onNotice) S.page.onNotice(n);
  });
  P.es.onerror = () => { if (!S.me) { P.es.close(); P.es = null; } };
}

// Every page's top bar shows the alarm count (app.js renders the empty pill).
const appShell = shell;
shell = function (current, inner) {
  appShell(current, inner);
  updatePill();
};

async function ackAlarm(id, m) {
  try { await api("POST", `/api/alarms/${encodeURIComponent(id)}/ack`); await refreshAlarms(); }
  catch (e) { if (m) flash(m, e.message); }
}

// ---------------------------------------------------------------- router hook (called from app.js)

function panelsRoute(parts) {
  ensureAlarmStream();
  if (parts[0] === "boards") { boardsPage(); return true; }
  if (parts[0] === "b" && parts[1]) { boardPage(parts[1], parts[2] === "edit"); return true; }
  if (parts[0] === "alarms") { alarmsPage(parts[1] || "active"); return true; }
  if (parts[0] === "insights") { insightsPage(parts[1] || "open"); return true; }
  return false;
}

// ---------------------------------------------------------------- insights

const AREAS = { comms: "Communications", alarms: "Alarms", historian: "Historian" };
const LEVELS = { warn: ["Check", "pn-sev pn-sev-high"], suggest: ["Suggestion", "pn-sev pn-sev-medium"], info: ["Note", "pn-sev"] };

async function insightsPage(tab) {
  clearPage();
  const tabs = [["open", "Open"], ["comms", "Communications"], ["alarms", "Alarms"], ["historian", "Historian"], ["dismissed", "Dismissed"]];
  shell("insights", `
    <div class="row"><div><h1>Insights</h1>
      <p class="sub">What Cowpoke has learned from watching every tag, service and alarm: polling faster than values change, flapping connections, bad reads, noisy history and alarm hygiene. Suggestions only: nothing changes until you change it.</p></div>
      <span class="spacer"></span>${can("operator") ? `<button id="run">Analyze now</button>` : ""}</div>
    <div id="m"></div>
    <div class="tabs">${tabs.map(([k, l]) => `<button class="${k === tab ? "on" : ""}" data-tab="${k}">${l}</button>`).join("")}</div>
    <div id="body" class="pn-tabbody"></div>`);
  document.querySelectorAll("[data-tab]").forEach((b) => (b.onclick = () => (location.hash = `#/insights/${b.dataset.tab}`)));
  const m = document.getElementById("m");
  const body = document.getElementById("body");
  const draw = (r) => {
    const all = r.insights || [];
    const list = all.filter((i) => tab === "dismissed" ? i.dismissed : !i.dismissed && (tab === "open" || i.area === tab));
    const since = r.watching_since ? new Date(r.watching_since).toLocaleString() : "not yet";
    const when = r.generated ? new Date(r.generated).toLocaleTimeString() : "not yet";
    body.innerHTML = `<div class="panel pad">
      <p class="muted small">Watching ${r.tags_watched || 0} tags since ${esc(since)} · last analyzed ${esc(when)} · ${r.dismissed || 0} dismissed.
        Insights need at least 30 minutes of watching; some (frozen values, standing alarms) need hours.</p>
      ${list.length ? list.map((i) => {
        const [lab, cls] = LEVELS[i.level] || LEVELS.info;
        const where = [i.service ? `<a class="mono" href="#/s/${encodeURIComponent(i.service)}">${esc(i.service)}</a>` : "", i.tag ? `<span class="mono">${esc(i.tag)}</span>` : ""].filter(Boolean).join(" · ");
        return `<div class="pn-insight" data-id="${esc(i.id)}">
          <div class="row"><span class="${cls}">${lab}</span><span class="tag">${esc(AREAS[i.area] || i.area)}</span>${where ? `<span class="detail">${where}</span>` : ""}
            <span class="spacer"></span>${can("operator") ? `<button class="small" data-act="${i.dismissed ? "restore" : "dismiss"}">${i.dismissed ? "Restore" : "Dismiss"}</button>` : ""}</div>
          <h3>${esc(i.title)}</h3>
          <p>${esc(i.detail)}</p>
          ${i.evidence.length ? `<ul class="detail">${i.evidence.map((e) => `<li>${esc(e)}</li>`).join("")}</ul>` : ""}
          <p class="pn-fix"><b>Suggestion:</b> ${esc(i.suggestion)}</p>
        </div>`;
      }).join("") : `<p class="muted">${tab === "dismissed" ? "Nothing dismissed." : "Nothing to suggest yet."}</p>`}
    </div>`;
    body.querySelectorAll("[data-act]").forEach((b) => (b.onclick = async () => {
      const id = b.closest("[data-id]").dataset.id;
      try {
        await api(b.dataset.act === "dismiss" ? "POST" : "DELETE", `/api/insights/${encodeURIComponent(id)}/dismiss`);
        load();
      } catch (e) { flash(m, e.message); }
    }));
  };
  const load = () => api("GET", "/api/insights").then(draw).catch((e) => flash(m, e.message));
  const run = document.getElementById("run");
  if (run) run.onclick = async () => {
    run.disabled = true;
    try { draw(await api("POST", "/api/insights/run")); } catch (e) { flash(m, e.message); }
    run.disabled = false;
  };
  load();
}

// ---------------------------------------------------------------- boards list

async function boardsPage() {
  clearPage();
  shell("boards", `
    <div class="row"><div><h1>Boards</h1><p class="sub">Dashboards you build from trends, values, gauges, lamps and alarm lists. Everyone sees them.</p></div></div>
    <div id="m"></div>
    <div class="pn-boards" id="list"><div class="panel pad muted">Loading…</div></div>
    ${can("operator") ? `<h2>New board</h2>
    <form class="panel pad row" id="new">
      <label>Name <input name="name" required placeholder="Line 1 overview"></label>
      <label>Id <input name="id" class="mono" required pattern="[a-z0-9_\\-]{1,64}" placeholder="line-1" title="a-z, 0-9, - and _"></label>
      <button class="primary">Create board</button>
    </form>` : ""}`);
  const m = document.getElementById("m");
  const list = await api("GET", "/api/dashboards").catch((e) => { flash(m, e.message); return []; });
  document.getElementById("list").innerHTML = list.length ? list.map((d) => `
    <a class="panel pn-board-card" href="#/b/${encodeURIComponent(d.id)}">
      <b>${esc(d.name)}</b><span class="tag">${esc(d.id)}</span>
      <span class="muted">${d.widgets} widget${d.widgets === 1 ? "" : "s"}${d.updated ? ` · edited ${esc(ago(d.updated))}${d.updated_by ? ` by ${esc(d.updated_by)}` : ""}` : ""}</span>
    </a>`).join("") : `<div class="panel pad muted">No boards yet.${can("operator") ? " Create one below." : ""}</div>`;
  const f = document.getElementById("new");
  if (f) {
    f.name.oninput = () => { if (!f.id.dataset.touched) f.id.value = pnSlug(f.name.value); };
    f.id.oninput = () => { f.id.dataset.touched = "1"; };
    f.onsubmit = async (ev) => {
      ev.preventDefault();
      try {
        const d = await api("POST", "/api/dashboards", { id: f.id.value, name: f.name.value, widgets: [] });
        location.hash = `#/b/${encodeURIComponent(d.id)}/edit`;
      } catch (e) { flash(m, e.message); }
    };
  }
}

// ---------------------------------------------------------------- one board

async function boardPage(id, editing) {
  clearPage();
  let board;
  try { board = await api("GET", `/api/dashboards/${encodeURIComponent(id)}`); }
  catch (e) { shell("boards", `<div class="msg err">${esc(e.message)}</div>`); return; }
  if (!can("operator")) editing = false;
  await loadServices(true).catch(() => {});
  await refreshAlarms();

  shell("boards", `
    <div class="row pn-board-head"><div><h1>${esc(board.name || board.id)}</h1>
      <p class="sub"><a href="#/boards">Boards</a> · <span class="tag">${esc(board.id)}</span>${board.updated_by ? ` · edited by ${esc(board.updated_by)}` : ""}</p></div>
      <span class="spacer"></span>
      ${can("operator") ? (editing
        ? `<button id="add">Add widget</button><button class="primary" id="done">Done</button>`
        : `<a class="btn" href="#/b/${encodeURIComponent(id)}/edit">Edit</a>`) : ""}
    </div>
    <div id="m"></div>
    <div id="editor"></div>
    <div class="pn-grid ${editing ? "editing" : ""}" id="grid"></div>
    ${editing ? `<div class="row pn-board-foot"><label>Board name <input id="bname" value="${esc(board.name)}"></label>
      <span class="spacer"></span><button class="danger" id="delboard">Delete board</button></div>` : ""}`);
  const m = document.getElementById("m");
  const grid = document.getElementById("grid");
  const charts = [];

  const save = async () => {
    try { board = await api("PUT", `/api/dashboards/${encodeURIComponent(id)}`, board); flash(m, ""); }
    catch (e) { flash(m, e.message); }
  };

  const render = () => {
    charts.length = 0;
    if (!board.widgets.length) {
      grid.innerHTML = `<div class="panel pad muted pn-empty">${editing ? "Add a widget to start." : `This board is empty.${can("operator") ? ` <a href="#/b/${encodeURIComponent(id)}/edit">Edit it</a> to add widgets.` : ""}`}</div>`;
      return;
    }
    grid.innerHTML = board.widgets.map((w, i) => `
      <section class="panel pn-w pn-${esc(w.type)}" style="--w:${w.width};--h:${w.height}" data-i="${i}" ${editing ? 'draggable="true"' : ""}>
        <header class="pn-wh"><b>${esc(w.title || defaultTitle(w))}</b>
          ${editing ? `<span class="pn-tools">
            <button class="small" data-a="left" title="Move earlier" aria-label="Move earlier">←</button><button class="small" data-a="right" title="Move later" aria-label="Move later">→</button>
            <button class="small" data-a="edit">Edit</button><button class="small danger" data-a="del">Remove</button></span>` : ""}
        </header>
        <div class="pn-wb"></div>
      </section>`).join("");
    grid.querySelectorAll(".pn-w").forEach((el) => {
      const c = mountWidget(board.widgets[+el.dataset.i], el.querySelector(".pn-wb"), m);
      if (c) charts.push(c);
    });
    if (editing) wireEditing();
  };

  const wireEditing = () => {
    grid.querySelectorAll("[data-a]").forEach((b) => (b.onclick = () => {
      const i = +b.closest(".pn-w").dataset.i;
      const ws = board.widgets;
      if (b.dataset.a === "edit") return widgetEditor(ws[i], (nw) => { ws[i] = nw; save().then(render); });
      if (b.dataset.a === "left" && i > 0) [ws[i - 1], ws[i]] = [ws[i], ws[i - 1]];
      else if (b.dataset.a === "right" && i < ws.length - 1) [ws[i + 1], ws[i]] = [ws[i], ws[i + 1]];
      else if (b.dataset.a === "del") ws.splice(i, 1);
      else return;
      save().then(render);
    }));
    // Drop a widget on another to move it there.
    let from = null;
    grid.querySelectorAll(".pn-w").forEach((el) => {
      el.addEventListener("dragstart", (e) => { from = +el.dataset.i; el.classList.add("dragging"); e.dataTransfer.effectAllowed = "move"; });
      el.addEventListener("dragend", () => el.classList.remove("dragging"));
      el.addEventListener("dragover", (e) => { e.preventDefault(); el.classList.add("over"); });
      el.addEventListener("dragleave", () => el.classList.remove("over"));
      el.addEventListener("drop", (e) => {
        e.preventDefault();
        el.classList.remove("over");
        const to = +el.dataset.i;
        if (from === null || from === to) return;
        const [w] = board.widgets.splice(from, 1);
        board.widgets.splice(to, 0, w);
        from = null;
        save().then(render);
      });
    });
  };

  if (editing) {
    document.getElementById("add").onclick = () => widgetEditor(null, (nw) => { board.widgets.push(nw); save().then(render); });
    document.getElementById("done").onclick = () => (location.hash = `#/b/${encodeURIComponent(id)}`);
    const bn = document.getElementById("bname");
    bn.onchange = () => { board.name = bn.value; save(); };
    document.getElementById("delboard").onclick = async () => {
      if (!confirm(`Delete the board ${board.name || board.id}? Its widgets go with it; tags and history stay.`)) return;
      try { await api("DELETE", `/api/dashboards/${encodeURIComponent(id)}`); location.hash = "#/boards"; } catch (e) { flash(m, e.message); }
    };
  }

  S.page = {
    onTag: (d) => grid.querySelectorAll(".pn-w").forEach((el) => {
      const w = board.widgets[+el.dataset.i];
      if (w && el._update && w.series?.some((s) => s.service === d.service && s.tag === d.tag)) el._update();
    }),
    onAlarms: () => grid.querySelectorAll(".pn-w").forEach((el) => el._update && el._update()),
    redraw: () => charts.forEach((c) => c.draw()),
  };
  S.timers.push(setInterval(() => charts.forEach((c) => c.tick()), 5000));
  render();
}

function defaultTitle(w) {
  if (w.type === "alarms") return w.service ? `Alarms · ${w.service}` : "Alarms";
  return w.series.map((s) => s.label || s.tag).join(", ") || WIDGET_TYPES.find((t) => t[0] === w.type)?.[1] || "";
}

// ---------------------------------------------------------------- widgets

function mountWidget(w, body, m) {
  const el = body.closest(".pn-w");
  if (w.type === "trend") return trendWidget(w, body);
  if (w.type === "alarms") {
    const draw = () => {
      const list = shownAlarms().filter((a) => !w.service || a.service === w.service)
        .sort((a, b) => (+b.state.active - +a.state.active) || SEVERITIES.indexOf(b.severity) - SEVERITIES.indexOf(a.severity));
      body.innerHTML = list.length ? `<ul class="pn-alist">${list.map((a) => `
        <li class="${a.state.active ? "active" : ""}">
          ${sevChip(a.severity)}<span class="msgtxt">${esc(a.state.message || a.name)}</span>
          <span class="muted when">${esc(a.state.active ? ago(a.state.since) : "cleared")}</span>
          ${!a.state.acked && can("operator") ? `<button class="small" data-ack="${esc(a.id)}">Ack</button>` : ""}
        </li>`).join("")}</ul>` : `<p class="muted pn-allclear">No alarms.</p>`;
      body.querySelectorAll("[data-ack]").forEach((b) => (b.onclick = () => ackAlarm(b.dataset.ack, m)));
    };
    el._update = draw;
    draw();
    return null;
  }
  const s = w.series[0];
  if (!s) { body.innerHTML = `<p class="muted">No tag picked.</p>`; return null; }
  const draw = () => {
    const lv = liveOf(s.service, s.tag);
    const info = tagInfo(s.service, s.tag);
    const q = lv?.quality || "bad";
    const alarming = shownAlarms().some((a) => a.service === s.service && a.tag === s.tag && a.state.active);
    if (w.type === "value") {
      body.innerHTML = `<div class="pn-value q-${esc(q)} ${alarming ? "alarming" : ""}">${esc(fmtValue(lv?.value))}${info?.units ? ` <small>${esc(info.units)}</small>` : ""}</div>
        <div class="pn-meta"><span class="mono">${esc(s.service)}.${esc(s.tag)}</span><span>${esc(q === "good" ? ago(lv?.ts) : q)}</span></div>`;
    } else if (w.type === "gauge") {
      body.innerHTML = gaugeSvg(w, lv, info, s, alarming);
    } else {
      const on = lv?.value === true || (typeof lv?.value === "number" && lv.value !== 0);
      const canSwitch = w.switch && can("operator") && info && info.writable !== false;
      body.innerHTML = `<div class="pn-lamp ${on ? "on" : ""} q-${esc(q)} ${alarming ? "alarming" : ""}"><span class="bulb"></span><b>${q === "good" ? (on ? "On" : "Off") : "No signal"}</b></div>
        ${canSwitch ? `<button class="small" data-switch>${on ? "Turn off" : "Turn on"}</button>` : ""}`;
      const b = body.querySelector("[data-switch]");
      if (b) b.onclick = async () => {
        try { await api("PUT", `/api/services/${encodeURIComponent(s.service)}/tags/${encodeURIComponent(s.tag)}`, { value: !on }); }
        catch (e) { flash(m, `${s.tag}: ${e.message}`); }
      };
    }
  };
  el._update = draw;
  draw();
  return null;
}

function gaugeSvg(w, lv, info, s, alarming) {
  const lo = w.min ?? 0, hi = w.max ?? 100;
  const v = typeof lv?.value === "number" ? lv.value : null;
  const frac = v === null ? 0 : Math.max(0, Math.min(1, (v - lo) / ((hi - lo) || 1)));
  const A0 = -210, SWEEP = 240, R = 66, cx = 90, cy = 82;
  const pt = (f, r = R) => { const a = ((A0 + SWEEP * f) * Math.PI) / 180; return [cx + r * Math.cos(a), cy + r * Math.sin(a)]; };
  const arc = (f0, f1) => {
    const [x0, y0] = pt(f0), [x1, y1] = pt(f1);
    return `M${x0.toFixed(1)},${y0.toFixed(1)} A${R},${R} 0 ${(f1 - f0) * SWEEP > 180 ? 1 : 0} 1 ${x1.toFixed(1)},${y1.toFixed(1)}`;
  };
  const ticks = P.alarms
    .filter((a) => a.service === s.service && a.tag === s.tag && (a.kind === "high" || a.kind === "low") && a.limit != null)
    .map((a) => {
      const f = Math.max(0, Math.min(1, (a.limit - lo) / ((hi - lo) || 1)));
      const [x0, y0] = pt(f, R - 10), [x1, y1] = pt(f, R + 10);
      return `<line class="limit" x1="${x0.toFixed(1)}" y1="${y0.toFixed(1)}" x2="${x1.toFixed(1)}" y2="${y1.toFixed(1)}"><title>${esc(a.name || a.id)}: ${esc(a.kind)} ${esc(a.limit)}</title></line>`;
    }).join("");
  const q = lv?.quality || "bad";
  return `<svg class="pn-gauge ${alarming ? "alarming" : ""} q-${esc(q)}" viewBox="0 0 180 140" role="img" aria-label="${esc(s.tag)} ${esc(fmtValue(v))}">
      <path class="track" d="${arc(0, 1)}"/>
      ${v !== null ? `<path class="fill" d="${arc(0, Math.max(frac, 0.001))}"/>` : ""}
      ${ticks}
      <text class="val" x="${cx}" y="${cy + 6}">${esc(fmtValue(v))}</text>
      <text class="unit" x="${cx}" y="${cy + 24}">${esc(info?.units || "")}</text>
      <text class="end" x="${pt(0)[0].toFixed(1)}" y="${(pt(0)[1] + 16).toFixed(1)}">${esc(fmtValue(lo))}</text>
      <text class="end" x="${pt(1)[0].toFixed(1)}" y="${(pt(1)[1] + 16).toFixed(1)}">${esc(fmtValue(hi))}</text>
    </svg>`;
}

function trendWidget(w, body) {
  const minutes = (RANGES.find((r) => r[0] === w.range) || RANGES[1])[1];
  body.innerHTML = `<div class="pn-legend">${w.series.map((s, i) => `<span><i style="background:var(${SERIES_VARS[i % 8]})"></i>${esc(s.label || s.tag)}</span>`).join("")}<span class="muted pn-range">${esc(w.range || "1h")}</span></div><canvas></canvas>`;
  const canvas = body.querySelector("canvas");
  let hist = w.series.map(() => []);
  let loadedAt = 0;
  const load = async () => {
    const to = new Date(), from = new Date(to - minutes * 60000);
    hist = await Promise.all(w.series.map((s) =>
      api("GET", `/api/services/${encodeURIComponent(s.service)}/history/${encodeURIComponent(s.tag)}?from=${from.toISOString()}&to=${to.toISOString()}&points=400`)
        .then((r) => r.points).catch(() => [])));
    loadedAt = Date.now();
    draw();
  };
  const draw = () => {
    const dpr = window.devicePixelRatio || 1;
    const W = canvas.clientWidth, H = canvas.clientHeight;
    if (!W || !H) return;
    canvas.width = W * dpr; canvas.height = H * dpr;
    const g = canvas.getContext("2d");
    g.setTransform(dpr, 0, 0, dpr, 0, 0);
    g.clearRect(0, 0, W, H);
    const now = Date.now(), from = now - minutes * 60000;
    // Stored history, then the live values this page has seen since.
    const lines = w.series.map((s, i) => {
      const pts = hist[i].map((p) => ({ t: p.t, v: p.v, lo: p.min, hi: p.max }));
      const last = pts.length ? pts[pts.length - 1].t : from;
      for (const [t, v] of liveOf(s.service, s.tag)?.pts || []) if (t > last && t >= from) pts.push({ t, v });
      return pts;
    });
    let lo = Infinity, hi = -Infinity;
    lines.forEach((pts) => pts.forEach((p) => { lo = Math.min(lo, p.lo ?? p.v); hi = Math.max(hi, p.hi ?? p.v); }));
    const padL = 44, padB = 18, padT = 6, padR = 8;
    g.font = `10px ${cssVar("--font-code")}`;
    g.fillStyle = cssVar("--muted");
    g.strokeStyle = cssVar("--rule");
    g.lineWidth = 1;
    if (lo === Infinity) { g.fillText("No data in this range yet.", padL, H / 2); return; }
    if (hi === lo) { hi += 1; lo -= 1; }
    const span = hi - lo; lo -= span * 0.06; hi += span * 0.06;
    const x = (t) => padL + ((t - from) / (now - from)) * (W - padL - padR);
    const y = (v) => padT + (1 - (v - lo) / (hi - lo)) * (H - padT - padB);
    for (let k = 0; k <= 3; k++) {
      const v = lo + ((hi - lo) * k) / 3, yy = Math.round(y(v)) + 0.5;
      g.beginPath(); g.moveTo(padL, yy); g.lineTo(W - padR, yy); g.stroke();
      g.fillText(fmtValue(Math.round(v * 100) / 100), 2, yy + 3);
    }
    for (let k = 0; k <= 3; k++) {
      const t = from + ((now - from) * k) / 3;
      const label = minutes > 1440 ? new Date(t).toLocaleDateString([], { month: "short", day: "numeric" }) : new Date(t).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
      g.fillText(label, Math.min(x(t), W - 44), H - 4);
    }
    lines.forEach((pts, i) => {
      if (!pts.length) return;
      const color = cssVar(SERIES_VARS[i % 8]);
      if (lines.length === 1 && pts.some((p) => p.lo !== undefined)) {
        g.fillStyle = color; g.globalAlpha = 0.15; g.beginPath();
        pts.forEach((p, k) => (k ? g.lineTo(x(p.t), y(p.hi ?? p.v)) : g.moveTo(x(p.t), y(p.hi ?? p.v))));
        [...pts].reverse().forEach((p) => g.lineTo(x(p.t), y(p.lo ?? p.v)));
        g.fill(); g.globalAlpha = 1;
      }
      g.strokeStyle = color; g.lineWidth = 1.6; g.beginPath();
      pts.forEach((p, k) => (k ? g.lineTo(x(p.t), y(p.v)) : g.moveTo(x(p.t), y(p.v))));
      g.stroke();
    });
  };
  load();
  // Short ranges reload often; long ones rely on the live tail between reloads.
  const every = minutes <= 60 ? 15000 : 60000;
  return { draw, tick: () => (Date.now() - loadedAt > every ? load() : draw()) };
}

// ---------------------------------------------------------------- widget editor

const TAG_FITS = {
  trend: (t) => t.datatype !== "string",
  value: () => true,
  gauge: (t) => t.datatype !== "string" && t.datatype !== "bool",
  lamp: (t) => t.datatype === "bool",
};

async function widgetEditor(existing, done) {
  const box = document.getElementById("editor");
  const services = await loadServices();
  const w = existing ? JSON.parse(JSON.stringify(existing)) : { type: "trend", title: "", width: 6, height: 2, series: [], range: "1h" };
  let filterText = "";
  const draw = () => {
    const multi = w.type === "trend";
    box.innerHTML = `<form class="panel pad pn-editor">
      <div class="row"><b>${existing ? "Edit widget" : "New widget"}</b><span class="spacer"></span><button type="button" class="small" id="wcancel">Cancel</button></div>
      <div class="pn-types">${WIDGET_TYPES.map(([k, l, d]) => `<button type="button" class="choice ${w.type === k ? "on" : ""}" data-t="${k}"><b>${l}</b><span>${d}</span></button>`).join("")}</div>
      <div class="pn-fields">
        <label>Title <input name="title" value="${esc(w.title)}" placeholder="${esc(defaultTitle(w))}"></label>
        <label>Width ${radios("width", [[3, "¼"], [4, "⅓"], [6, "½"], [8, "⅔"], [12, "Full"]], w.width)}</label>
        <label>Height ${radios("height", [[1, "1"], [2, "2"], [3, "3"], [4, "4"]], w.height)}</label>
        ${w.type === "trend" ? `<label>Range ${radios("range", RANGES.map(([r]) => [r, r]), w.range || "1h")}</label>` : ""}
        ${w.type === "gauge" ? `<label>Scale from <input name="min" type="number" step="any" value="${esc(w.min ?? 0)}"></label><label>to <input name="max" type="number" step="any" value="${esc(w.max ?? 100)}"></label>` : ""}
        ${w.type === "lamp" ? `<label class="pn-check"><input type="checkbox" name="switch" ${w.switch ? "checked" : ""}> Operators can switch it</label>` : ""}
        ${w.type === "alarms" ? `<label>Show alarms from ${radios("svc", [["", "Every service"], ...services.map((s) => [s.id, s.name])], w.service || "")}</label>` : ""}
      </div>
      ${w.type !== "alarms" ? `<div class="pn-tagpick">
        <div class="row"><b>${multi ? "Tags (up to 8)" : "Tag"}</b><input id="tfilter" placeholder="Filter tags" class="mono" value="${esc(filterText)}"></div>
        <div class="pn-chosen">${w.series.map((s, i) => `<span class="pn-chip"><i style="background:var(${SERIES_VARS[i % 8]})"></i><span class="mono">${esc(s.service)}.${esc(s.tag)}</span><button type="button" class="small" data-rm="${i}" aria-label="Remove ${esc(s.tag)}">×</button></span>`).join("") || `<span class="muted">None picked yet.</span>`}</div>
        <div class="pn-taglist" id="tlist"></div>
      </div>` : ""}
      <div id="wm"></div>
      <div class="row"><button class="primary">${existing ? "Save widget" : "Add to board"}</button></div>
    </form>`;
    const f = box.querySelector("form");
    box.querySelectorAll("[data-t]").forEach((b) => (b.onclick = () => {
      collect(f);
      w.type = b.dataset.t;
      const fits = TAG_FITS[w.type];
      w.series = w.type === "alarms" ? [] : w.series.filter((s) => { const t = tagInfo(s.service, s.tag); return !t || fits(t); }).slice(0, w.type === "trend" ? 8 : 1);
      if (w.type === "trend" && w.height < 2) w.height = 2;
      draw();
    }));
    box.querySelector("#wcancel").onclick = () => (box.innerHTML = "");
    box.querySelectorAll("[data-rm]").forEach((b) => (b.onclick = () => { collect(f); w.series.splice(+b.dataset.rm, 1); draw(); }));
    const tlist = box.querySelector("#tlist");
    const filter = box.querySelector("#tfilter");
    const listTags = () => {
      filterText = filter.value;
      const q = filterText.toLowerCase();
      const fits = TAG_FITS[w.type];
      const rows = [];
      for (const s of services) for (const t of s.tags) {
        if (!fits(t)) continue;
        const key = `${s.id}.${t.name}`;
        if (q && !key.toLowerCase().includes(q) && !(s.name || "").toLowerCase().includes(q)) continue;
        const on = w.series.some((x) => x.service === s.id && x.tag === t.name);
        rows.push(`<button type="button" class="pn-tagopt ${on ? "on" : ""}" data-s="${esc(s.id)}" data-tag="${esc(t.name)}">
          <span class="mono">${esc(key)}</span><span class="muted">${esc(t.datatype)}${t.units ? ` · ${esc(t.units)}` : ""}${t.historized ? " · history" : ""}</span></button>`);
      }
      tlist.innerHTML = rows.slice(0, 80).join("") || `<p class="muted">No matching tags.</p>`;
      tlist.querySelectorAll("[data-s]").forEach((b) => (b.onclick = () => {
        collect(f);
        const i = w.series.findIndex((x) => x.service === b.dataset.s && x.tag === b.dataset.tag);
        if (i >= 0) w.series.splice(i, 1);
        else if (multi) { if (w.series.length < 8) w.series.push({ service: b.dataset.s, tag: b.dataset.tag }); }
        else w.series = [{ service: b.dataset.s, tag: b.dataset.tag }];
        draw();
      }));
    };
    if (filter) { filter.oninput = listTags; listTags(); }
    f.onsubmit = (ev) => {
      ev.preventDefault();
      collect(f);
      if (w.type !== "alarms" && !w.series.length) return flash(box.querySelector("#wm"), "Pick a tag.");
      box.innerHTML = "";
      done(w);
    };
  };
  const collect = (f) => {
    w.title = f.title.value.trim();
    w.width = +(picked(f, "width") || w.width);
    w.height = +(picked(f, "height") || w.height);
    if (w.type === "trend") w.range = picked(f, "range") || "1h"; else delete w.range;
    if (w.type === "gauge") { w.min = f.min.value === "" ? null : +f.min.value; w.max = f.max.value === "" ? null : +f.max.value; } else { delete w.min; delete w.max; }
    if (w.type === "lamp") w.switch = !!f.switch?.checked; else delete w.switch;
    if (w.type === "alarms") w.service = picked(f, "svc") || null; else delete w.service;
  };
  draw();
  box.scrollIntoView({ behavior: "smooth", block: "start" });
}

// ---------------------------------------------------------------- alarms page

async function alarmsPage(tab) {
  clearPage();
  const tabs = [["active", "Active"], ["defs", "Alarms"]];
  if (can("admin")) tabs.push(["hooks", "Hooks"]);
  tabs.push(["log", "Log"]);
  shell("alarms", `
    <div class="row"><div><h1>Alarms</h1><p class="sub">Limits, states and timeouts on any tag, with hooks that run when an alarm raises, clears or is acknowledged.</p></div></div>
    <div id="m"></div>
    <div class="tabs">${tabs.map(([k, l]) => `<button class="${k === tab ? "on" : ""}" data-tab="${k}">${l}</button>`).join("")}</div>
    <div id="body" class="pn-tabbody"></div>`);
  document.querySelectorAll("[data-tab]").forEach((b) => (b.onclick = () => (location.hash = `#/alarms/${b.dataset.tab}`)));
  const m = document.getElementById("m");
  const body = document.getElementById("body");
  await refreshAlarms();
  if (tab === "defs") defsTab(body, m);
  else if (tab === "hooks" && can("admin")) hooksTab(body, m);
  else if (tab === "log") logTab(body, m);
  else activeTab(body, m);
}

function activeTab(body, m) {
  const draw = () => {
    const list = shownAlarms().sort((a, b) => (+b.state.active - +a.state.active) || SEVERITIES.indexOf(b.severity) - SEVERITIES.indexOf(a.severity));
    const unacked = list.filter((a) => !a.state.acked).length;
    body.innerHTML = `<div class="panel scroll">
      <div class="row pn-bar"><b>${list.length ? `${list.length} alarm${list.length === 1 ? "" : "s"}, ${unacked} not acknowledged` : "All clear"}</b><span class="spacer"></span>
        ${unacked && can("operator") ? `<button class="small" id="ackall">Acknowledge all</button>` : ""}</div>
      <table><thead><tr><th>Severity</th><th>Alarm</th><th>State</th><th class="hide-sm">Since</th><th></th></tr></thead><tbody>
      ${list.map((a) => `<tr class="${a.state.active ? "pn-row-active" : ""}">
        <td>${sevChip(a.severity)}</td>
        <td><b>${esc(a.state.message || a.name || a.id)}</b><br><span class="mono muted">${esc(a.service)}.${esc(a.tag)}</span></td>
        <td>${esc(alarmStateText(a))}${a.state.acked_by ? `<br><span class="muted">by ${esc(a.state.acked_by)}</span>` : ""}</td>
        <td class="hide-sm muted">${esc(ago(a.state.since))}</td>
        <td>${!a.state.acked && can("operator") ? `<button class="small" data-ack="${esc(a.id)}">Acknowledge</button>` : ""}</td>
      </tr>`).join("") || `<tr><td colspan="5" class="muted">No active or unacknowledged alarms.</td></tr>`}
      </tbody></table></div>`;
    body.querySelectorAll("[data-ack]").forEach((b) => (b.onclick = () => ackAlarm(b.dataset.ack, m)));
    const all = body.querySelector("#ackall");
    if (all) all.onclick = async () => { try { await api("POST", "/api/alarms/ack-all"); refreshAlarms(); } catch (e) { flash(m, e.message); } };
  };
  S.page = { onAlarms: draw };
  S.timers.push(setInterval(draw, 15000));
  draw();
}

async function defsTab(body, m) {
  await loadServices();
  const hooks = can("admin") ? await api("GET", "/api/hooks").catch(() => []) : [];
  let formOpen = false;
  const draw = () => {
    if (formOpen) return;
    body.innerHTML = `<div class="panel scroll"><table>
      <thead><tr><th>Alarm</th><th>Condition</th><th class="hide-sm">Hooks</th><th>Now</th><th></th></tr></thead><tbody>
      ${P.alarms.map((a) => `<tr>
        <td><b>${esc(a.name || a.id)}</b> ${sevChip(a.severity)}${a.enabled ? "" : ` <span class="tag">off</span>`}<br><span class="mono muted">${esc(a.service)}.${esc(a.tag)}</span></td>
        <td>${esc(conditionText(a))}</td>
        <td class="hide-sm">${(a.hooks || []).map((h) => `<span class="tag">${esc(h)}</span>`).join(" ") || `<span class="muted">none</span>`}</td>
        <td>${a.state?.active ? `<span class="st-degraded"><span class="dot"></span>active</span>` : `<span class="muted mono">${esc(fmtValue(a.current?.value))}</span>`}</td>
        <td>${can("admin") ? `<button class="small" data-edit="${esc(a.id)}">Edit</button>` : ""}</td>
      </tr>`).join("") || `<tr><td colspan="5" class="muted">No alarms yet.</td></tr>`}
      </tbody></table></div>
      ${can("admin") ? `<div class="row pn-actions"><button class="primary" id="newalarm">New alarm</button><span class="spacer"></span>
        <a class="btn" href="/api/alarms/config" download="alarms.yaml">Download alarms.yaml</a>
        <label class="btn">Import… <input type="file" id="imp" accept=".yaml,.yml" hidden></label></div>
      <div id="aform"></div>` : ""}`;
    if (!can("admin")) return;
    body.querySelector("#newalarm").onclick = () => alarmForm(null);
    body.querySelectorAll("[data-edit]").forEach((b) => (b.onclick = () => alarmForm(P.alarms.find((a) => a.id === b.dataset.edit))));
    body.querySelector("#imp").onchange = async (ev) => {
      const file = ev.target.files[0];
      if (!file || !confirm(`Replace every alarm and hook with the ones in ${file.name}?`)) return;
      try { await api("PUT", "/api/alarms/config", await file.text(), "yaml"); await refreshAlarms(); draw(); flash(m, "Imported.", "ok"); }
      catch (e) { flash(m, e.message); }
    };
  };
  const alarmForm = (a) => {
    formOpen = true;
    const box = body.querySelector("#aform");
    const d = a ? JSON.parse(JSON.stringify(a)) : { id: "", name: "", service: "", tag: "", kind: "high", severity: "medium", delay_s: 0, deadband: 0, hooks: [], enabled: true };
    const close = () => { formOpen = false; draw(); };
    const drawForm = () => {
      const k = d.kind;
      box.innerHTML = `<form class="panel pad pn-editor">
        <div class="row"><b>${a ? `Edit ${esc(a.name || a.id)}` : "New alarm"}</b><span class="spacer"></span><button type="button" class="small" id="acancel">Cancel</button></div>
        <div class="pn-fields">
          <label>Name <input name="name" value="${esc(d.name)}" required placeholder="Tank level high"></label>
          <label>Id <input name="id" class="mono" value="${esc(d.id)}" required pattern="[a-z0-9_\\-]{1,64}" placeholder="tank-high"></label>
          <label class="pn-tagfield">Tag <input name="tag" class="mono" autocomplete="off" value="${esc(d.service ? `${d.service}.${d.tag}` : "")}" required placeholder="service.tag"><span class="pn-suggest"></span></label>
        </div>
        <label>Condition ${radios("kind", ALARM_KINDS, k)}</label>
        <div class="pn-fields">
          ${k === "high" || k === "low" ? `<label>Limit <input name="limit" type="number" step="any" required value="${esc(d.limit ?? "")}"></label>` : ""}
          ${k === "equals" ? `<label>Value <input name="value" class="mono" required value="${esc(d.value === undefined ? "" : JSON.stringify(d.value))}" placeholder='true, 3 or "text"'></label>` : ""}
          ${k === "stale" ? `<label>No update for (s) <input name="stale_s" type="number" min="1" required value="${esc(d.stale_s ?? 60)}"></label>` : ""}
          ${k === "rate" ? `<label>Faster than (per min) <input name="rate_per_min" type="number" step="any" min="0" required value="${esc(d.rate_per_min ?? "")}"></label>` : ""}
          ${k === "high" || k === "low" || k === "rate" ? `<label>Deadband <input name="deadband" type="number" step="any" min="0" value="${esc(d.deadband || 0)}"></label>` : ""}
          <label>On-delay (s) <input name="delay_s" type="number" min="0" value="${esc(d.delay_s || 0)}"></label>
        </div>
        <label>Severity ${radios("severity", SEVERITIES.map((s) => [s, s]), d.severity)}</label>
        <label>Message <input name="message" value="${esc(d.message || "")}" placeholder="Default: built from the condition. Placeholders: {{value}} {{limit}} {{tag}} {{service}} {{name}}"></label>
        <div><b class="pn-lbl">Hooks</b> ${hooks.length ? hooks.map((h) => `<label class="pn-check"><input type="checkbox" name="hook" value="${esc(h.id)}" ${d.hooks?.includes(h.id) ? "checked" : ""}>${esc(h.name || h.id)} <span class="muted">${esc(h.kind)}</span></label>`).join("") : `<span class="muted">None yet. Add them on the Hooks tab.</span>`}</div>
        <label class="pn-check"><input type="checkbox" name="enabled" ${d.enabled !== false ? "checked" : ""}> Enabled</label>
        <div id="am"></div>
        <div class="row"><button class="primary">Save alarm</button>${a ? `<span class="spacer"></span><button type="button" class="danger" id="adel">Delete alarm</button>` : ""}</div>
      </form>`;
      const f = box.querySelector("form");
      wireTagField(f.tag, f.querySelector(".pn-suggest"), (t) => (k === "high" || k === "low" || k === "rate") ? t.datatype !== "string" && t.datatype !== "bool" : true);
      f.name.oninput = () => { if (!a && !f.id.dataset.touched) f.id.value = pnSlug(f.name.value); };
      f.id.oninput = () => { f.id.dataset.touched = "1"; };
      f.querySelectorAll('input[name="kind"]').forEach((r) => (r.onchange = () => { grab(f); d.kind = r.value; drawForm(); }));
      box.querySelector("#acancel").onclick = close;
      const am = box.querySelector("#am");
      const del = box.querySelector("#adel");
      if (del) del.onclick = async () => {
        if (!confirm(`Delete the alarm ${a.name || a.id}?`)) return;
        try { await api("DELETE", `/api/alarms/${encodeURIComponent(a.id)}`); await refreshAlarms(); close(); } catch (e) { flash(am, e.message); }
      };
      f.onsubmit = async (ev) => {
        ev.preventDefault();
        grab(f);
        const out = Object.fromEntries(Object.entries(d).filter(([key, v]) => !["shown", "state", "current"].includes(key) && v !== undefined));
        try {
          if (a) await api("PUT", `/api/alarms/${encodeURIComponent(a.id)}`, out);
          else await api("POST", "/api/alarms", out);
          await refreshAlarms();
          close();
          flash(m, "Saved.", "ok");
        } catch (e) { flash(am, e.message); }
      };
    };
    const grab = (f) => {
      d.name = f.name.value.trim();
      d.id = f.id.value.trim();
      const [svc, ...rest] = f.tag.value.trim().split(".");
      d.service = svc; d.tag = rest.join(".");
      d.kind = picked(f, "kind") || d.kind;
      d.severity = picked(f, "severity") || d.severity;
      const num = (n) => (f[n] && f[n].value !== "" ? +f[n].value : undefined);
      d.limit = num("limit"); d.stale_s = num("stale_s"); d.rate_per_min = num("rate_per_min");
      d.deadband = num("deadband") || 0; d.delay_s = num("delay_s") || 0;
      if (f.value) { try { d.value = JSON.parse(f.value.value); } catch (e) { d.value = f.value.value; } } else delete d.value;
      d.message = f.message.value.trim() || undefined;
      d.hooks = [...f.querySelectorAll('input[name="hook"]:checked')].map((x) => x.value);
      d.enabled = f.enabled.checked;
    };
    drawForm();
    box.scrollIntoView({ behavior: "smooth", block: "start" });
  };
  S.page = { onAlarms: draw };
  draw();
}

function conditionText(a) {
  const db = a.deadband ? `, deadband ${a.deadband}` : "";
  const delay = a.delay_s ? `, after ${a.delay_s} s` : "";
  switch (a.kind) {
    case "high": return `above ${a.limit}${db}${delay}`;
    case "low": return `below ${a.limit}${db}${delay}`;
    case "equals": return `equals ${JSON.stringify(a.value)}${delay}`;
    case "bad_quality": return `bad quality${delay}`;
    case "stale": return `no update for ${a.stale_s} s`;
    case "rate": return `changes faster than ${a.rate_per_min}/min${db}${delay}`;
    default: return a.kind;
  }
}

// ---------------------------------------------------------------- hooks tab

async function hooksTab(body, m) {
  let hooks = [];
  await loadServices();
  const load = async () => { hooks = await api("GET", "/api/hooks").catch((e) => { flash(m, e.message); return []; }); draw(); };
  const draw = () => {
    body.innerHTML = `<div class="panel scroll"><table>
      <thead><tr><th>Hook</th><th>Runs on</th><th class="hide-sm">Used by</th><th>Last run</th><th></th></tr></thead><tbody>
      ${hooks.map((h) => `<tr>
        <td><b>${esc(h.name || h.id)}</b> <span class="tag">${esc(h.kind)}</span>${h.enabled ? "" : ` <span class="tag">off</span>`}<br><span class="muted mono">${esc(hookTarget(h))}</span></td>
        <td>${(h.on || []).map((o) => `<span class="tag">${esc(o)}</span>`).join(" ")}</td>
        <td class="hide-sm">${h.used_by.map((x) => `<span class="tag">${esc(x)}</span>`).join(" ") || `<span class="muted">no alarms</span>`}</td>
        <td>${h.last ? `<span class="${h.last.ok ? "st-running" : "st-error"}"><span class="dot"></span>${h.last.ok ? "ok" : "failed"}</span> <span class="muted">${esc(ago(h.last.time))}</span><br><span class="detail">${esc(h.last.detail)}</span>` : `<span class="muted">never</span>`}</td>
        <td><span class="row"><button class="small" data-test="${esc(h.id)}">Test</button><button class="small" data-edit="${esc(h.id)}">Edit</button></span></td>
      </tr>`).join("") || `<tr><td colspan="5" class="muted">No hooks yet.</td></tr>`}
      </tbody></table></div>
      <div class="row pn-actions"><button class="primary" id="newhook">New hook</button></div>
      <div id="hform"></div>`;
    body.querySelector("#newhook").onclick = () => hookForm(null);
    body.querySelectorAll("[data-edit]").forEach((b) => (b.onclick = () => hookForm(hooks.find((h) => h.id === b.dataset.edit))));
    body.querySelectorAll("[data-test]").forEach((b) => (b.onclick = async () => {
      b.disabled = true; b.textContent = "Testing…";
      try { const r = await api("POST", `/api/hooks/${encodeURIComponent(b.dataset.test)}/test`); flash(m, `${b.dataset.test}: ${r.ok ? "worked" : "failed"}: ${r.detail}`, r.ok ? "ok" : "err"); }
      catch (e) { flash(m, e.message); }
      load();
    }));
  };
  const hookForm = (h) => {
    const box = body.querySelector("#hform");
    const d = h ? JSON.parse(JSON.stringify(h)) : { id: "", name: "", kind: "webhook", on: ["raise", "clear"], enabled: true, method: "POST" };
    delete d.last; delete d.used_by;
    const drawForm = () => {
      const k = d.kind;
      const field = (n, label, attrs = "", val = d[n]) => `<label>${label} <input name="${n}" ${attrs} value="${esc(val ?? "")}"></label>`;
      box.innerHTML = `<form class="panel pad pn-editor">
        <div class="row"><b>${h ? `Edit ${esc(h.name || h.id)}` : "New hook"}</b><span class="spacer"></span><button type="button" class="small" id="hcancel">Cancel</button></div>
        <div class="pn-fields">${field("name", "Name", 'required placeholder="Slack #plant"')}${field("id", "Id", 'class="mono" required pattern="[a-z0-9_\\-]{1,64}" placeholder="slack-plant"')}</div>
        <label>Kind ${radios("kind", HOOK_KINDS, k)}</label>
        <div><b class="pn-lbl">Runs when an alarm</b> ${[["raise", "raises"], ["clear", "clears"], ["ack", "is acknowledged"]].map(([v, l]) => `<label class="pn-check"><input type="checkbox" name="on" value="${v}" ${d.on?.includes(v) ? "checked" : ""}>${l}</label>`).join("")}</div>
        <div class="pn-fields">
        ${k === "webhook" ? `${field("url", "URL", 'class="mono" required type="url" placeholder="https://hooks.slack.com/services/…"')}
          <label>Method ${radios("method", [["POST", "POST"], ["PUT", "PUT"], ["GET", "GET"]], d.method || "POST")}</label>` : ""}
        ${k === "write" ? `<label class="pn-tagfield">Tag <input name="wtag" class="mono" autocomplete="off" required value="${esc(d.service ? `${d.service}.${d.tag}` : "")}" placeholder="service.tag"><span class="pn-suggest"></span></label>
          ${field("value", "Write on raise", 'class="mono" required placeholder="true"', d.value === undefined ? "" : JSON.stringify(d.value))}
          ${field("clear_value", "Write on clear", 'class="mono" placeholder="false (blank: nothing)"', d.clear_value === undefined || d.clear_value === null ? "" : JSON.stringify(d.clear_value))}` : ""}
        ${k === "email" ? `${field("host", "SMTP server", 'required placeholder="smtp.example.com"')}${field("port", "Port", 'type="number" placeholder="587"')}
          <label>Security ${radios("security", [["starttls", "STARTTLS"], ["tls", "TLS"], ["none", "None"]], d.security || "starttls")}</label>
          ${field("username", "Username", 'autocomplete="off"')}${field("password", "Password", 'type="password" autocomplete="new-password"')}
          ${field("from", "From", 'required placeholder="Cowpoke <alarms@example.com>"')}${field("to", "To (comma-separated)", "required", (d.to || []).join(", "))}
          ${field("subject", "Subject", 'placeholder="[Cowpoke] {{severity}} {{event}}: {{name}}"')}` : ""}
        ${k === "mqtt" ? `${field("host", "Broker", 'required placeholder="mqtt.example.com"')}${field("port", "Port", 'type="number" placeholder="1883"')}
          ${field("topic", "Topic", 'class="mono" required placeholder="cowpoke/alarms/{{alarm}}"')}
          ${field("username", "Username", 'autocomplete="off"')}${field("password", "Password", 'type="password" autocomplete="new-password"')}
          <label>QoS ${radios("qos", [[0, "0"], [1, "1"]], d.qos || 0)}</label><label class="pn-check"><input type="checkbox" name="retain" ${d.retain ? "checked" : ""}> Retain</label>` : ""}
        </div>
        ${k !== "write" ? `<label>${k === "mqtt" ? "Payload" : "Body"} <span class="muted">Blank sends ${k === "email" ? "a plain summary" : "the alarm event as JSON"}. Placeholders: {{name}} {{event}} {{severity}} {{message}} {{value}} {{service}} {{tag}} {{time}} {{user}}</span>
          <textarea name="body" class="pn-small-ta" placeholder='${k === "email" ? "{{message}}" : '{"text": "{{message}}"}'}'>${esc(bodyText(d, k))}</textarea></label>` : ""}
        <label class="pn-check"><input type="checkbox" name="enabled" ${d.enabled !== false ? "checked" : ""}> Enabled</label>
        <div id="hm"></div>
        <div class="row"><button class="primary">Save hook</button><button type="button" id="htry">Try it</button>${h ? `<span class="spacer"></span><button type="button" class="danger" id="hdel">Delete hook</button>` : ""}</div>
      </form>`;
      const f = box.querySelector("form");
      if (f.wtag) wireTagField(f.wtag, f.querySelector(".pn-suggest"));
      f.querySelectorAll('input[name="kind"]').forEach((r) => (r.onchange = () => { grab(f); d.kind = r.value; drawForm(); }));
      box.querySelector("#hcancel").onclick = () => (box.innerHTML = "");
      const hm = box.querySelector("#hm");
      box.querySelector("#htry").onclick = async () => {
        if (!f.reportValidity()) return;
        grab(f);
        try { const r = await api("POST", `/api/hooks/${encodeURIComponent(d.id || "try")}/test`, clean(d)); flash(hm, `${r.ok ? "Worked" : "Failed"}: ${r.detail}`, r.ok ? "ok" : "err"); }
        catch (e) { flash(hm, e.message); }
      };
      const del = box.querySelector("#hdel");
      if (del) del.onclick = async () => {
        if (!confirm(`Delete the hook ${h.name || h.id}?`)) return;
        try { await api("DELETE", `/api/hooks/${encodeURIComponent(h.id)}`); load(); } catch (e) { flash(hm, e.message); }
      };
      f.onsubmit = async (ev) => {
        ev.preventDefault();
        grab(f);
        try {
          if (h) await api("PUT", `/api/hooks/${encodeURIComponent(h.id)}`, clean(d));
          else await api("POST", "/api/hooks", clean(d));
          flash(m, "Saved.", "ok");
          load();
        } catch (e) { flash(hm, e.message); }
      };
    };
    const grab = (f) => {
      const v = (n) => (f[n] ? f[n].value.trim() : undefined);
      const json = (s) => { if (s === "" || s === undefined) return undefined; try { return JSON.parse(s); } catch (e) { return s; } };
      d.name = v("name"); d.id = v("id"); d.kind = picked(f, "kind") || d.kind;
      d.on = [...f.querySelectorAll('input[name="on"]:checked')].map((x) => x.value);
      d.enabled = f.enabled.checked;
      for (const key of ["url", "host", "username", "password", "from", "subject", "topic"]) if (f[key]) d[key] = v(key) || undefined;
      if (f.port) d.port = v("port") ? +v("port") : undefined;
      if (d.kind === "webhook") d.method = picked(f, "method") || "POST";
      if (d.kind === "email") { d.security = picked(f, "security"); d.to = (v("to") || "").split(",").map((x) => x.trim()).filter(Boolean); }
      if (d.kind === "mqtt") { d.qos = +(picked(f, "qos") || 0); d.retain = !!f.retain?.checked; }
      if (d.kind === "write") {
        const [svc, ...rest] = (v("wtag") || "").split(".");
        d.service = svc; d.tag = rest.join(".");
        d.value = json(v("value")); d.clear_value = json(v("clear_value"));
      }
      if (f.body) {
        const b = f.body.value.trim();
        if (d.kind === "webhook") d.body = json(b);
        if (d.kind === "mqtt") d.payload = json(b);
        if (d.kind === "email") d.body = b || undefined;
      }
    };
    drawForm();
    box.scrollIntoView({ behavior: "smooth", block: "start" });
  };
  load();
}

function bodyText(d, k) {
  const v = k === "mqtt" ? d.payload : d.body;
  if (v === undefined || v === null) return "";
  return typeof v === "string" ? v : JSON.stringify(v, null, 2);
}

// Only the fields that belong to the hook's kind.
function clean(d) {
  const keep = {
    webhook: ["url", "method", "headers", "body"],
    write: ["service", "tag", "value", "clear_value"],
    email: ["host", "port", "security", "username", "password", "from", "to", "subject", "body"],
    mqtt: ["host", "port", "topic", "username", "password", "qos", "retain", "payload"],
  }[d.kind];
  const out = { id: d.id, name: d.name, kind: d.kind, on: d.on, enabled: d.enabled };
  for (const k of keep) if (d[k] !== undefined && d[k] !== "") out[k] = d[k];
  return out;
}

function hookTarget(h) {
  if (h.kind === "webhook") return `${h.method || "POST"} ${h.url}`;
  if (h.kind === "write") return `${h.service}.${h.tag} ← ${JSON.stringify(h.value)}`;
  if (h.kind === "email") return `${(h.to || []).join(", ")} via ${h.host}`;
  if (h.kind === "mqtt") return `${h.host}:${h.port || 1883} ${h.topic}`;
  return "";
}

// ---------------------------------------------------------------- log tab

async function logTab(body, m) {
  const log = await api("GET", "/api/alarms/log?limit=300").catch((e) => { flash(m, e.message); return []; });
  body.innerHTML = `<div class="panel scroll"><table>
    <thead><tr><th>Time</th><th>Event</th><th>Alarm</th><th class="hide-sm">Value</th></tr></thead><tbody>
    ${log.map((e) => `<tr>
      <td class="mono muted">${esc(new Date(e.time).toLocaleString())}</td>
      <td><span class="pn-ev pn-ev-${esc(e.event)}">${esc(e.event)}</span>${e.user ? ` <span class="muted">${esc(e.user)}</span>` : ""}</td>
      <td>${sevChip(e.severity)} ${esc(e.message || e.name)}</td>
      <td class="hide-sm mono">${esc(fmtValue(e.value))}</td>
    </tr>`).join("") || `<tr><td colspan="4" class="muted">Nothing has happened yet.</td></tr>`}
    </tbody></table></div>`;
}
