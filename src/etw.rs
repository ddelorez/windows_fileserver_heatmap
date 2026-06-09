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
use std::sync::{Arc, Mutex};

use ferrisetw::parser::Parser;
use ferrisetw::provider::Provider;
use ferrisetw::schema_locator::SchemaLocator;
use ferrisetw::trace::{TraceTrait, UserTrace};
use ferrisetw::EventRecord;

use crate::correlation::CorrelationEngine;
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
) -> Result<(), Box<dyn std::error::Error>> {
    let no_inventory = walked_shares.is_empty();
    let mut engine = CorrelationEngine::default();
    engine.load_inventory(inventory, walked_shares);
    let engine = Arc::new(Mutex::new(engine));
    let engine_cb = engine.clone();

    if matches!(mode, Mode::Resolve) && no_inventory {
        eprintln!("no inventory loaded; join skipped");
    }

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
                    // Periodic full resolved-table dump. Ctrl+C on a busy server
                    // is unreliable (and we can't reach a final flush after an
                    // abrupt kill), so we keep a recent authoritative tally in
                    // the stream — names resolved at dump time, so late 600s have
                    // already named earlier opens.
                    if eng.opens_total % 1000 == 0 && eng.opens_total > 0 {
                        print!("{}", eng.resolved_table());
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

    // Final flush on a graceful stop (trace end, or `logman stop SmbHeatSpike
    // -ets`). An abrupt Ctrl+C kill won't reach here — the periodic dumps in the
    // callback cover that case.
    if let Mode::Resolve = mode {
        let eng = engine.lock().unwrap();
        print!("{}", eng.resolved_table());
    }

    status.map_err(|e| format!("ETW trace processing failed: {e:?}"))?;

    Ok(())
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
