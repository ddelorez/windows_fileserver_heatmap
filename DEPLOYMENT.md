# Deployment Guide

How to build and deploy **smb-heat-spike** from scratch: one Linux **collector**
(stores telemetry, serves the dashboard) and one or more Windows **agents** (one
per file server you want to measure).

> **About this guide.** It mirrors a working production deployment. Where a value
> comes from that deployment's operational record rather than from reading the
> binary's source, it's flagged. **Before trusting any flag in this guide, run
> the binary with `--help` and confirm the flag names and defaults for your
> build.** The architecture intentionally has no auth and no retry — read the
> Security Model section before exposing anything.

---

## Replace these tokens before running anything

This guide uses non-bracket placeholder tokens (angle brackets break PowerShell
parsing). Substitute your own values everywhere they appear:

| Token | Meaning | Example |
|---|---|---|
| `COLLECTOR_HOST` | Hostname (or IP) of the Linux collector | `heatmap01` |
| `LAN_SUBNET` | CIDR of the trusted subnet allowed to reach the collector | `10.0.0.0/24` |
| `SHARE_NAME` | A share's display name (becomes the dashboard label) | `Engineering` |
| `SHARE_PATH` | The share's local root path on the file server | `E:\Data\Engineering` |
| `LINUX_USER` | A non-root login on the collector host for file transfer | `admin` |

---

## Architecture in one paragraph

Each Windows agent consumes the `Microsoft-Windows-SMBServer` ETW provider,
rolls file opens into sparse per-day read/write counts, and POSTs cumulative
NDJSON snapshots to the collector over plain HTTP on a timed flush. The collector
is a single Rust binary: it accepts `/ingest`, stores per-day access counts in an
embedded DuckDB file (single writer, behind one mutex), and serves an embedded
dashboard. Dropped POSTs are harmless by construction — each snapshot supersedes
the last within a run. Placement decisions stay human: the tool ranks
candidates, you decide.

```
  file server A (agent) ─┐
  file server B (agent) ─┼─ POST /ingest (NDJSON, plain HTTP) ─► collector ─► DuckDB
  file server C (agent) ─┘                                          │
                                                                    └─► dashboard (browser)
```

---

## Prerequisites

**Collector (Linux):**
- A Rust toolchain (`rustup` → stable `cargo`).
- A C/C++ build toolchain. The DuckDB dependency is compiled with the `bundled`
  feature, which builds a native C++ library — on Debian/Ubuntu install
  `build-essential` (and `cmake` if the build asks for it).
