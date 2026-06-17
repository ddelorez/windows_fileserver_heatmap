"use strict";

// No framework, no build step, no localStorage. Plain fetch + DOM.
//
// Two data modes share the page: the leaderboard (top level) and the children
// carve (drilled into a subtree). The chart (horizontal heat bars) is the front
// door; the raw table is a secondary view toggled by the Chart/Table switch.
//
// state.parent  null => leaderboard;  {server,share,subtree} => children of it
// state.view    "chart" | "table"
// state.data    the last response (kept so a sort/view switch re-renders without
//               a refetch — sort metric and color ramp are both client-side)
const state = { parent: null, view: "chart", data: null };

const REFRESH_MS = 300000; // 300s, matches the agent flush cadence
const $ = (sel) => document.querySelector(sel);

// ---- formatting -----------------------------------------------------------

function fmtBytes(n) {
  if (n === null || n === undefined) return "—";
  const u = ["B", "K", "M", "G", "T", "P"];
  let v = Number(n), i = 0;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return (i === 0 ? v : v.toFixed(v < 10 ? 1 : 0)) + u[i];
}
function fmtNum(n) {
  if (n === null || n === undefined) return "—";
  return Number(n).toLocaleString(undefined, { maximumFractionDigits: 1 });
}
function fmtDensity(d) {
  if (d === null || d === undefined) return "—";
  const v = Number(d);
  if (v === 0) return "0";
  if (v < 0.001 || v >= 1000) return v.toExponential(2);
  return v.toPrecision(3);
}
function fmtPct(p) {
  if (p === null || p === undefined) return "—";
  return (Number(p) * 100).toFixed(1) + "%";
}
function fmtStale(s) {
  if (s === null || s === undefined) return "?";
  s = Number(s);
  if (s < 90) return s + "s";
  if (s < 5400) return Math.round(s / 60) + "m";
  if (s < 172800) return Math.round(s / 3600) + "h";
  return Math.round(s / 86400) + "d";
}
function nowClock() {
  const d = new Date();
  const p = (n) => String(n).padStart(2, "0");
  return p(d.getHours()) + ":" + p(d.getMinutes()) + ":" + p(d.getSeconds());
}
function setUpdated() { $("#updated").textContent = "updated " + nowClock(); }
function setStatus(msg) { $("#status").textContent = msg || ""; }

// ---- color ramp -----------------------------------------------------------

// Sequential green -> yellow -> red, low metric = green. t in [0,1].
function ramp(t) {
  const h = 120 * (1 - Math.max(0, Math.min(1, t)));
  return "hsl(" + h.toFixed(0) + ", 70%, 55%)";
}

// ---- controls -------------------------------------------------------------

function sortMetric() {
  return $("#controls").elements["_sort"].value; // "demand" | "density"
}

// Build the API query params from the controls (client-only `_`-prefixed
// controls are excluded). Empty text fields are omitted.
function apiParams() {
  const f = $("#controls");
  const p = {};
  for (const el of f.elements) {
    if (!el.name || el.name.startsWith("_") || el.type === "submit" || el.type === "button") continue;
    const v = el.value.trim();
    if (v !== "") p[el.name] = v;
  }
  return p;
}

// The metric value used for a row's bar length / color / sort.
function metricOf(row, metric) {
  const v = metric === "density" ? row.density : row.demand;
  return v === null || v === undefined ? 0 : Number(v);
}

// ---- bar chart rendering --------------------------------------------------

// rows: leaderboard or children rows. childMode adds pct labels. onClick(row)
// drills in. The color scale is normalized across THESE rows (min..max), recomputed
// every render, so single-digit demands still spread across the ramp.
function renderBars(rows, childMode, onClick) {
  const chart = $("#chart");
  chart.innerHTML = "";
  const metric = sortMetric();

  const sorted = rows.slice().sort((a, b) => metricOf(b, metric) - metricOf(a, metric));
  const vals = sorted.map((r) => metricOf(r, metric));
  const max = vals.length ? Math.max(...vals) : 0;
  const min = vals.length ? Math.min(...vals) : 0;
  const span = max - min;

  for (const row of sorted) {
    const v = metricOf(row, metric);
    const width = max > 0 ? (v / max) * 100 : 0;
    const t = span > 0 ? (v - min) / span : 1; // uniform set -> treat as hot

    const rowEl = document.createElement("div");
    rowEl.className = "bar-row";
    rowEl.title = row.subtree +
      "  demand=" + fmtNum(row.demand) +
      "  density=" + fmtDensity(row.density) +
      "  span=" + row.span +
      "  bytes=" + fmtBytes(row.bytes) +
      (row.unknown_bytes_files ? "  (" + row.unknown_bytes_files + " unknown-bytes files)" : "");
    rowEl.addEventListener("click", () => onClick(row));

    const track = document.createElement("div");
    track.className = "bar-track";

    const fill = document.createElement("div");
    fill.className = "bar-fill";
    fill.style.width = width + "%";
    fill.style.background = ramp(t);
    track.appendChild(fill);

    const name = document.createElement("span");
    name.className = "bar-name";
    name.textContent = row.subtree;
    track.appendChild(name);

    const meta = document.createElement("span");
    meta.className = "bar-meta";
    let metaHtml = '<span class="dim">d ' + fmtNum(row.demand) + "</span>" +
      '<span class="dim">s ' + row.span + "</span>";
    if (childMode) {
      metaHtml = '<span class="dim">' + fmtPct(row.pct_demand) + " dmd</span>" + metaHtml;
    }
    metaHtml += '<span class="bytes">' + fmtBytes(row.bytes) + "</span>";
    meta.innerHTML = metaHtml;
    track.appendChild(meta);

    rowEl.appendChild(track);
    chart.appendChild(rowEl);
  }
}

