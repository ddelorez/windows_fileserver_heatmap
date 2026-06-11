//! ETW plumbing. This is the one file whose exact calls depend on the
//! ferrisetw version pinned in Cargo.toml — if the project doesn't build, start
//! here and reconcile against that version's docs/examples. The shape (open a
//! real-time UserTrace on the SMBServer GUID with a keyword mask, parse each
//! record, route by event id) is stable across versions; method names have
//! drifted (`start` / `start_and_process` / `process_from_handle`).
//!
//! Note on channels: a real-time provider session enables the provider directly
//! by GUID + keyword, so you do NOT need to enable the Analytic *log* channel in
//! wevtutil to receive events 500/550/552/600/650. If `discover` shows no events
//! at all, that assumption is the first thing to check (and try setting the
//! trace level to Informational explicitly — see below).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ferrisetw::parser::Parser;
use ferrisetw::provider::Provider;
use ferrisetw::schema_locator::SchemaLocator;
use ferrisetw::trace::{TraceTrait, UserTrace};
use ferrisetw::EventRecord;

use crate::correlation::CorrelationEngine;
use crate::emit;
use crate::events::{self, DISCOVER_TARGETS};
use crate::inventory::Inventory;

pub const SMBSERVER_GUID: &str = "D48CE617-33A2-4BC3-A5C7-11AA4F29619E";
const SESSION_NAME: &str = "SmbHeatSpike";

#[derive(Clone, Copy)]
pub enum Mode {
    Discover,
    Resolve,
}

pub fn run(
    mode: Mode,
    mask: u64,
    inventory: Inventory,
    walked_shares: HashSet<String>,
    flush_secs: u64,
    emit_dir: Option<PathBuf>,
    collector: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let no_inventory = walked_shares.is_empty();
    let mut engine = CorrelationEngine::default();
    engine.load_inventory(inventory, walked_shares);
    let engine = Arc::new(Mutex::new(engine));
    let engine_cb = engine.clone();

    if matches!(mode, Mode::Resolve) && no_inventory {
        eprintln!("no inventory loaded; join skipped");
    }

    // Only Resolve mode produces a heat-table dump, so only it runs a flusher.
    // The flusher owns ALL flush output — the periodic console dump moves OFF the
    // event-callback thread to here (no serialization/printing inside the callback
    // anymore; that was the A1 hazard) — plus the flush-secs timer, the dump_seq,
    // and (Step 2) the NDJSON emit. run_id/server are minted ONCE here at trace
    // start so every dump in the run shares one identity. The callback only signals
    // FlushNow when the 1000-open counter trips; main signals Stop on graceful
    // shutdown and joins the flusher BEFORE returning, so `logman stop` can't race
    // past the final flush.
    let (flush_tx, flusher) = if matches!(mode, Mode::Resolve) {
        let (tx, rx) = mpsc::channel::<FlushMsg>();
        let ctx = FlushCtx {
            engine: engine.clone(),
            server: server_name(),
            run_id: mint_run_id(),
            walked_shares: engine.lock().unwrap().walked_shares_vec(),
            emit_dir,
            collector: collector.map(Collector::new),
            dump_seq: 1,
        };
        let handle = thread::spawn(move || run_flusher(rx, ctx, flush_secs));
        (Some(tx), Some(handle))
    } else {
        (None, None)
    };
    let cb_tx = flush_tx.clone();

    let callback = move |record: &EventRecord, locator: &SchemaLocator| {
        // Resolving the schema is what lets the Parser name-address fields.
        let schema = match locator.event_schema(record) {
            Ok(s) => s,
            Err(_) => return,
        };
        let parser = Parser::create(record, &schema);
        let id = record.event_id();

        match mode {
            Mode::Discover => discover_dump(id, &parser),
            Mode::Resolve => {
                if let Some(ev) = events::parse_event(id, &parser) {
                    // FILETIME (100ns since 1601 UTC); the engine maps it to a
                    // America/Chicago civil day for per-day demand counts.
                    let event_filetime = record.raw_timestamp();
                    let mut eng = engine_cb.lock().unwrap();
                    if let Some(access) = eng.apply(&ev, event_filetime) {
                        println!("{access}");
                    }
                    // Lightweight heartbeat so you can watch progress.
                    if eng.opens_total % 200 == 0 && eng.opens_total > 0 {
                        eprintln!("  [{}]", eng.stats_line());
                    }
                    // Periodic full resolved-table dump trigger. Ctrl+C on a busy
                    // server is unreliable (and we can't reach a final flush after
                    // an abrupt kill), so we keep a recent authoritative tally in
                    // the stream. The dump itself runs on the flusher thread: we
                    // compute the trip while holding the lock, RELEASE it, and only
                    // then signal — no serialization/printing happens inline here.
                    let trip = eng.opens_total % 1000 == 0 && eng.opens_total > 0;
                    drop(eng);
                    if trip {
                        if let Some(tx) = &cb_tx {
                            let _ = tx.send(FlushMsg::FlushNow);
                        }
                    }
                }
            }
        }
    };

    let provider = Provider::by_guid(SMBSERVER_GUID)
        .add_callback(callback)
        .any(mask)
        // .level(...) // Defaults are usually fine. If no events arrive, set the
        //             // trace level to Informational (4) — these events are level 4.
        .build();

    let (_trace, handle) = UserTrace::new()
        .named(SESSION_NAME.to_string())
        .enable(provider)
        .start()
        .map_err(|e| format!("failed to start ETW trace: {e:?}"))?;
    // VERIFY (ferrisetw API): depending on version this blocking call may be
    // `UserTrace::process_from_handle(handle)?;` (as below) or the trace may
    // process via `start_and_process()` / `trace.process()`. Adjust to match.
    let status = UserTrace::process_from_handle(handle);

    // Graceful stop (trace end, or `logman stop SmbHeatSpike -ets`): tell the
    // flusher to do its FINAL flush, then JOIN it before we return. The join is
    // load-bearing — without it process exit could race past the last dump, which
    // is the tail-loss the graceful-stop flush exists to prevent. An abrupt Ctrl+C
    // kill won't reach here; the periodic dumps cover that case.
    if let (Some(tx), Some(handle)) = (flush_tx, flusher) {
        let _ = tx.send(FlushMsg::Stop);
        let _ = handle.join();
    }

    status.map_err(|e| format!("ETW trace processing failed: {e:?}"))?;

    Ok(())
}