- `systemd` and `ufw` on the target host (or your distro's equivalents).
- A small VM is plenty. Row volume is on the order of 1–2M rows/year at
  small-shop activity.

**Agent (Windows file server):**
- Built with the Rust **MSVC** toolchain on a Windows build machine. Do **not**
  install a toolchain on the production file server — build elsewhere, copy the
  exe over (same rule as the collector).
- Administrative rights on the target file server (ETW consumption + registering
  a SYSTEM scheduled task).

---

## Part A — Build the binaries

### Collector (on a Linux build host / WSL2)
```
cargo build --release
```
The release binary is produced under `target/release/`. Because DuckDB is
bundled, the first build is slow (it compiles the native library). Confirm the
binary's accepted flags before deploying:
```
./target/release/COLLECTOR_BINARY --help
```

### Agent (on a Windows build machine, MSVC toolchain)
```
cargo build --release
```
This yields `smb-heat-spike.exe` under `target\release\`. Confirm its flags:
```
.\target\release\smb-heat-spike.exe --help
```

> **No-toolchain-on-target rule:** build on a dedicated build machine, copy the
> resulting binary to the production host. Never compile on a production file
> server.

---

## Part B — Deploy the collector (Linux)

The collector listens on **TCP 2742** and exposes:
`POST /ingest`, `GET /api/leaderboard`, `GET /api/children`, `GET /api/health`,
and the dashboard at `GET /` (+ `/app.js`, `/style.css`). Everything else is 404.

### 1. Place the binary and create its data directory
Copy the release binary to the host, then install it to a stable path and create
the directory it will write to:
```
scp target/release/COLLECTOR_BINARY LINUX_USER@COLLECTOR_HOST:/tmp/collector-new
```
On the collector host:
```
sudo install -o root -g root -m 755 /tmp/collector-new /opt/heatmap/collector
sudo mkdir -p /var/lib/heatmap
```
The collector creates/uses a DuckDB file and a dump archive under
`/var/lib/heatmap` (the DB file and `archive/` directory). The optional
`--tier-log` flag points at a CSV used to annotate current tier / last-migration
in the dashboard; the file may be absent (tier columns simply render empty).

### 2. Create the systemd unit
This is a **template** — adapt the `ExecStart` flags to match what
`--help` reports for your build. Running as a dedicated unprivileged user is the
recommended posture (shown below); whatever user you pick must own
`/var/lib/heatmap`.

```ini
# /etc/systemd/system/heatmap-collector.service
[Unit]
Description=smb-heat-spike collector + dashboard
After=network-online.target
Wants=network-online.target

[Service]
# Recommended: a dedicated service account that owns /var/lib/heatmap
# sudo useradd --system --home /var/lib/heatmap --shell /usr/sbin/nologin heatmap
# sudo chown -R heatmap:heatmap /var/lib/heatmap
User=heatmap
Group=heatmap
ExecStart=/opt/heatmap/collector --tier-log /var/lib/heatmap/tier-log.csv
Restart=always
RestartSec=2

[Install]
WantedBy=multi-user.target
```

Enable and start it:
```
sudo systemctl daemon-reload
sudo systemctl enable --now heatmap-collector.service
```

### 3. Proofread the startup line
**Always read the listening line before trusting downstream state.** It echoes
the resolved paths and engine version — confirm they match expectation:
```
sudo journalctl -u heatmap-collector.service -n 20 --no-pager
```
It should report listening on `0.0.0.0:2742` and echo the archive path, the DB
path, the DuckDB engine version, and the tier-log path. If any path is wrong,
stop and fix it before deploying an agent.

### 4. Firewall — the load-bearing control
The ingest endpoint has **no authentication** and shares the port with the
dashboard. The firewall rule is the compensating control that makes the no-auth
design acceptable; it is **not optional**. Scope the port to your trusted subnet:
```
sudo ufw allow from LAN_SUBNET to any port 2742 proto tcp
```
For a tighter posture, scope to specific agent IPs instead of a whole subnet.
Read the Security Model section below before choosing.

### Updating the collector later (binary-swap ritual)
DuckDB uses single-writer discipline, so swap while stopped:
```
scp target/release/COLLECTOR_BINARY LINUX_USER@COLLECTOR_HOST:/tmp/collector-new
sudo systemctl stop heatmap-collector.service
sudo install -o root -g root -m 755 /tmp/collector-new /opt/heatmap/collector
sudo systemctl start heatmap-collector.service
# then proofread the startup line again
```
For ad-hoc DB inspection, use the stop → `duckdb -readonly` → query → start
sequence (or query a file copy), never a second concurrent writer.

---

## Part C — Deploy an agent (Windows file server)

Repeat per file server. The agent must run as **SYSTEM in session 0** so it
survives RDP sign-out — an interactively launched agent is torn down when the
admin signs out. This is the single most important agent deployment detail.

### 1. Place the binary at a stable path
Copy `smb-heat-spike.exe` to a stable location (not Downloads):
```
C:\HeatMapAgent\smb-heat-spike.exe
```

### 2. Identify the shares to track
The agent takes an explicit allowlist of `NAME=PATH` pairs (it does not
auto-discover shares). List the server's shares to choose from:
```
Get-SmbShare | Where-Object { $_.Name -notlike '*$' } | Format-Table Name, Path
```
Each pair becomes one `--share "SHARE_NAME=SHARE_PATH"` argument. Paths with
spaces are fine — the double-quoted form survives.

### 3. Register the SYSTEM / at-boot scheduled task
The settings below are the proven configuration. Note the **no execution time
limit** — a default limit would self-terminate the long-running agent after a
few days. Add one `--share` per share inside the `-Argument` string.

```powershell
$action = New-ScheduledTaskAction -Execute 'C:\HeatMapAgent\smb-heat-spike.exe' `
  -Argument 'resolve --share "SHARE_NAME=SHARE_PATH" --collector http://COLLECTOR_HOST:2742/ingest'

$trigger = New-ScheduledTaskTrigger -AtStartup

$principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' `
  -LogonType ServiceAccount -RunLevel Highest

$settings = New-ScheduledTaskSettingsSet `
  -ExecutionTimeLimit (New-TimeSpan -Seconds 0) `
  -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)

Register-ScheduledTask -TaskName 'SmbHeatSpikeAgent' `
  -Action $action -Trigger $trigger -Principal $principal -Settings $settings
```

To track several shares, repeat the `--share "..."` token inside `-Argument`:
```
'resolve --share "SHARE_NAME=SHARE_PATH" --share "OTHER_NAME=OTHER_PATH" --collector http://COLLECTOR_HOST:2742/ingest'
```

