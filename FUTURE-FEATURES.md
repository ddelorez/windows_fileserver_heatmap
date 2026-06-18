# Future / Planned Features

> Forward-looking wishlist for **smb-heat-spike**. Nothing here is committed,
> staged, or ratified — these are candidate enhancements captured so the intent
> isn't lost. The tool is already functional for its immediate purpose (manual
> HDD↔SSD tiering decisions via the leaderboard / split-explorer dashboard);
> everything below is additive.
>
> Each item notes a feasibility read and, where relevant, the architectural
> constraints it must respect. The load-bearing decisions it must **not**
> disturb: the two independent crates (no Cargo workspace; the agent's frozen
> dependency tree stays frozen), the collector's single-writer
> `Mutex<Connection>` discipline, and the self-contained single-binary
> principle (UI embedded via `include_str!`; no framework, no CDN, no build
> step).

---

## 1. Unified installer — **someday/maybe**

A single installer that lets the operator choose **agent-only** or **server
module**, replacing today's hand-run deploy rituals.

> Explicitly the lowest-priority item — a "someday/maybe." The manual deploy
> path (see `DEPLOYMENT.md`) is the supported route until this exists.

### Agent installer
Mostly packaging of steps already executed by hand. The
SYSTEM / session-0 / at-boot Task Scheduler task is proven on production; an
installer just orchestrates it: place the binary at a stable path, register the
exact task definition, write the invocation. Given a small admin-operated shop
with no code-signing certificate in play, a **PowerShell installer** (optionally
wrapped in a self-extracting exe) is the most in-character path — it mirrors the
manual ritual rather than introducing MSI/WiX machinery. The share-selection UI
(item 2) is the natural front-end for this installer.

**Feasibility:** straightforward. Low architectural risk.

### Collector module — containerize
Containerizing the collector is feasible and arguably cleaner than the current
bespoke systemd deploy. It's a single binary with DuckDB bundled and the UI
embedded, so there is no asset pipeline to manage.

