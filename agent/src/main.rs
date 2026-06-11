//! smb-heat-spike — correlation-engine spike.
//!
//! Usage:
//!   smb-heat-spike discover            # dump real ETW field names for target events
//!   smb-heat-spike resolve             # print resolved (user, share, path) per open
//!   smb-heat-spike resolve --rundown   # also enumerate pre-existing sessions/trees
//!   smb-heat-spike resolve --share NAME=ROOT [--share ...]
//!                                      # walk each ROOT and join file sizes at emit
//!   smb-heat-spike resolve --flush-secs <secs>
//!                                      # periodic flush cadence (default 300)
//!   smb-heat-spike resolve --emit-dir <path>
//!                                      # write each flush as NDJSON into <path>
//!   smb-heat-spike resolve --collector <url>
//!                                      # POST each flush's NDJSON to <url> (plain HTTP)
//!   smb-heat-spike walk <Share> <dir>  # standalone file-size inventory of <dir>
//!
//! The ETW modes must run elevated (a real-time ETW session requires admin); the
//! `walk` mode does not.

mod correlation;
mod emit;
mod etw;
mod events;
mod identity;
mod inventory;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use etw::Mode;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Standalone inventory walker — deliberately NOT wired into the ETW/resolve
    // path. Builds an in-memory inventory of a local tree and prints a summary.
    if args.get(1).map(String::as_str) == Some("walk") {
        let (Some(share), Some(root)) = (args.get(2), args.get(3)) else {
            eprintln!("usage: smb-heat-spike walk <ShareName> <local_root>");
            std::process::exit(2);
        };
        match inventory::walk(share, Path::new(root)) {
            Ok(w) => print!("{}", w.summary()),
            Err(e) => {
                eprintln!("walk failed: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let mode = match args.get(1).map(String::as_str) {
        Some("discover") => Mode::Discover,
        Some("resolve") | None => Mode::Resolve,
        Some(other) => {
            eprintln!("unknown mode '{other}'. use: discover | resolve [--rundown]");
            std::process::exit(2);
        }
    };

    let mut mask = events::MASK_CORRELATION;
    if args.iter().any(|a| a == "--rundown") {
        // Pull in Rundown enumeration so the engine isn't blind to sessions and
        // trees that existed before the trace started. Recommended on a busy
        // server like SGIFS01 where long-lived sessions dominate.
        mask |= events::KW_RUNDOWN;
    }

    // Build the startup inventory from explicit --share Name=Root pairs (resolve
    // mode only). No pairs -> empty inventory -> the engine skips the join.
    let (inventory, walked_shares) = match mode {
        Mode::Resolve => build_inventory(&args),
        Mode::Discover => (inventory::Inventory::new(), HashSet::new()),
    };

    let flush_secs = parse_flush_secs(&args);
    let emit_dir = parse_emit_dir(&args);
    let collector = parse_collector(&args);

    if let Err(e) = etw::run(mode, mask, inventory, walked_shares, flush_secs, emit_dir, collector) {
        eprintln!("error: {e}");
        eprintln!();
        eprintln!("if this is a session/ETW error, a previous run's session may be lingering:");
        eprintln!("  logman query -ets | findstr SmbHeatSpike");
        eprintln!("  logman stop SmbHeatSpike -ets");
        std::process::exit(1);
    }
}

/// Parse `--collector <url>` — endpoint to POST each flush's NDJSON to. Absent ->
/// `None` (no POST). Plain HTTP only (TLS is compiled out). A missing value
/// aborts, matching `--share`'s style.
fn parse_collector(args: &[String]) -> Option<String> {
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--collector" {
            let Some(url) = args.get(i + 1) else {
                eprintln!("--collector needs a URL");
                std::process::exit(2);
            };
            return Some(url.clone());
        }
        i += 1;
    }
    None
}

/// Parse `--emit-dir <path>` — directory for per-flush NDJSON files
/// (`<run_id[..8]>-<seq>.ndjson`). Absent -> `None` (console-only). A missing
/// value aborts, matching `--share`'s style.
fn parse_emit_dir(args: &[String]) -> Option<PathBuf> {
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--emit-dir" {
            let Some(path) = args.get(i + 1) else {
                eprintln!("--emit-dir needs a directory path");
                std::process::exit(2);
            };
            return Some(PathBuf::from(path));
        }
        i += 1;
    }
    None
}

/// Parse `--flush-secs <secs>` — the periodic flush cadence in seconds; default
/// 300. Only meaningful in resolve mode (the flusher runs there); harmless
/// elsewhere. A missing value or non-integer aborts, matching `--share`'s style.
fn parse_flush_secs(args: &[String]) -> u64 {
    const DEFAULT_FLUSH_SECS: u64 = 300;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--flush-secs" {
            let Some(v) = args.get(i + 1) else {
                eprintln!("--flush-secs needs a value in seconds");
                std::process::exit(2);
            };
            match v.parse::<u64>() {
                Ok(secs) => return secs,
                Err(_) => {
                    eprintln!("--flush-secs expects a non-negative integer, got '{v}'");
                    std::process::exit(2);
                }
            }
        }
        i += 1;
    }
    DEFAULT_FLUSH_SECS
}

/// Walk each `--share NAME=ROOT` pair (explicit allowlist; no Get-SmbShare) and
/// merge into one inventory. ShareName is lowercased for both the inventory keys
/// and the walked-share set, so the emit-time join is case-insensitive. A walk
/// error aborts (the inventory is a precondition for the join).
fn build_inventory(args: &[String]) -> (inventory::Inventory, HashSet<String>) {
    let mut inv = inventory::Inventory::new();
    let mut walked: HashSet<String> = HashSet::new();

    let mut i = 1;
    while i < args.len() {
        if args[i] != "--share" {
            i += 1;
            continue;
        }
        let Some(pair) = args.get(i + 1) else {
            eprintln!("--share needs a NAME=ROOT pair");
            std::process::exit(2);
        };
        let Some((name, root)) = pair.split_once('=') else {
            eprintln!("--share expects NAME=ROOT, got '{pair}'");
            std::process::exit(2);
        };
        let name_lc = name.to_lowercase();
        match inventory::walk(&name_lc, Path::new(root)) {
            Ok(w) => {
                inv.extend(w.entries);
                walked.insert(name_lc);
            }
            Err(e) => {
                eprintln!("walk failed for {name}={root}: {e}");
                std::process::exit(1);
            }
        }
        i += 2;
    }

    (inv, walked)
}