Other agent flags worth knowing (confirm via `--help`): `--flush-secs` sets the
push interval (default 300s); `--emit-dir` additionally writes the NDJSON dumps
to disk, useful for validating a first run.

### 4. Start it manually and verify session 0 before trusting the boot trigger
```powershell
Start-ScheduledTask -TaskName 'SmbHeatSpikeAgent'
Get-Process smb-heat-spike -IncludeUserName | Format-List Name, Id, SessionId, UserName
```
The success signature is **`SessionId : 0`** and **`UserName : NT AUTHORITY\SYSTEM`**.
If you see `SessionId : 1`, it's running interactively and will die on sign-out —
fix the principal before relying on it.

### 5. Confirm a dump actually lands
Within one flush interval, the collector health endpoint should show this agent.
From the collector (or any allowed host):
```
curl http://COLLECTOR_HOST:2742/api/health
```
Then open the dashboard — the host should appear as a green pill. Only after a
real dump lands should you trust the at-boot trigger (a reboot is the final
confirmation).

---

## Part D — Verify end to end

1. Browse to `http://COLLECTOR_HOST:2742/` from a host inside `LAN_SUBNET`.
2. Each running agent shows as a pill with a "last seen" age near the top.
3. The leaderboard ranks subtrees by demand. Each row shows:
   - **`d`** — *demand*, the heat score over the window
     (`w_read·reads + w_write·writes`, summed across active days);
   - **`s`** — *span*, the count of distinct calendar days that subtree saw
     activity (the burst-vs-sustained discriminator);
   - the **bold** value — *bytes*, the subtree's on-disk size (a `—` means the
     byte total is unknown for that subtree).
4. Controls: Window (30/60/90 days), Granularity (top-level folders vs one level
   deeper), Chart/Table, and an Advanced panel (sort metric, `w_read`/`w_write`,
   `min_span`, server/share filter, row limit). Knob changes re-rank instantly —
   nothing is reprocessed.

---

## Security Model — read before exposing anything

This system is built for a small, fully-trusted LAN and makes deliberate
trade-offs that you are accepting when you deploy it:

- **No authentication on any endpoint, plain HTTP.** `/ingest` accepts NDJSON
  from anyone who can reach the port. The **firewall rule scoping port 2742 is
  the load-bearing compensating control** — without it, anyone on the network
  can write telemetry or read the dashboard.
- **Opening the dashboard opens ingest.** They share one port. A subnet-wide
  `ufw allow` exposes the write endpoint to the whole subnet. If write exposure
  matters in your environment, scope the rule to specific agent IPs, or run the
  dashboard behind a reverse proxy that adds auth (planned hardening:
  read/write port-split).
- **The dashboard displays folder/path names** (no file contents, and no user
  identity — both are dropped by agent design). Path names can still reveal
  organizational structure (HR/finance/user folders). Confirm that audience is
  acceptable before widening access.
- **No transport encryption.** TLS is compiled out of the agent's HTTP client.
  Telemetry crosses the network in clear text. Acceptable on a trusted LAN;
  reconsider for anything routed beyond it.

If your environment is larger or less trusted than a small admin-only shop, treat
auth, TLS, and ingest IP-scoping as prerequisites rather than future hardening.

---

## Known limitations

- **Server/share filter is case-sensitive** while the agent stores the server
  name lowercased — filtering by the upper-case hostname returns zero rows. Use
  the lowercase form until fixed.
- **Access to unwalked user-home paths resolves under a share named `unknown`**
  rather than a named share.
- **No retry / no spool by design.** A failed POST is dropped; the next flush
  carries everything (snapshots are cumulative). Don't add a fallback spool.

---

## Quick reference

| | Value |
|---|---|
| Collector port | `2742` |
| Collector binary path | `/opt/heatmap/collector` |
| Collector data dir | `/var/lib/heatmap` (DB file + `archive/`) |
| systemd unit | `heatmap-collector.service` |
| Agent binary path | `C:\HeatMapAgent\smb-heat-spike.exe` |
| Agent scheduled task | `SmbHeatSpikeAgent` (SYSTEM, at-startup, no time limit) |
| Agent success signature | `SessionId : 0` + `NT AUTHORITY\SYSTEM` |
| Default flush interval | 300s (`--flush-secs`) |
| Firewall rule | `ufw allow from LAN_SUBNET to any port 2742 proto tcp` |

> Flag names and exact defaults: confirm with `--help` on your build before
> relying on them.
