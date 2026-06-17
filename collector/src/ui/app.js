"use strict";

// No framework, no build step, no localStorage. Plain fetch + DOM. Two views
// share one table: the leaderboard (top level) and the children carve (drilled
// into a subtree). `state.parent` null => leaderboard; non-null => children.
const state = { parent: null };

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

// ---- query knobs ----------------------------------------------------------

function readKnobs() {
  const f = $("#knobs");
  const k = {};
  for (const el of f.elements) {
    if (!el.name) continue;
    const v = el.value.trim();
    if (v !== "") k[el.name] = v;
  }
  return k;
}

function qs(extra) {
  const k = Object.assign(readKnobs(), extra || {});
  return new URLSearchParams(k).toString();
}

function setStatus(msg) { $("#status").textContent = msg || ""; }

// ---- table rendering ------------------------------------------------------

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
    if (onRowClick) {
      r.className = "clickable";
      r.addEventListener("click", () => onRowClick(row));
    }
    for (const c of cols) {
      const td = document.createElement("td");
      const val = c.get(row);
      td.textContent = val;
      if (c.num) td.className = "num";
      if (c.cls) td.className = (td.className + " " + c.cls).trim();
      r.appendChild(td);
    }
    tbody.appendChild(r);
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
      pill.innerHTML = '<span class="dot ' + cls + '"></span>' +
        s.server + " " + fmtStale(stale);
      strip.appendChild(pill);
    }
  } catch (e) {
    $("#health").textContent = "health error: " + e.message;
  }
}

// ---- leaderboard view -----------------------------------------------------

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

async function loadLeaderboard() {
  setStatus("loading…");
  try {
    const resp = await fetch("/api/leaderboard?" + qs());
    const data = await resp.json();
    if (!resp.ok) throw new Error(data.error || ("leaderboard " + resp.status));
    renderCrumbs();
    $("#meta").textContent = "anchor_day=" + data.anchor_day +
      "  rows=" + data.rows.length +
      "  (w_read=" + data.knobs.w_read + " w_write=" + data.knobs.w_write +
      " window=" + data.knobs.window + " min_span=" + data.knobs.min_span +
      " depth=" + data.knobs.depth + ")";
    renderTable(LEADERBOARD_COLS, data.rows, (row) => drillInto(row.server, row.share, row.subtree));
    setStatus(data.rows.length ? "" : "no subtrees match (try lowering min_span or widening window)");
  } catch (e) {
    setStatus("error: " + e.message);
  }
}

// ---- children view --------------------------------------------------------

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

async function loadChildren() {
  setStatus("loading…");
  try {
    const resp = await fetch("/api/children?" + qs({ parent: state.parent.subtree,
      server: state.parent.server, share: state.parent.share }));
    const data = await resp.json();
    if (!resp.ok) throw new Error(data.error || ("children " + resp.status));
    renderCrumbs();
    $("#meta").textContent = "parent=" + data.parent +
      "  parent_bytes=" + fmtBytes(data.parent_totals.bytes) +
      "  parent_demand=" + fmtNum(data.parent_totals.demand) +
      "  children=" + data.rows.length;
    renderTable(CHILDREN_COLS, data.rows, (row) => {
      // Drill one level deeper: the child subtree becomes the new parent.
      drillInto(state.parent.server, state.parent.share, row.subtree);
    });
    setStatus(data.rows.length ? "" : "no children under this subtree");
  } catch (e) {
    setStatus("error: " + e.message);
  }
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

  const scope = [];
  if (state.parent.server) scope.push(state.parent.server);
  if (state.parent.share) scope.push(state.parent.share);

  // Build a crumb per backslash segment of the current parent subtree, each
  // walking back up to that depth.
  const segs = state.parent.subtree.split("\\");
  let acc = [];
  segs.forEach((seg, i) => {
    acc.push(seg);
    const sep = document.createElement("span");
    sep.className = "sep";
    sep.textContent = "›";
    nav.appendChild(sep);
    const target = acc.join("\\");
    if (i === segs.length - 1) {
      const cur = document.createElement("span");
      cur.textContent = seg;
      nav.appendChild(cur);
    } else {
      const a = document.createElement("a");
      a.textContent = seg;
      a.addEventListener("click", () =>
        drillInto(state.parent.server, state.parent.share, target));
      nav.appendChild(a);
    }
  });
  if (scope.length) {
    const tag = document.createElement("span");
    tag.className = "sep";
    tag.textContent = "  [" + scope.join(" / ") + "]";
    nav.appendChild(tag);
  }
}

// ---- wiring ---------------------------------------------------------------

$("#knobs").addEventListener("submit", (e) => {
  e.preventDefault();
  // Re-query the current view with the new knobs (stay where you are).
  if (state.parent) loadChildren(); else loadLeaderboard();
});

loadHealth();
goLeaderboard();
setInterval(loadHealth, 60000);
