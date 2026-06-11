//! All DuckDB interaction for the collector — the single place that touches the
//! database. The connection lives behind a `Mutex` (the single-writer rule made
//! explicit in code: the accept loop is already sequential, and the guard also
//! supplies the `&mut Connection` that `Connection::transaction` requires).
//!
//! Binding choices (verified against duckdb-rs 1.x, recorded here):
//!   * run_id — bound as the hyphenated string with `CAST(? AS UUID)`. The
//!     duckdb crate's "uuid" feature would allow a native `uuid::Uuid` binding;
//!     we deliberately do NOT enable it (keeps the dep set as audited). If the
//!     string+cast ever proves awkward, enabling that feature is the documented
//!     alternative.
//!   * emitted_at — an RFC3339 instant bound as a string with
//!     `CAST(? AS TIMESTAMP)`. DuckDB's `TIMESTAMP` is timezone-naive, so we
//!     strip a trailing *zero* UTC designator (`Z`, `+00:00`, `-00:00`) in Rust
//!     first — pure string surgery, no timezone math, valid because the agent's
//!     instants are all UTC. A NON-zero offset (e.g. `+05:00`) is rejected
//!     loudly rather than stripped (stripping would silently shift the instant).
//!     See [`strip_utc_suffix`].
//!   * file_id / day_index / reads / writes / alloc_bytes — bound as integers;
//!     see the per-statement comments for the width casts.
//!
//! NO Appender: per-row prepared statements inside one transaction per dump.

use std::path::Path;
use std::sync::Mutex;

use duckdb::{params, Connection, OptionalExt};

use crate::ingest::{DumpRow, Header};

/// Executed verbatim at startup when `--db` is present. Idempotent
/// (`IF NOT EXISTS` throughout), so it is safe to run on an existing file.
const DDL: &str = "
CREATE SEQUENCE IF NOT EXISTS file_id_seq;

CREATE TABLE IF NOT EXISTS files (
    file_id          BIGINT PRIMARY KEY DEFAULT nextval('file_id_seq'),
    server           TEXT NOT NULL,
    share            TEXT NOT NULL,
    path             TEXT NOT NULL,
    alloc_bytes      BIGINT,
    restat           BOOLEAN NOT NULL DEFAULT FALSE,
    unresolved_share BOOLEAN NOT NULL DEFAULT FALSE,
    first_seen       TIMESTAMP NOT NULL,
    last_seen        TIMESTAMP NOT NULL,
    UNIQUE (server, share, path)
);

CREATE TABLE IF NOT EXISTS runs (
    run_id        UUID PRIMARY KEY,
    server        TEXT NOT NULL,
    started_at    TIMESTAMP,
    last_dump_seq INTEGER NOT NULL,
    last_dump_at  TIMESTAMP NOT NULL
);

CREATE TABLE IF NOT EXISTS day_counts (
    file_id   BIGINT  NOT NULL,
    run_id    UUID    NOT NULL,
    day_index INTEGER NOT NULL,
    reads     INTEGER NOT NULL,
    writes    INTEGER NOT NULL,
    PRIMARY KEY (file_id, run_id, day_index)
);
";

/// What an `ingest` call resolved to, for the per-request log line.
#[derive(Debug, PartialEq, Eq)]
pub enum IngestOutcome {
    /// The dump's rows were applied (insert/update) and committed.
    Accepted,
    /// `dump_seq <= runs.last_dump_seq` — already-seen or out-of-order dump.
    /// The transaction is rolled back; the body stays archived intentionally.
    DedupeNoop,
}

/// Errors from an ingest transaction. A `Db` error means the body (already
/// archived) is the replay source; main.rs maps this to HTTP 500.
#[derive(Debug)]
pub enum IngestError {
    /// The header carried no `emitted_at`, so first_seen/last_seen/run
    /// timestamps cannot be set. (The agent always emits it.)
    MissingEmittedAt,
    /// `emitted_at` carries a non-zero timezone offset (e.g. `+05:00`). We will
    /// not strip it (that would silently shift the instant) and DuckDB's naive
    /// TIMESTAMP cannot represent it — so we reject loudly. (The agent emits
    /// UTC: `Z` or `+00:00`.)
    NonUtcOffset(String),
    Db(duckdb::Error),
}

