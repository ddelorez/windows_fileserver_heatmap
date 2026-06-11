//! collector — heat-spike server.
//!
//! A single Linux binary that listens for NDJSON dumps from the Windows agents,
//! archives each raw body to disk, and (when `--db` is given) ingests accepted
//! dumps into an embedded DuckDB file. The raw archive is written FIRST and
//! unconditionally — it is the inspectable record and the replay source — then
//! the parsed dump is applied to the database in one transaction.
//!
//! Usage:
//!   collector [--bind <ip:port>] [--archive-dir <path>] [--db <path>]
//!     --bind <ip:port>      listen address      (default 0.0.0.0:2742)
//!     --archive-dir <path>  raw-dump archive    (default ./archive)
//!     --db <path>           DuckDB file; OMITTED => archive-only mode
//!
//! Concurrency: tiny_http's accept loop runs on the main thread and each
//! request is handled inline (sequential). Three agents on 5-minute timers are
//! near-zero concurrency, and sequential handling is the single-writer
//! discipline the database wants (the connection is a `Mutex<Connection>`).

mod archive;
mod db;
mod ingest;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tiny_http::{Method, Request, Response, Server};

use db::{Db, IngestOutcome};
use ingest::Disposition;

/// Sanity cap against a runaway client — not a tuning parameter. Bodies larger
/// than this are rejected with 400 before anything is archived.
const MAX_BODY: usize = 256 * 1024 * 1024;

const DEFAULT_BIND: &str = "0.0.0.0:2742";
const DEFAULT_ARCHIVE_DIR: &str = "./archive";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let bind = parse_bind(&args);
    let archive_dir = parse_archive_dir(&args);
    let db_path = parse_db(&args);

    // Open the database up front (when requested) so a bad path / unreadable
    // file fails fast at startup rather than on the first dump.
    let db: Option<Db> = match &db_path {
        Some(path) => match Db::open(path) {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("failed to open db {}: {e}", path.display());
                std::process::exit(1);
            }
        },
        None => None,
    };

    // Display tokens for the startup marker and per-request log lines.
    let db_disp = db_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "none".to_string());
    let engine_disp = db.as_ref().map(Db::engine_version).unwrap_or("none");
    // Per-request "db=" token is the file name only (or "none").
    let db_token = db_path
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "none".to_string());

    let server = match Server::http(&bind) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to bind {bind}: {e}");
            std::process::exit(1);
        }
    };

    // Noisy-mode startup marker: one line to stderr and one appended to
    // <archive-dir>/_startup.log (best-effort; archive writes surface dir
    // problems later if this fails).
    let ts = rfc3339_utc(unix_now().as_secs() as i64);
    if db.is_some() {
        eprintln!(
            "collector listening on {bind}, archive {}, db {db_disp} (engine {engine_disp})",
            archive_dir.display()
        );
    } else {
        eprintln!(
            "collector listening on {bind}, archive {} (archive-only, no db)",
            archive_dir.display()
        );
    }
    append_startup_log(&archive_dir, &ts, &bind, &db_disp, engine_disp);

    // Accept loop on the main thread; handle each request inline (no per-request
    // threads — see the module doc comment).
    for request in server.incoming_requests() {
        handle(request, &archive_dir, db.as_ref(), &db_token);
    }
}

/// Append one startup marker to `<archive-dir>/_startup.log`, creating the dir
/// and file as needed. Best-effort: a failure is logged to stderr, not fatal.
fn append_startup_log(archive_dir: &Path, ts: &str, bind: &str, db: &str, engine: &str) {
    if let Err(e) = std::fs::create_dir_all(archive_dir) {
        eprintln!("warning: could not create archive dir for _startup.log: {e}");
        return;
    }
    let line = format!(
        "{ts} bind={bind} archive={} db={db} engine={engine}\n",
        archive_dir.display()
    );
    let res = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(archive_dir.join("_startup.log"))
        .and_then(|mut f| f.write_all(line.as_bytes()));
    if let Err(e) = res {
        eprintln!("warning: could not write _startup.log: {e}");
    }
}

