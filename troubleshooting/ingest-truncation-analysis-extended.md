# Collector concurrency & DB-locking model — head-of-line backpressure analysis

**Date:** 2026-06-29
**Scope:** READ-ONLY. Collector crate only (`agent/` ignored). No edits, no build, no git.
**Question:** Does a slow large-body ingest block other agents' uploads (head-of-line backpressure)?
**Companion doc:** `ingest-truncation-analysis.md` (the `/ingest` truncation root-cause).

---

## Bottom line

The collector is **fully serial** — a single-threaded accept loop, one request
handled to completion before the next is dequeued. There is no thread-per-request
and no worker pool. Because of this, a slow large-body ingest **does** create
head-of-line backpressure: while one agent's 26 MB body is being read (or its
110k-row transaction is committing), every other agent's request waits in the
kernel accept queue. The `Mutex<Connection>` is almost irrelevant to that
backpressure — the serialization happens one level up, at the accept loop,
before the lock is ever reached.

---

## 1. Serve/accept loop — serial, no threads — `collector/src/main.rs:111-116`

```rust
// Accept loop on the main thread; handle each request inline (no per-request
// threads — see the module doc comment).
for request in server.incoming_requests() {
    handle(request, &archive_dir, db.as_ref(), &db_token, tier_log.as_deref());
}
```

`handle` runs **inline on the main thread**. The only other relevant line is the
server construction, `main.rs:86`: `let server = match Server::http(&bind)`. A
repo-wide grep of `collector/src/` for `spawn`, `thread::`, `ThreadPool`,
`num_threads` returns **nothing** — no thread spawning anywhere. The design is
documented deliberately at `main.rs:15-18`: *"tiny_http's accept loop runs on the
main thread and each request is handled inline (sequential)."*

> Note: `incoming_requests()` only yields the next request once the previous
> `handle` returns. So requests serialize at the *accept* boundary — independent
> of the DB lock.

## 2. DB connection handle — `Mutex<Connection>`, shared — `collector/src/db.rs:110-114`

```rust
/// Owns the DuckDB connection and the engine version string read at open time.
pub struct Db {
    conn: Mutex<Connection>,
    engine_version: String,
}
```

A single connection behind a `Mutex`, shared across all requests (one `Db` is
created at startup and passed by reference into every `handle`). Read endpoints
(`query.rs`) go through the same mutex via `Db::lock()` (`db.rs:141-143`) — no
second connection, no read-only attach.

## 3. Lock scope in the /ingest path — lock is **only around the DB transaction**, NOT the read/classify/archive

In the handler, the only DB touch is the `db.ingest(...)` call at `main.rs:244`,
which happens **after** body read (L185-194), classify/parse (L203), and archive
(L205-213). The handler holds no lock during any of those earlier steps. The lock
is acquired *inside* `ingest`, `collector/src/db.rs:163-164`:

```rust
let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
let tx = conn.transaction()?;
```

...and released when the `MutexGuard` (`conn`) drops at the end of `ingest` —
i.e. after `tx.commit()` at `db.rs:230`. So the lock spans **only the
transaction** (dedupe probe -> per-row upserts -> run-row -> commit), not the body
read, parse, or archive write.

**Caveat for the backpressure question:** the lock scope is narrow, but it
doesn't buy any concurrency. Since the accept loop (§1) is serial, only one
`handle` is ever running at a time, so the mutex is effectively uncontended — the
comment at `db.rs:160-162` says exactly this. The real serialization point is the
accept loop, not the lock.

## 4. How ~110k rows are written per dump — per-row prepared statements, 2+ statements per row, one transaction — `collector/src/db.rs:184-206`