impl From<duckdb::Error> for IngestError {
    fn from(e: duckdb::Error) -> Self {
        IngestError::Db(e)
    }
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IngestError::MissingEmittedAt => write!(f, "header missing emitted_at"),
            IngestError::NonUtcOffset(ts) => {
                write!(f, "emitted_at has a non-UTC offset: {ts}")
            }
            IngestError::Db(e) => write!(f, "{e}"),
        }
    }
}

/// Owns the DuckDB connection and the engine version string read at open time.
pub struct Db {
    conn: Mutex<Connection>,
    engine_version: String,
}

impl Db {
    /// Open (or create) the database at `path`, run the idempotent DDL, and read
    /// the engine version. The version is recorded so the operator knows the
    /// minimum standalone DuckDB CLI that can open this file (the CLI must be
    /// same-or-newer than the bundled engine).
    pub fn open(path: &Path) -> duckdb::Result<Db> {
        let conn = Connection::open(path)?;
        conn.execute_batch(DDL)?;
        let engine_version: String = conn.query_row("SELECT version()", [], |r| r.get(0))?;
        Ok(Db {
            conn: Mutex::new(conn),
            engine_version,
        })
    }

    /// The DuckDB engine version (e.g. `v1.1.3`) read at open.
    pub fn engine_version(&self) -> &str {
        &self.engine_version
    }

    /// Ingest one ACCEPTED dump in a single transaction. Called AFTER the body
    /// has been archived (archive-first order is unchanged). Sequence:
    ///   1. dedupe gate on `runs.last_dump_seq`,
    ///   2+3. per row: dimension upsert (select-then-insert/update) + fact upsert,
    ///   4. run-row upsert,
    ///   5. commit.
    pub fn ingest(&self, header: &Header, rows: &[DumpRow]) -> Result<IngestOutcome, IngestError> {
        let emitted_at = header
            .emitted_at
            .as_deref()
            .ok_or(IngestError::MissingEmittedAt)?;
        // Strip a zero UTC designator; a non-zero offset is rejected loudly.
        let ts = strip_utc_suffix(emitted_at)
            .ok_or_else(|| IngestError::NonUtcOffset(emitted_at.to_string()))?;

        // Sequential accept loop => no real contention; a poisoned lock would
        // only happen if a prior handler panicked mid-transaction, in which case
        // recovering the inner connection is the sensible move.
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction()?;

        // --- 1. dedupe gate -------------------------------------------------
        // last_dump_seq is INTEGER (i32). `None` => this run is unseen.
        let stored_seq: Option<i32> = tx
            .query_row(
                "SELECT last_dump_seq FROM runs WHERE run_id = CAST(? AS UUID)",
                params![header.run_id],
                |r| r.get(0),
            )
            .optional()?;

        if let Some(prev) = stored_seq {
            if (header.dump_seq as i32) <= prev {
                // Already applied (or older). Roll back, leave the archive be.
                tx.rollback()?;
                return Ok(IngestOutcome::DedupeNoop);
            }
        }

        // --- 2 + 3. per-row dimension + facts -------------------------------
        for row in rows {
            let file_id = upsert_dimension(&tx, &header.server, row, ts)?;

            // Facts upsert — the §14 ON CONFLICT spelling, applied verbatim.
            // reads/writes are u32 in the wire format; bound as i64 (always
            // non-negative, never truncates) and narrowed to the INTEGER column
            // by DuckDB (a pathological >2^31 count errors loudly rather than
            // wrapping). day_index is the agent's i32, bound verbatim.
            tx.execute(
                "INSERT INTO day_counts (file_id, run_id, day_index, reads, writes)
                 VALUES (?, CAST(? AS UUID), ?, ?, ?)
                 ON CONFLICT (file_id, run_id, day_index)
                 DO UPDATE SET reads = excluded.reads, writes = excluded.writes",
                params![
                    file_id,
                    header.run_id,
                    row.day,
                    row.reads as i64,
                    row.writes as i64
                ],
            )?;
        }

        // --- 4. run row -----------------------------------------------------
        // We already know existence from the dedupe probe: `None` => insert with
        // started_at; `Some` => update last_dump_seq + last_dump_at ONLY.
        // started_at is set once and never updated.
        match stored_seq {
            None => {
                tx.execute(
                    "INSERT INTO runs (run_id, server, started_at, last_dump_seq, last_dump_at)
                     VALUES (CAST(? AS UUID), ?, CAST(? AS TIMESTAMP), ?, CAST(? AS TIMESTAMP))",
                    params![header.run_id, header.server, ts, header.dump_seq as i32, ts],
                )?;
            }
            Some(_) => {
                tx.execute(
                    "UPDATE runs SET last_dump_seq = ?, last_dump_at = CAST(? AS TIMESTAMP)
                     WHERE run_id = CAST(? AS UUID)",
                    params![header.dump_seq as i32, ts, header.run_id],
                )?;
            }
        }

        // --- 5. commit ------------------------------------------------------
        tx.commit()?;
        Ok(IngestOutcome::Accepted)
    }
}