/// Messages from the event-callback thread (and main) to the flusher thread.
enum FlushMsg {
    /// The 1000-open counter tripped in the event callback.
    FlushNow,
    /// Graceful shutdown: do one final flush, then exit the loop.
    Stop,
}

/// All flusher-thread-owned state: the engine handle, the run-scoped emit identity
/// (`server`/`run_id`/`walked_shares`), the emit sink config, and the monotonic
/// `dump_seq`.
struct FlushCtx {
    engine: Arc<Mutex<CorrelationEngine>>,
    server: String,
    run_id: String,
    /// Walked-share allowlist (lowercased), for the NDJSON header.
    walked_shares: Vec<String>,
    /// `--emit-dir`: write each dump as `<run_id[..8]>-<seq>.ndjson` here.
    emit_dir: Option<PathBuf>,
    /// `--collector`: POST each dump's NDJSON here. Agent built once.
    collector: Option<Collector>,
    /// Starts at 1, increments on EVERY flush — even a failed write/POST — so seq
    /// gaps in the emitted/received stream are a signal, not silent loss.
    dump_seq: u32,
}

/// A configured collector endpoint: a reusable ureq agent + the target URL. TLS
/// is compiled out (plain HTTP only); a ~10 s connect and ~10 s overall timeout
/// are set so a stuck collector can't wedge the flusher indefinitely.
struct Collector {
    agent: ureq::Agent,
    url: String,
}

impl Collector {
    fn new(url: String) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_global(Some(Duration::from_secs(10)))
            .build();
        Collector { agent: ureq::Agent::new_with_config(config), url }
    }
}