/// Handle one request end to end: route, read (capped), classify, archive,
/// (optionally) ingest into the db, log, respond. Every path logs exactly one
/// stdout line (carrying a `db=` token) and sends exactly one response.
fn handle(mut request: Request, archive_dir: &Path, db: Option<&Db>, db_token: &str) {
    let now = unix_now();
    let ts = rfc3339_utc(now.as_secs() as i64);
    let ip = request
        .remote_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|| "-".to_string());

    // ---- routing ----------------------------------------------------------
    let path = request.url().split('?').next().unwrap_or("").to_string();
    let is_ingest = *request.method() == Method::Post && path == "/ingest";
    if !is_ingest {
        log_line(&ts, &ip, "-", "-", "-", "-", 404, "no route", db_token);
        let _ = request.respond(Response::empty(404));
        return;
    }

    // ---- read body, capped at MAX_BODY (read one byte past to detect over) --
    let mut body = Vec::new();
    if let Err(e) = request
        .as_reader()
        .take(MAX_BODY as u64 + 1)
        .read_to_end(&mut body)
    {
        log_line(&ts, &ip, "-", "-", "-", "-", 400, "body read error", db_token);
        let _ = request.respond(Response::from_string(format!("read error: {e}")).with_status_code(400));
        return;
    }
    if body.len() > MAX_BODY {
        // Rejected before archiving — the archive should not hold a runaway body.
        log_line(&ts, &ip, "-", "-", "-", "-", 400, "body exceeds 256 MiB", db_token);
        let _ = request.respond(Response::from_string("body exceeds 256 MiB").with_status_code(400));
        return;
    }

    // ---- classify (pure) then archive (I/O) -------------------------------
    let disposition = ingest::classify(&body);

    let archived = match &disposition {
        Disposition::Malformed { .. } => {
            archive::write_malformed(archive_dir, now.as_millis(), &body)
        }
        Disposition::Rejected { header, .. } | Disposition::Accepted { header, .. } => {
            let rel = ingest::archive_rel_path(&header.server, &header.run_id, header.dump_seq);
            archive::write_verbatim(archive_dir, &rel, &body)
        }
    };

    // An archive write failure is an operator/disk problem, not a client error:
    // report it as 500 and keep the identity fields we managed to parse.
    if let Err(e) = archived {
        let (server, run, seq) = log_identity(&disposition);
        log_line(&ts, &ip, &server, &run, &seq, "-", 500, &format!("archive failed: {e}"), db_token);
        let _ = request.respond(Response::from_string("archive write failed").with_status_code(500));
        return;
    }

    // ---- log + respond per disposition ------------------------------------
    let (server, run, seq) = log_identity(&disposition);
    match &disposition {
        Disposition::Malformed { reason } => {
            log_line(&ts, &ip, &server, &run, &seq, "-", 400, reason, db_token);
            let _ = request.respond(Response::from_string(reason.clone()).with_status_code(400));
        }
        Disposition::Rejected { reason, .. } => {
            log_line(&ts, &ip, &server, &run, &seq, "-", 400, reason, db_token);
            let _ = request.respond(Response::from_string(reason.clone()).with_status_code(400));
        }
        Disposition::Accepted { header, rows } => {
            let n = rows.len().to_string();
            match db {
                // Archive-only mode: step-2 behavior preserved exactly.
                None => {
                    log_line(&ts, &ip, &server, &run, &seq, &n, 200, "ok", db_token);
                    let _ = request.respond(Response::empty(200));
                }
                // DB mode: ingest the parsed dump (archive already written).
                Some(db) => match db.ingest(header, rows) {
                    Ok(IngestOutcome::Accepted) => {
                        log_line(&ts, &ip, &server, &run, &seq, &n, 200, "accepted", db_token);
                        let _ = request.respond(Response::empty(200));
                    }
                    Ok(IngestOutcome::DedupeNoop) => {
                        log_line(&ts, &ip, &server, &run, &seq, &n, 200, "dedupe-noop", db_token);
                        let _ = request.respond(Response::empty(200));
                    }
                    // Body is already archived — the archive is the replay source.
                    Err(e) => {
                        log_line(&ts, &ip, &server, &run, &seq, &n, 500, &format!("db error: {e}"), db_token);
                        let _ = request.respond(Response::from_string("db ingest failed").with_status_code(500));
                    }
                },
            }
        }
    }
}