/// Dimension upsert via select-then-insert-or-update (NOT `ON CONFLICT`: `files`
/// has two uniqueness constraints — the `file_id` PK and the (server,share,path)
/// UNIQUE — and we need the `file_id` back). Returns the row's `file_id`.
fn upsert_dimension(
    tx: &duckdb::Connection,
    server: &str,
    row: &DumpRow,
    ts: &str,
) -> duckdb::Result<i64> {
    let restat = row.flags.iter().any(|f| f == "restat");
    let unresolved = row.flags.iter().any(|f| f == "unresolved_share");
    // alloc_bytes is u64 on the wire, BIGINT (i64) in the column. File sizes
    // never approach i64::MAX, so the cast is safe; `None` stays SQL NULL.
    let alloc: Option<i64> = row.alloc_bytes.map(|v| v as i64);

    let existing: Option<i64> = tx
        .query_row(
            "SELECT file_id FROM files WHERE server = ? AND share = ? AND path = ?",
            params![server, row.share, row.path],
            |r| r.get(0),
        )
        .optional()?;

    match existing {
        Some(file_id) => {
            // LITERAL last-write-wins: alloc_bytes/restat/unresolved_share are
            // overwritten with this dump's values. A restat row (alloc_bytes
            // NULL, restat=true) therefore CLOBBERS a previously-known byte
            // count — deliberate; the latest dump is authoritative. first_seen
            // is never touched; last_seen advances to this dump's emitted_at.
            tx.execute(
                "UPDATE files
                    SET alloc_bytes = ?, restat = ?, unresolved_share = ?,
                        last_seen = CAST(? AS TIMESTAMP)
                  WHERE file_id = ?",
                params![alloc, restat, unresolved, ts, file_id],
            )?;
            Ok(file_id)
        }
        None => {
            // INSERT ... RETURNING file_id — duckdb-rs returns the RETURNING row
            // straight from query_row, so no follow-up SELECT or currval() is
            // needed. (INSERT-then-SELECT on the UNIQUE key would be the fallback
            // if RETURNING were unsupported; it is supported here.)
            // first_seen == last_seen == this dump's emitted_at on first sight.
            let file_id: i64 = tx.query_row(
                "INSERT INTO files
                     (server, share, path, alloc_bytes, restat, unresolved_share,
                      first_seen, last_seen)
                 VALUES (?, ?, ?, ?, ?, ?, CAST(? AS TIMESTAMP), CAST(? AS TIMESTAMP))
                 RETURNING file_id",
                params![server, row.share, row.path, alloc, restat, unresolved, ts, ts],
                |r| r.get(0),
            )?;
            Ok(file_id)
        }
    }
}