No Appender (stated explicitly at `db.rs:22`: *"NO Appender: per-row prepared
statements inside one transaction per dump"*). The per-row loop:

```rust
// --- 2 + 3. per-row dimension + facts -------------------------------
for row in rows {
    let file_id = upsert_dimension(&tx, &header.server, row, ts)?;

    tx.execute(
        "INSERT INTO day_counts (file_id, run_id, day_index, reads, writes)
         VALUES (?, CAST(? AS UUID), ?, ?, ?)
         ON CONFLICT (file_id, run_id, day_index)
         DO UPDATE SET reads = excluded.reads, writes = excluded.writes",
        params![ file_id, header.run_id, row.day, row.reads as i64, row.writes as i64 ],
    )?;
}
```

Each row also runs `upsert_dimension` (`db.rs:238-292`), which is itself a
**SELECT then an INSERT...RETURNING or an UPDATE**:

```rust
let existing: Option<i64> = tx
    .query_row(
        "SELECT file_id FROM files WHERE server = ? AND share = ? AND path = ?",
        params![server, row.share, row.path],
        |r| r.get(0),
    )
    .optional()?;

match existing {
    Some(file_id) => { tx.execute("UPDATE files SET ... WHERE file_id = ?", ...)?; Ok(file_id) }
    None => { let file_id: i64 = tx.query_row("INSERT INTO files ... RETURNING file_id", ...)?; Ok(file_id) }
}
```

So per dump row the writer issues **2 round-trips** (1 SELECT + 1 INSERT/UPDATE on
`files`) **plus 1** (the `day_counts` upsert) = **~3 statements/row**, none
batched, all on a single connection inside one transaction. For 110k rows that's
on the order of **~330k individually-executed statements** committed atomically at
the end. The entire run holds the writer (and, because of §1, the whole server)
for the full duration of that loop + commit. Statements are built ad-hoc via
`tx.execute`/`tx.query_row` each iteration (not a hoisted `prepare`d statement
reused across rows), so there's also per-iteration prepare overhead.

## 5. Socket read/write timeouts — none, anywhere

- **Collector:** sets none. It calls `Server::http(&bind)` (`main.rs:86`) with no
  config; there is no `ServerConfig`, no `set_read_timeout`/`set_write_timeout`
  call in `collector/src/` (grep-confirmed).
- **tiny_http 0.12.0 itself:** never calls `set_read_timeout`/`set_write_timeout`
  on accepted sockets (grep of the installed crate source finds no such call).
  Its only timeout handling is *reactive*, not preventive — if a socket read
  happens to return `ErrorKind::TimedOut`, it replies 408 and closes
  (`client.rs:205-214`, the "request timeout" arm) — but since no timeout is ever
  *set* on the socket, that arm is effectively dead under default config. Accepted
  sockets block **indefinitely**.

**Implication:** a slow client mid-upload will hold the single main thread
indefinitely (no read deadline to break it), and every other agent blocks behind
it in the accept backlog. Combined with §1, that's an unbounded head-of-line
stall, not just a slow-ingest delay.

---

## Head-of-line backpressure verdict

| Factor | Finding | Effect on backpressure |
|---|---|---|
| Accept loop | serial, main-thread, inline `handle` (`main.rs:113`) | **Primary bottleneck** — one request at a time, period |
| DB lock scope | narrow (transaction only, `db.rs:163-230`) | minimal — but moot, since accept loop already serializes |
| Ingest cost | ~3 unbatched statements x rows, one txn (`db.rs:185-206`) | a 110k-row dump holds the *whole server* for the full loop+commit |
| Socket timeout | none (collector or tiny_http) | a slow/stalled upload blocks **all** agents indefinitely |

So: yes — a slow large-body ingest blocks other agents' uploads. The blocking is
at the accept loop (a slow body read stalls everything) and again during the long
single-threaded transaction (a slow commit stalls everything). The
`Mutex<Connection>` is not the cause; it's a consequence-free guard given the
serial design.

## Mitigation directions (not applied — operator decision)

1. **Bound the blast radius of a slow upload:** set a read timeout on accepted
   sockets so a stalled client can't wedge the single thread indefinitely
   (tiny_http leaves sockets with no deadline; see §5).
2. **Stop letting one big ingest freeze the server:** either move request
   handling onto a small worker pool / thread-per-request (tiny_http supports
   draining `incoming_requests()` from multiple threads), or decouple ingest from
   accept (read+archive fast, hand the parsed dump to a background DB writer).
3. **Shorten how long the writer is held per dump:** batch the `day_counts`
   writes (Appender or multi-row INSERT) and hoist the per-row `files` lookups,
   to cut the ~330k-statement transaction down dramatically (§4).