- The DuckDB `bundled` feature compiles a native C++ library, so a **multi-stage
  Docker build** (build *inside* the image against the target's glibc) is the
  safe call — not copying a WSL-built binary into a scratch image. A fully
  static `musl` build would avoid this but static C++ on musl is usually more
  trouble than it's worth; debian-slim multi-stage is the pragmatic choice.
- Single-writer discipline maps cleanly: **one container = one writer.** The
  only rule is never run two replicas against the same DB file.
- Restart policy, the tier-log path, the DB/archive paths, and the port all
  translate from the systemd unit to container args / mounted volumes
  one-to-one.

**Feasibility:** moderate. Makes the "install the server module" story genuinely
portable — the main win.

### Remote install over SSH (run from the Windows agent host)
Drive a collector install on an existing Linux box in the fleet by entering an
IP + SSH credentials on the Windows machine where the agent is being installed.

- Technically doable (Windows ships an OpenSSH client) but **credential handling
  is the sensitive surface.** Design posture: key-based auth as the default,
  never store a password, and a clear sudo story for the privileged target ops
  (unit install, firewall rule, service user).
- **Recommended simplification:** if the collector is containerized, make remote
  install a thin *"ssh in → ensure Docker present → `docker run` with the right
  volume / port / restart policy"* wrapper. Idempotent, far fewer moving parts
  than scripting a native install.

**Feasibility:** moderate, with the SSH-credential surface as the one real
caution. The operator performs the credential entry; the tool should never hold
or transcribe secrets in plain text.

---

## 2. Agent share selection (auto-assess + checkbox picker)

Replace the hand-typed `--share NAME=PATH` allowlist with an interactive picker:
enumerate the shares that actually exist on the server and let the operator tick
the ones to track. (Today, instrumenting a server means hand-walking its shares
— e.g. a 14-share manual walk on a production GIS server.)

### Enumeration
Windows exposes shares several ways: `Get-SmbShare` (modern; returns
Name / Path / Description; trivially filters out `$` admin shares), CIM
`Win32_Share`, or the native `NetShareEnum` — which is what the Computer
Management snap-in itself reads. The agent already pins the `windows` crate, so
native enumeration is possible without shelling out, **but** per the
no-fabrication rule the exact crate module path / struct binding is a
verify-at-build item, not an assertion. Given the agent's no-new-surface
conservatism, shelling to `Get-SmbShare` and parsing may be the more
in-character choice than expanding the windows-crate API footprint. Either works.

### Where it lives — important
This belongs in the **installer / config tool, not the resident agent.** The
agent runs headless in session 0, which cannot display UI; a checkbox picker has
to run interactively in the admin's session at config time.

There is a design that requires **zero changes to the closed agent**: the
installer enumerates shares, shows the checkboxes, and writes the chosen
`NAME=PATH` pairs straight into the scheduled task's argument string. The agent
keeps consuming `--share` exactly as it does today; the selection logic lives
entirely in the installer. This keeps the agent record genuinely closed.
(Having the agent read a config file instead is a small but real reopen of the
agent — worth an explicit decision, not a drift.)

### On the native file dialog
Steer away from a folder picker as the *primary* mechanism. The unit of tracking
is a **share** (name + path), not an arbitrary folder. A folder picker would
happily let you select a path that isn't shared — which produces nothing, since
the agent only sees `SMBServer` opens and non-shared local paths are invisible
to it by construction. Enumerate-and-checkbox is both easier and semantically
correct. Keep a file dialog, if at all, as an "advanced: add a path manually"
escape hatch.

**Feasibility:** the auto-enumerate half is easy and has a clean zero-agent-change
path. Strong candidate.

---

## 3. Dashboard / heatmap UI refinements + telemetry-display customization

Modernize the displayed heatmapping and let the operator customize which
telemetry measurements drive the view.

The data layer already supports this completely. Every metric the UI could want
(demand, span, bytes, density, pct_bytes / pct_demand, unknown-bytes count, tier
columns) is **computed at query time with nothing baked into storage**, so
re-tuning and re-mapping cost zero reprocessing. "Customization of telemetry
measurements" is therefore a pure UI-layer change:

- let the operator choose which metric drives **bar length**, which drives
  **color**, and which render as **labels**;
- surface the demand↔density toggle more prominently (it's the un-skew / carve
  signal);
- the **treemap** (area = bytes, color = density) is the headline "modernize the
  heatmap" move and is already the planned next view — very doable in vanilla
  SVG/Canvas.

### The one ceiling to respect
"Modernize" has a hard boundary set by a deliberate project decision: **no
framework, no CDN, no build step** — everything embedded via `include_str!`.
Vanilla JS + hand-rolled SVG/Canvas goes a long way (the existing bar chart and
a treemap both live comfortably there). But D3-grade interactivity or a component
kit collides with the no-build-step rule — it would mean a CDN (rejected) or
vendoring assets plus a build step (rejected), i.e. reopening the
self-contained-single-binary principle. Scope the customization ambition against
that boundary before starting, so "modernize" doesn't quietly become "add a
toolchain."

### Easy production win hiding here
The theme toggle is session-only because browser storage was unavailable in the
development preview environment — but the production dashboard is served to a
real browser, where `localStorage` works. So metric / theme / window preferences
can persist across visits at no cost. *(Inference — confirm in the production
browser context when implementing.)*

**Feasibility:** lowest-risk of the three, and partly roadmapped already.

---

## Related smaller items (already logged elsewhere)

These are pre-existing backlog/refinement items, repeated here for visibility:

- **Server/share filter case-sensitivity** — the filter matches case-sensitively
  while the agent stores `server` lowercased; typing the upper-case hostname
  returns zero rows. Fix is a one-liner (lowercase the input or compare
  case-insensitively).
- **User-home access landing under share `unknown`** — access to unwalked
  user-home paths resolves under an `unknown` share rather than a named one.
- **ufw read/write port-split** — a hardening candidate: keep the dashboard
  subnet-open but scope the no-auth `/ingest` endpoint to agent IPs only.
- **Identity filtering** — intentionally parked; revisit trigger is a remote
  service-account appearing in SMB access.