/// Strip a trailing *zero* UTC designator from an RFC3339 instant, yielding the
/// naive UTC wall-clock that DuckDB's tz-naive `TIMESTAMP` expects. Pure string
/// surgery — NO timezone math — and a LOCAL function (not chrono's `naive_utc`;
/// collector code never touches chrono even though arrow links it).
///
/// Only `Z`, `+00:00`, and `-00:00` are stripped, because they denote UTC and
/// dropping them does not move the instant:
///   "2026-06-10T00:00:00+00:00"   -> Some("2026-06-10T00:00:00")
///   "2026-06-11T00:00:00Z"        -> Some("2026-06-11T00:00:00")
///   "2026-06-10T00:00:00.5+00:00" -> Some("2026-06-10T00:00:00.5")
///
/// A NON-zero offset is NOT stripped (that would silently shift the instant) and
/// yields `None` so the caller can reject the dump loudly:
///   "2026-06-10T00:00:00+05:00"   -> None
///
/// A bare naive string (no offset, no `Z`) is returned unchanged (the agent
/// always emits a designator, but a naive instant is unambiguous here).
fn strip_utc_suffix(ts: &str) -> Option<&str> {
    if let Some(rest) = ts.strip_suffix('Z') {
        return Some(rest);
    }
    // An offset, if present, lives in the time component (after 'T'); the date
    // component's '-' separators must not be mistaken for an offset sign.
    if let Some(tpos) = ts.find('T') {
        let time = &ts[tpos + 1..];
        if let Some(off) = time.rfind(|c| c == '+' || c == '-') {
            // Only a zero offset is safe to drop. Anything else (a real offset
            // like "+05:00", or a malformed one) is rejected.
            return match &time[off..] {
                "+00:00" | "-00:00" => Some(&ts[..tpos + 1 + off]),
                _ => None,
            };
        }
    }
    Some(ts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.duckdb")).unwrap();
        (dir, db)
    }

    const RUN: &str = "5b08749f-1234-5678-9abc-def012345678";

    fn header(seq: u32, emitted: &str) -> Header {
        Header {
            server: "sgifs01".into(),
            run_id: RUN.into(),
            dump_seq: seq,
            emitted_at: Some(emitted.into()),
        }
    }

    fn dump_row(share: &str, path: &str, day: i32, reads: u32, writes: u32) -> DumpRow {
        DumpRow {
            share: share.into(),
            path: path.into(),
            day,
            reads,
            writes,
            alloc_bytes: Some(4096),
            flags: vec![],
        }
    }

    // Read helpers ----------------------------------------------------------

    // Scalar i64 read. duckdb-rs FromSql is type-strict, so callers cast any
    // INTEGER column to BIGINT in SQL (`reads::BIGINT`) — count(*)/BIGINT
    // columns are already i64.
    fn scalar_i64(db: &Db, sql: &str) -> i64 {
        let conn = db.conn.lock().unwrap();
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    // 1. DDL idempotence: opening + running the DDL twice on the same file path.
    #[test]
    fn ddl_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.duckdb");
        let _a = Db::open(&path).unwrap();
        drop(_a);
        // Re-open the same file: DDL runs again, must not error.
        let _b = Db::open(&path).unwrap();
    }

    // Engine-version probe returns a non-empty string (recorded at startup).
    #[test]
    fn engine_version_non_empty() {
        let (_d, db) = temp_db();
        assert!(!db.engine_version().is_empty());
    }

    // 2. Accepted dump ingests; dimension row is exact.
    #[test]
    fn accepted_dump_writes_exact_dimension() {
        let (_d, db) = temp_db();
        let rows = vec![dump_row("data", "a.txt", 20613, 2, 1)];
        assert_eq!(
            db.ingest(&header(1, "2026-06-10T00:00:00+00:00"), &rows).unwrap(),
            IngestOutcome::Accepted
        );

        let conn = db.conn.lock().unwrap();
        let (server, share, path, alloc, restat, unresolved, first_seen, last_seen): (
            String,
            String,
            String,
            i64,
            bool,
            bool,
            String,
            String,
        ) = conn
            .query_row(
                "SELECT server, share, path, alloc_bytes, restat, unresolved_share,
                        CAST(first_seen AS VARCHAR), CAST(last_seen AS VARCHAR)
                   FROM files",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(server, "sgifs01");
        assert_eq!(share, "data");
        assert_eq!(path, "a.txt");
        assert_eq!(alloc, 4096);
        assert!(!restat);
        assert!(!unresolved);
        assert_eq!(first_seen, last_seen);
        // DuckDB renders the naive timestamp with a space separator.
        assert_eq!(first_seen, "2026-06-10 00:00:00");
    }

    // 3. Facts rows are exact; day_index round-trips verbatim.
    #[test]
    fn facts_rows_exact_and_day_index_verbatim() {
        let (_d, db) = temp_db();
        let rows = vec![dump_row("data", "a.txt", 20613, 7, 3)];
        db.ingest(&header(1, "2026-06-10T00:00:00+00:00"), &rows).unwrap();

        let conn = db.conn.lock().unwrap();
        let (day, reads, writes): (i32, i32, i32) = conn
            .query_row(
                "SELECT day_index, reads, writes FROM day_counts",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(day, 20613);
        assert_eq!(reads, 7);
        assert_eq!(writes, 3);
    }

    // 4. Re-ingesting the identical dump (same seq) is a dedupe no-op; nothing
    //    changes.
    #[test]
    fn reingest_same_seq_is_noop() {
        let (_d, db) = temp_db();
        let rows = vec![dump_row("data", "a.txt", 20613, 7, 3)];
        db.ingest(&header(1, "2026-06-10T00:00:00+00:00"), &rows).unwrap();

        // Second time, same seq => no-op (even with different counts in hand).
        let rows2 = vec![dump_row("data", "a.txt", 20613, 999, 999)];
        assert_eq!(
            db.ingest(&header(1, "2026-06-10T01:00:00+00:00"), &rows2).unwrap(),
            IngestOutcome::DedupeNoop
        );

        assert_eq!(scalar_i64(&db, "SELECT count(*) FROM files"), 1);
        assert_eq!(scalar_i64(&db, "SELECT count(*) FROM day_counts"), 1);
        assert_eq!(scalar_i64(&db, "SELECT reads::BIGINT FROM day_counts"), 7);
        assert_eq!(scalar_i64(&db, "SELECT last_dump_seq::BIGINT FROM runs"), 1);
    }

    // 5. A seq-2 dump with the same keys but changed counts REPLACES the facts
    //    (not summed); last_seen bumps; started_at is unchanged; seq advances.
    #[test]
    fn seq2_replaces_facts_and_bumps_last_seen() {
        let (_d, db) = temp_db();
        db.ingest(
            &header(1, "2026-06-10T00:00:00+00:00"),
            &[dump_row("data", "a.txt", 20613, 2, 0)],
        )
        .unwrap();
        db.ingest(
            &header(2, "2026-06-10T00:05:00+00:00"),
            &[dump_row("data", "a.txt", 20613, 5, 1)],
        )
        .unwrap();

        // Facts replaced, not summed.
        assert_eq!(scalar_i64(&db, "SELECT reads::BIGINT FROM day_counts"), 5);
        assert_eq!(scalar_i64(&db, "SELECT writes::BIGINT FROM day_counts"), 1);
        assert_eq!(scalar_i64(&db, "SELECT count(*) FROM day_counts"), 1);

        let conn = db.conn.lock().unwrap();
        let (started, last_dump, last_seen, seq): (String, String, String, i32) = conn
            .query_row(
                "SELECT CAST(r.started_at AS VARCHAR), CAST(r.last_dump_at AS VARCHAR),
                        CAST(f.last_seen AS VARCHAR), r.last_dump_seq
                   FROM runs r, files f",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(started, "2026-06-10 00:00:00"); // never updated
        assert_eq!(last_dump, "2026-06-10 00:05:00"); // bumped
        assert_eq!(last_seen, "2026-06-10 00:05:00"); // bumped
        assert_eq!(seq, 2);
    }

    // 6. Out-of-order: after seq 2, ingesting seq 1 is a no-op.
    #[test]
    fn out_of_order_seq1_after_seq2_is_noop() {
        let (_d, db) = temp_db();
        db.ingest(
            &header(1, "2026-06-10T00:00:00+00:00"),
            &[dump_row("data", "a.txt", 20613, 2, 0)],
        )
        .unwrap();
        db.ingest(
            &header(2, "2026-06-10T00:05:00+00:00"),
            &[dump_row("data", "a.txt", 20613, 5, 1)],
        )
        .unwrap();
        assert_eq!(
            db.ingest(
                &header(1, "2026-06-10T00:10:00+00:00"),
                &[dump_row("data", "a.txt", 20613, 100, 100)],
            )
            .unwrap(),
            IngestOutcome::DedupeNoop
        );
        assert_eq!(scalar_i64(&db, "SELECT reads::BIGINT FROM day_counts"), 5); // seq-2 value stands
        assert_eq!(scalar_i64(&db, "SELECT last_dump_seq::BIGINT FROM runs"), 2);
    }

    // 7. An unresolved-share row: share "unknown", flag set, alloc_bytes NULL.
    #[test]
    fn unresolved_share_row() {
        let (_d, db) = temp_db();
        let row = DumpRow {
            share: "unknown".into(),
            path: "mystery.dat".into(),
            day: 20613,
            reads: 1,
            writes: 0,
            alloc_bytes: None,
            flags: vec!["unresolved_share".into()],
        };
        db.ingest(&header(1, "2026-06-10T00:00:00+00:00"), &[row]).unwrap();

        let conn = db.conn.lock().unwrap();
        let (share, unresolved, alloc_is_null): (String, bool, bool) = conn
            .query_row(
                "SELECT share, unresolved_share, alloc_bytes IS NULL FROM files",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(share, "unknown");
        assert!(unresolved);
        assert!(alloc_is_null);
    }

    // 8. A restat row for a path that previously had bytes => alloc_bytes becomes
    //    NULL (the literal last-write-wins invariant).
    #[test]
    fn restat_clobbers_known_bytes_to_null() {
        let (_d, db) = temp_db();
        // seq 1: known size.
        db.ingest(
            &header(1, "2026-06-10T00:00:00+00:00"),
            &[dump_row("data", "a.txt", 20613, 1, 0)],
        )
        .unwrap();
        assert_eq!(scalar_i64(&db, "SELECT alloc_bytes FROM files"), 4096);

        // seq 2: restat, alloc unknown.
        let restat_row = DumpRow {
            share: "data".into(),
            path: "a.txt".into(),
            day: 20613,
            reads: 2,
            writes: 0,
            alloc_bytes: None,
            flags: vec!["restat".into()],
        };
        db.ingest(&header(2, "2026-06-10T00:05:00+00:00"), &[restat_row]).unwrap();

        let conn = db.conn.lock().unwrap();
        let (restat, alloc_is_null): (bool, bool) = conn
            .query_row(
                "SELECT restat, alloc_bytes IS NULL FROM files",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(restat);
        assert!(alloc_is_null); // previously-known 4096 was clobbered to NULL
    }

    // 9. UUID round-trip: stored run_id reads back string-equal to the input.
    #[test]
    fn uuid_round_trips() {
        let (_d, db) = temp_db();
        db.ingest(
            &header(1, "2026-06-10T00:00:00+00:00"),
            &[dump_row("data", "a.txt", 20613, 1, 0)],
        )
        .unwrap();
        let conn = db.conn.lock().unwrap();
        let run_id: String = conn
            .query_row("SELECT CAST(run_id AS VARCHAR) FROM runs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(run_id, RUN);
    }

    // 10. TIMESTAMP cast accepts both the "+00:00" offset and the "Z" spelling,
    //     and both denote the same instant.
    #[test]
    fn timestamp_cast_accepts_offset_and_z() {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("ts.duckdb")).unwrap();
        let from_offset: String = conn
            .query_row(
                "SELECT CAST(? AS TIMESTAMP)::VARCHAR",
                params![strip_utc_suffix("2026-06-10T00:00:00+00:00").unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        let from_z: String = conn
            .query_row(
                "SELECT CAST(? AS TIMESTAMP)::VARCHAR",
                params![strip_utc_suffix("2026-06-10T00:00:00Z").unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(from_offset, "2026-06-10 00:00:00");
        assert_eq!(from_offset, from_z);
    }

    // strip_utc_suffix unit checks (pure string surgery, local — not chrono).
    #[test]
    fn strip_utc_suffix_strips_zero_designators() {
        assert_eq!(strip_utc_suffix("2026-06-10T00:00:00+00:00"), Some("2026-06-10T00:00:00"));
        assert_eq!(strip_utc_suffix("2026-06-11T00:00:00Z"), Some("2026-06-11T00:00:00"));
        assert_eq!(strip_utc_suffix("2026-06-10T00:00:00-00:00"), Some("2026-06-10T00:00:00"));
        assert_eq!(strip_utc_suffix("2026-06-10T12:34:56.5+00:00"), Some("2026-06-10T12:34:56.5"));
        assert_eq!(strip_utc_suffix("2026-06-10T00:00:00"), Some("2026-06-10T00:00:00"));
    }

    // A non-zero offset must NOT be stripped (that would shift the instant) — it
    // returns None so the caller rejects it.
    #[test]
    fn strip_utc_suffix_rejects_nonzero_offset() {
        assert_eq!(strip_utc_suffix("2026-06-10T00:00:00+05:00"), None);
        assert_eq!(strip_utc_suffix("2026-06-10T00:00:00-08:00"), None);
    }

    // End-to-end: a dump whose emitted_at has a non-zero offset fails ingest
    // loudly (NonUtcOffset -> 500 at the HTTP layer) and writes nothing.
    #[test]
    fn ingest_rejects_nonzero_offset() {
        let (_d, db) = temp_db();
        let err = db
            .ingest(
                &header(1, "2026-06-10T00:00:00+05:00"),
                &[dump_row("data", "a.txt", 20613, 1, 0)],
            )
            .unwrap_err();
        assert!(matches!(err, IngestError::NonUtcOffset(_)));
        // Transaction never opened past the timestamp check — DB is untouched.
        assert_eq!(scalar_i64(&db, "SELECT count(*) FROM files"), 0);
        assert_eq!(scalar_i64(&db, "SELECT count(*) FROM runs"), 0);
    }

    // 11. Rows never vanish within a run: a seq-2 superset replaces overlapping
    //     facts while earlier rows remain present.
    #[test]
    fn seq2_superset_keeps_earlier_rows() {
        let (_d, db) = temp_db();
        db.ingest(
            &header(1, "2026-06-10T00:00:00+00:00"),
            &[
                dump_row("data", "a.txt", 20613, 1, 0),
                dump_row("data", "b.txt", 20613, 1, 0),
            ],
        )
        .unwrap();
        db.ingest(
            &header(2, "2026-06-10T00:05:00+00:00"),
            &[
                dump_row("data", "a.txt", 20613, 9, 0), // changed
                dump_row("data", "b.txt", 20613, 1, 0), // same
                dump_row("data", "c.txt", 20613, 1, 0), // new
            ],
        )
        .unwrap();

        assert_eq!(scalar_i64(&db, "SELECT count(*) FROM files"), 3);
        assert_eq!(scalar_i64(&db, "SELECT count(*) FROM day_counts"), 3);
        // a.txt still present, with the replaced count.
        assert_eq!(
            scalar_i64(
                &db,
                "SELECT dc.reads::BIGINT FROM day_counts dc JOIN files f USING (file_id) WHERE f.path = 'a.txt'"
            ),
            9
        );
        // b.txt (an earlier row) is still present.
        assert_eq!(
            scalar_i64(&db, "SELECT count(*) FROM files WHERE path = 'b.txt'"),
            1
        );
    }
}