/// The flusher thread. Owns the flush-secs timer and ALL flush output. Wakes on
/// whichever comes first — a FlushNow/Stop message or the timer deadline — flushes,
/// and resets the deadline. Stop (or a disconnected channel) does a final flush
/// and exits, so main's join returns only after the tail has been emitted.
fn run_flusher(rx: Receiver<FlushMsg>, mut ctx: FlushCtx, flush_secs: u64) {
    // If emitting to files, make sure the directory exists once up front rather
    // than failing (and logging) on every flush.
    if let Some(dir) = &ctx.emit_dir {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("emit: cannot create --emit-dir {}: {e}", dir.display());
        }
    }

    let interval = Duration::from_secs(flush_secs);
    let mut next = Instant::now() + interval;
    loop {
        let timeout = next.saturating_duration_since(Instant::now());
        match rx.recv_timeout(timeout) {
            Ok(FlushMsg::FlushNow) | Err(RecvTimeoutError::Timeout) => {
                ctx.flush();
                next = Instant::now() + interval;
            }
            Ok(FlushMsg::Stop) | Err(RecvTimeoutError::Disconnected) => {
                ctx.flush();
                break;
            }
        }
    }
}

impl FlushCtx {
    /// One flush. Snapshot under the lock (console string + emit rows from the
    /// SAME snapshot), RELEASE the lock, then write output with no lock held. The
    /// console dump content/format is unchanged; NDJSON is built only when a sink
    /// wants it. `dump_seq` always advances afterward.
    fn flush(&mut self) {
        // Only pay for NDJSON when a sink consumes it (--emit-dir and/or --collector).
        let want_ndjson = self.emit_dir.is_some() || self.collector.is_some();
        // Stamp emitted_at outside the lock so the lock hold stays minimal.
        let emitted_at = if want_ndjson { rfc3339_now() } else { String::new() };

        let (console, ndjson) = {
            let eng = self.engine.lock().unwrap();
            let console = eng.resolved_table();
            let ndjson = want_ndjson.then(|| {
                let rows = eng.emit_rows();
                let meta = emit::Meta {
                    server: &self.server,
                    run_id: &self.run_id,
                    dump_seq: self.dump_seq,
                    emitted_at: &emitted_at,
                    walked_shares: &self.walked_shares,
                };
                emit::document(&meta, &rows)
            });
            (console, ndjson)
        };

        // The console dump remains the primary live-verification surface. Both the
        // file write and the POST run with NO lock held, on the SAME `doc`.
        print!("{console}");
        if let Some(doc) = ndjson {
            self.write_emit_dir(&doc);
            self.post_collector(&doc);
        }

        // Increment unconditionally — a failed write/POST above must not stall the
        // seq, so gaps stay meaningful.
        self.dump_seq += 1;
    }

    /// Write one dump to `<emit_dir>/<run_id[..8]>-<seq>.ndjson`. The filename
    /// stem is the first 8 hex of the run_id (the first hyphen sits at index 8, so
    /// the stem never includes it); the header still carries the full run_id.
    fn write_emit_dir(&self, doc: &str) {
        let Some(dir) = &self.emit_dir else { return };
        let stem = &self.run_id[..8];
        let path = dir.join(format!("{stem}-{}.ndjson", self.dump_seq));
        if let Err(e) = std::fs::write(&path, doc) {
            eprintln!("emit: failed to write {}: {e}", path.display());
        }
    }

    /// POST one dump's NDJSON to the collector, if configured. ureq treats non-2xx
    /// as an error by default, so any failure — connect, timeout, or HTTP status —
    /// lands in one log line carrying this dump's seq; then we drop it and carry on
    /// (no retry, no spool). Success logs one terse line to stderr so stdout (the
    /// console dump) stays clean.
    fn post_collector(&self, doc: &str) {
        let Some(c) = &self.collector else { return };
        match c
            .agent
            .post(c.url.as_str())
            .header("Content-Type", "application/x-ndjson")
            .send(doc)
        {
            Ok(resp) => eprintln!("emit: POST dump_seq={} -> {}", self.dump_seq, resp.status().as_u16()),
            Err(e) => eprintln!("emit: POST dump_seq={} failed: {e}", self.dump_seq),
        }
    }
}

/// Mint the run_id once at trace start via the RPC `UuidCreate`, formatted as a
/// hyphenated lowercase UUID by hand from the GUID fields. `UuidCreate` returns
/// `RPC_S_OK` or the non-fatal `RPC_S_UUID_LOCAL_ONLY` (still a usable, locally
/// unique value), so the status is intentionally ignored.
fn mint_run_id() -> String {
    let mut g = windows::core::GUID::default();
    // SAFETY: UuidCreate only writes a UUID into the pointed-to GUID.
    unsafe {
        let _ = windows::Win32::System::Rpc::UuidCreate(&mut g);
    }
    let d = g.data4;
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        g.data1, g.data2, g.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7],
    )
}