/// Pull the loggable identity (server, run_id[..8], dump_seq) from a
/// disposition. `Malformed` has no trusted identity, so all three are `"-"`.
fn log_identity(d: &Disposition) -> (String, String, String) {
    match d {
        Disposition::Malformed { .. } => ("-".into(), "-".into(), "-".into()),
        Disposition::Rejected { header, .. } | Disposition::Accepted { header, .. } => (
            header.server.clone(),
            ingest::run8(&header.run_id),
            header.dump_seq.to_string(),
        ),
    }
}

/// One stdout line per request: RFC3339 time, client IP, server, run_id (first
/// 8), dump_seq, row count, HTTP status, a short reason, and the db file in use
/// (`none` in archive-only mode). Fields use `key=` prefixes so the line is
/// greppable; `"-"` marks a value we do not have.
fn log_line(
    ts: &str,
    ip: &str,
    server: &str,
    run: &str,
    seq: &str,
    rows: &str,
    status: u16,
    reason: &str,
    db: &str,
) {
    println!(
        "{ts} ip={ip} server={server} run={run} seq={seq} rows={rows} status={status} reason={reason} db={db}"
    );
}

// ---- CLI parsing (hand-rolled, agent style) -------------------------------

/// Parse `--bind <ip:port>`; default `0.0.0.0:2742`. A missing value aborts.
fn parse_bind(args: &[String]) -> String {
    parse_value(args, "--bind")
        .map(|v| v.to_string())
        .unwrap_or_else(|| DEFAULT_BIND.to_string())
}

/// Parse `--archive-dir <path>`; default `./archive`. A missing value aborts.
fn parse_archive_dir(args: &[String]) -> PathBuf {
    parse_value(args, "--archive-dir")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_ARCHIVE_DIR))
}

/// Parse `--db <path>`; OPTIONAL. Absent => `None` => archive-only mode. A
/// present flag with no value aborts (matching the other flags' style).
fn parse_db(args: &[String]) -> Option<PathBuf> {
    parse_value(args, "--db").map(PathBuf::from)
}

/// Find `<flag> <value>` in args, returning the value. A flag with no following
/// value aborts with exit code 2, matching the agent's `--share` style.
fn parse_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let mut i = 1;
    while i < args.len() {
        if args[i] == flag {
            match args.get(i + 1) {
                Some(v) => return Some(v),
                None => {
                    eprintln!("{flag} needs a value");
                    std::process::exit(2);
                }
            }
        }
        i += 1;
    }
    None
}

// ---- time -----------------------------------------------------------------

/// Duration since the Unix epoch (saturating to 0 on a pre-epoch clock).
fn unix_now() -> std::time::Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

/// Format unix `secs` as RFC3339 UTC (`YYYY-MM-DDTHH:MM:SSZ`). Hand-rolled from
/// `SystemTime` so the crate needs no date library — the only timestamps we
/// produce are log lines and the `_malformed` filename (which uses millis).
fn rfc3339_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Days-since-Unix-epoch -> (year, month, day) via Howard Hinnant's
/// `civil_from_days` (proleptic Gregorian, valid across the full range).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_known_instants() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        // 2026-06-11T00:00:00Z (2353 days after the 2020-01-01 epoch anchor).
        assert_eq!(rfc3339_utc(1_781_136_000), "2026-06-11T00:00:00Z");
    }

    #[test]
    fn cli_defaults_and_overrides() {
        let none: Vec<String> = vec!["collector".into()];
        assert_eq!(parse_bind(&none), DEFAULT_BIND);
        assert_eq!(parse_archive_dir(&none), PathBuf::from(DEFAULT_ARCHIVE_DIR));
        assert_eq!(parse_db(&none), None); // archive-only by default

        let some: Vec<String> = vec![
            "collector".into(),
            "--bind".into(),
            "127.0.0.1:9999".into(),
            "--archive-dir".into(),
            "/srv/dumps".into(),
            "--db".into(),
            "/srv/heat.duckdb".into(),
        ];
        assert_eq!(parse_bind(&some), "127.0.0.1:9999");
        assert_eq!(parse_archive_dir(&some), PathBuf::from("/srv/dumps"));
        assert_eq!(parse_db(&some), Some(PathBuf::from("/srv/heat.duckdb")));
    }
}