// ---- table rendering (secondary) ------------------------------------------

const LEADERBOARD_COLS = [
  { label: "server", get: (r) => r.server },
  { label: "subtree", get: (r) => r.subtree },
  { label: "bytes", get: (r) => fmtBytes(r.bytes), num: true },
  { label: "demand", get: (r) => fmtNum(r.demand), num: true },
  { label: "density", get: (r) => fmtDensity(r.density), num: true },
  { label: "span", get: (r) => r.span, num: true },
  { label: "tier", get: (r) => r.tier || "—", cls: "tier" },
  { label: "note", get: (r) => r.note || "", cls: "tier" },
  { label: "?bytes", get: (r) => r.unknown_bytes_files, num: true },
];

const CHILDREN_COLS = [
  { label: "subtree", get: (r) => r.subtree },
  { label: "bytes", get: (r) => fmtBytes(r.bytes), num: true },
  { label: "%bytes", get: (r) => fmtPct(r.pct_bytes), num: true },
  { label: "demand", get: (r) => fmtNum(r.demand), num: true },
  { label: "%demand", get: (r) => fmtPct(r.pct_demand), num: true },
  { label: "density", get: (r) => fmtDensity(r.density), num: true },
  { label: "span", get: (r) => r.span, num: true },
  { label: "?bytes", get: (r) => r.unknown_bytes_files, num: true },
];

function renderTable(cols, rows, onRowClick) {
  const thead = $("#table thead");
  const tbody = $("#table tbody");
  thead.innerHTML = "";
  tbody.innerHTML = "";

  const tr = document.createElement("tr");
  for (const c of cols) {
    const th = document.createElement("th");
    th.textContent = c.label;
    tr.appendChild(th);
  }
  thead.appendChild(tr);

  for (const row of rows) {
    const r = document.createElement("tr");
    r.className = "clickable";
    r.addEventListener("click", () => onRowClick(row));
    for (const c of cols) {
      const td = document.createElement("td");
      td.textContent = c.get(row);
      if (c.num) td.className = "num";
      if (c.cls) td.className = (td.className + " " + c.cls).trim();
      r.appendChild(td);
    }
    tbody.appendChild(r);
  }
}

// ---- shared render (chart vs table off the same cached data) ---------------

function render() {
  if (!state.data) return;
  const chartEl = $("#chart");
  const tableEl = $("#table");
  const isChart = state.view === "chart";
  chartEl.classList.toggle("hidden", !isChart);
  tableEl.classList.toggle("hidden", isChart);

  if (state.parent) {
    const rows = state.data.rows;
    const onClick = (row) => drillInto(state.parent.server, state.parent.share, row.subtree);
    if (isChart) renderBars(rows, true, onClick);
    else renderTable(CHILDREN_COLS, rows, onClick);
  } else {
    const rows = state.data.rows;
    const onClick = (row) => drillInto(row.server, row.share, row.subtree);
    if (isChart) renderBars(rows, false, onClick);
    else renderTable(LEADERBOARD_COLS, rows, onClick);
  }
}

// ---- health strip ---------------------------------------------------------

async function loadHealth() {
  try {
    const resp = await fetch("/api/health");
    if (!resp.ok) throw new Error("health " + resp.status);
    const data = await resp.json();
    const strip = $("#health");
    strip.innerHTML = "";
    if (!data.servers.length) { strip.textContent = "no servers yet"; return; }
    for (const s of data.servers) {
      const stale = s.seconds_stale;
      let cls = "green";
      if (stale >= 3600) cls = "red";
      else if (stale >= 600) cls = "amber";
      const pill = document.createElement("span");
      pill.className = "health-pill";
      pill.title = s.last_dump_at + "  (seq " + s.last_dump_seq + ")";
      pill.innerHTML = '<span class="dot ' + cls + '"></span>' + s.server + " " + fmtStale(stale);
      strip.appendChild(pill);
    }
  } catch (e) {
    $("#health").textContent = "health error: " + e.message;
  }
}