/// The reporting server name: `%COMPUTERNAME%` lowercased (no API call). Empty if
/// the variable is somehow unset.
fn server_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default().to_lowercase()
}

/// `emitted_at` for an NDJSON header: wall-clock now as RFC3339 UTC. SystemTime ->
/// duration since UNIX_EPOCH -> chrono `from_timestamp` -> `to_rfc3339` (no `clock`
/// feature, so no iana-time-zone). Pre-epoch clocks fall back to the epoch.
fn rfc3339_now() -> String {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    chrono::DateTime::from_timestamp(since.as_secs() as i64, since.subsec_nanos())
        .unwrap_or_default()
        .to_rfc3339()
}

/// Discover mode: print one line per record, prefixed by the event id, so that
/// related twins land on adjacent lines and shared keys (e.g. a 600's and a
/// 650's TreeConnectGUID/ShareGUID) can be eyeballed against each other.
///
/// 600/650 carry the correlation-critical fields, so those get an explicit,
/// ordered field set; every other target event falls back to a generic sweep
/// of whichever known properties parse. Names/types mirror the confirmed schema
/// in events.rs, reusing its parse + GUID-formatting helpers.
fn discover_dump(id: u16, parser: &Parser) {
    if !DISCOVER_TARGETS.contains(&id) {
        return;
    }

    let fields: Vec<String> = match id {
        // Smb2FileOpen — the access pulse; print the open identity, its tree/
        // share linkage, the path, and the access mask.
        events::E_OPEN => vec![
            field_str("Name", parser, events::P_FILE_NAME),
            field_guid("OpenGUID", parser, events::P_OPEN_GUID),
            field_guid("ShareGUID", parser, events::P_SHARE_GUID),
            field_guid("TreeConnectGUID", parser, events::P_TREE_GUID),
            field_access("DesiredAccess", parser, events::P_DESIRED_ACCESS),
        ],
        // Smb2TreeConnectAllocate — print the share name and the same two GUIDs
        // a 650 carries, so the two records line up.
        events::E_TREE_ALLOC => vec![
            field_str("ShareName", parser, events::P_SHARE_NAME),
            field_guid("ShareGUID", parser, events::P_SHARE_GUID),
            field_guid("TreeConnectGUID", parser, events::P_TREE_GUID),
        ],
        // Every other target event: generic sweep of whichever known property
        // parses on this record.
        _ => vec![
            field_guid("ConnectionGUID", parser, events::P_CONN_GUID),
            field_guid("SessionGUID", parser, events::P_SESSION_GUID),
            field_guid("TreeConnectGUID", parser, events::P_TREE_GUID),
            field_guid("ShareGUID", parser, events::P_SHARE_GUID),
            field_guid("OpenGUID", parser, events::P_OPEN_GUID),
            field_str("UserName", parser, events::P_USER_NAME),
            field_str("DomainName", parser, events::P_DOMAIN_NAME),
            field_str("ShareName", parser, events::P_SHARE_NAME),
            field_str("ScopeName", parser, events::P_SCOPE_NAME),
            field_str("Name", parser, events::P_FILE_NAME),
            field_str("TransportName", parser, events::P_TRANSPORT),
            field_addr("Address", parser, events::P_ADDRESS),
            field_access("DesiredAccess", parser, events::P_DESIRED_ACCESS),
        ],
    }
    .into_iter()
    .flatten()
    .collect();

    println!("{id:>3}  {}", fields.join("  "));
}

fn field_guid(label: &str, parser: &Parser, names: &[&str]) -> Option<String> {
    events::first_guid(parser, names).map(|k| format!("{label}={}", events::fmt_guid_key(k)))
}

fn field_str(label: &str, parser: &Parser, names: &[&str]) -> Option<String> {
    events::first_str(parser, names).map(|v| format!("{label}={v}"))
}

fn field_addr(label: &str, parser: &Parser, names: &[&str]) -> Option<String> {
    events::first_socket_addr(parser, names).map(|v| format!("{label}={v}"))
}

fn field_access(label: &str, parser: &Parser, names: &[&str]) -> Option<String> {
    events::first_u64(parser, names).map(|v| format!("{label}=0x{:08X}", v as u32))
}
