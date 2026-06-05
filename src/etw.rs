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

use std::sync::{Arc, Mutex};

use ferrisetw::parser::Parser;
use ferrisetw::provider::Provider;
use ferrisetw::schema_locator::SchemaLocator;
use ferrisetw::trace::{TraceTrait, UserTrace};
use ferrisetw::EventRecord;

use crate::correlation::CorrelationEngine;
use crate::events::{self, DISCOVER_TARGETS};

pub const SMBSERVER_GUID: &str = "D48CE617-33A2-4BC3-A5C7-11AA4F29619E";
const SESSION_NAME: &str = "SmbHeatSpike";

#[derive(Clone, Copy)]
pub enum Mode {
    Discover,
    Resolve,
}

pub fn run(mode: Mode, mask: u64) -> Result<(), Box<dyn std::error::Error>> {
    let engine = Arc::new(Mutex::new(CorrelationEngine::default()));
    let engine_cb = engine.clone();

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
                    let mut eng = engine_cb.lock().unwrap();
                    if let Some(access) = eng.apply(&ev) {
                        println!("{access}");
                    }
                    // Periodic heartbeat so you can watch the resolve ratio.
                    if eng.opens_total % 200 == 0 && eng.opens_total > 0 {
                        eprintln!("  [{}]", eng.stats_line());
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
    UserTrace::process_from_handle(handle)
        .map_err(|e| format!("ETW trace processing failed: {e:?}"))?;

    Ok(())
}

/// Discover mode: for each target event, print whichever candidate property
/// name actually parsed and its value. This is the authoritative way to learn
/// the real field names — trim the arrays in events.rs to what shows up here.
/// (Cross-check against PerfView's event view if a field never appears.)
fn discover_dump(id: u16, parser: &Parser) {
    if !DISCOVER_TARGETS.contains(&id) {
        return;
    }
    println!("--- event {id} ---");
    dump_u64(parser, "conn_id", events::P_CONN_ID);
    dump_u64(parser, "session_id", events::P_SESSION_ID);
    dump_u64(parser, "tree_id", events::P_TREE_ID);
    dump_str(parser, "user", events::P_USER_NAME);
    dump_str(parser, "user_sid", events::P_USER_SID);
    dump_str(parser, "share", events::P_SHARE_NAME);
    dump_str(parser, "file", events::P_FILE_NAME);
    dump_str(parser, "client", events::P_CLIENT_ADDR);
}

fn dump_u64(parser: &Parser, label: &str, names: &[&str]) {
    for n in names {
        if let Ok(v) = parser.try_parse::<u64>(n) {
            println!("  {label:<11} <- {n} = {v} (u64)");
            return;
        }
        if let Ok(v) = parser.try_parse::<u32>(n) {
            println!("  {label:<11} <- {n} = {v} (u32)");
            return;
        }
    }
}

fn dump_str(parser: &Parser, label: &str, names: &[&str]) {
    for n in names {
        if let Ok(v) = parser.try_parse::<String>(n) {
            println!("  {label:<11} <- {n} = {v}");
            return;
        }
    }
}