// ---- data loaders ---------------------------------------------------------

async function loadLeaderboard() {
  setStatus("loading…");
  try {
    const resp = await fetch("/api/leaderboard?" + new URLSearchParams(apiParams()));
    const data = await resp.json();
    if (!resp.ok) throw new Error(data.error || ("leaderboard " + resp.status));
    state.parent = null;
    state.data = data;
    renderCrumbs();
    $("#meta").textContent = "anchor_day=" + data.anchor_day +
      "  rows=" + data.rows.length +
      "  (window=" + data.knobs.window + " depth=" + data.knobs.depth +
      " min_span=" + data.knobs.min_span +
      " w_read=" + data.knobs.w_read + " w_write=" + data.knobs.w_write + ")";
    render();
    setStatus(data.rows.length ? "" : "no subtrees match — try a wider window or lower min_span (Advanced)");
    setUpdated();
  } catch (e) {
    setStatus("error: " + e.message);
  }
}

async function loadChildren() {
  setStatus("loading…");
  try {
    const params = Object.assign(apiParams(), {
      parent: state.parent.subtree,
      server: state.parent.server,
      share: state.parent.share,
    });
    const resp = await fetch("/api/children?" + new URLSearchParams(params));
    const data = await resp.json();
    if (!resp.ok) throw new Error(data.error || ("children " + resp.status));
    state.data = data;
    renderCrumbs();
    $("#meta").textContent = "parent=" + data.parent +
      "  parent_bytes=" + fmtBytes(data.parent_totals.bytes) +
      "  parent_demand=" + fmtNum(data.parent_totals.demand) +
      "  children=" + data.rows.length;
    render();
    setStatus(data.rows.length ? "" : "no children under this subtree");
    setUpdated();
  } catch (e) {
    setStatus("error: " + e.message);
  }
}

// Re-fetch whichever view is active (used by Apply, control changes, auto-refresh).
function reload() {
  if (state.parent) loadChildren(); else loadLeaderboard();
}

// ---- navigation -----------------------------------------------------------

function drillInto(server, share, subtree) {
  state.parent = { server, share, subtree };
  loadChildren();
}

function goLeaderboard() {
  state.parent = null;
  loadLeaderboard();
}

function renderCrumbs() {
  const nav = $("#crumbs");
  nav.innerHTML = "";
  const root = document.createElement("a");
  root.textContent = "leaderboard";
  root.addEventListener("click", goLeaderboard);
  nav.appendChild(root);

  if (!state.parent) return;

  // One crumb per backslash segment of the parent subtree, each walking up to
  // that depth.
  const segs = state.parent.subtree.split("\\");
  const acc = [];
  segs.forEach((seg, i) => {
    acc.push(seg);
    const sep = document.createElement("span");
    sep.className = "sep";
    sep.textContent = "›";
    nav.appendChild(sep);
    if (i === segs.length - 1) {
      const cur = document.createElement("span");
      cur.textContent = seg;
      nav.appendChild(cur);
    } else {
      const target = acc.join("\\");
      const a = document.createElement("a");
      a.textContent = seg;
      a.addEventListener("click", () => drillInto(state.parent.server, state.parent.share, target));
      nav.appendChild(a);
    }
  });

  const scope = [];
  if (state.parent.server) scope.push(state.parent.server);
  if (state.parent.share) scope.push(state.parent.share);
  if (scope.length) {
    const tag = document.createElement("span");
    tag.className = "sep scope";
    tag.textContent = "  [" + scope.join(" / ") + "]";
    nav.appendChild(tag);
  }
}

// ---- wiring ---------------------------------------------------------------

$("#controls").addEventListener("submit", (e) => { e.preventDefault(); reload(); });

// Window / Granularity re-query immediately (the two friendly controls).
for (const name of ["window", "depth"]) {
  $("#controls").elements[name].addEventListener("change", reload);
}
// Sort metric is client-side: re-render the cached data, no refetch.
$("#controls").elements["_sort"].addEventListener("change", render);

// Chart / Table toggle: pure re-render off cached data.
$("#view-toggle").addEventListener("click", (e) => {
  const btn = e.target.closest("button[data-view]");
  if (!btn) return;
  state.view = btn.dataset.view;
  for (const b of $("#view-toggle").querySelectorAll("button")) {
    b.classList.toggle("active", b === btn);
  }
  render();
});

loadHealth();
goLeaderboard();
setInterval(loadHealth, 60000);
setInterval(reload, REFRESH_MS);
