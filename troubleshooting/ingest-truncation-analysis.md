# Collector `/ingest` truncation — read-only investigation

**Date:** 2026-06-29
**Scope:** READ-ONLY. No files modified, no build, no git changes.
**Symptom:** `/ingest` returns HTTP 400 on every POST, reason
`"not a valid row/footer: EOF while parsing a string"`. Archived raw bodies are
truncated to exactly 393216 B (384 KB) or 262144 B (256 KB) — clean 64 KB
multiples — ending mid-record inside an open JSON string. Real bodies are
~26 MB / ~110,000 NDJSON lines. Last success was a 26 MB / 110,192-row POST (200).

---

## Bottom line up front

**The leading hypothesis is false.** The collector does *not* do a single
`.read()`, a `take(N)` sized to the body, or a fixed-capacity buffer. It drains
the request body correctly with `read_to_end`, capped only at a 256 MiB sanity
ceiling. The truncation is **not introduced inside the collector's read loop** —
the collector faithfully archives exactly the bytes that arrived on the socket.
The 64 KB-boundary truncation originates **upstream**, and the agent code
contains the smoking gun: a **10-second `timeout_global`** on the body upload
(see §7).

---

## 1. Collector crate + src layout

`collector/` — the `tiny_http` crate (`name = "collector"`,
`collector/Cargo.toml`). The agent crate is `agent/` (Windows/ETW, uses
`ureq`). No root `Cargo.toml`; deliberately not a workspace.

```
collector/src/
  main.rs     <- HTTP accept loop + the body read (the network I/O lives here)
  ingest.rs   <- pure classify/parse/validate (no socket, no disk)
  archive.rs  <- raw-body archive writer
  db.rs       <- DuckDB ingest
  query.rs    <- read endpoints
  ui/         <- embedded dashboard
```

## 2. Where `/ingest` reads the body — `collector/src/main.rs:184-194`

```rust
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
```

`read_to_end` loops `read()` internally until EOF — it **does** drain the
stream. The `.take(MAX_BODY+1)` is a guard limit, not the read size (see §4).

## 3. Full read -> archive -> parse flow (`handle`, main.rs:152-262)

The relevant tail, `collector/src/main.rs:202-233`:

```rust
// ---- classify (pure) then archive (I/O) -------------------------------
let disposition = ingest::classify(&body);                    // <- PARSE happens here (line 203)

let archived = match &disposition {
    Disposition::Malformed { .. } => {
        archive::write_malformed(archive_dir, now.as_millis(), &body)
    }
    Disposition::Rejected { header, .. } | Disposition::Accepted { header, .. } => {
        let rel = ingest::archive_rel_path(&header.server, &header.run_id, header.dump_seq);
        archive::write_verbatim(archive_dir, &rel, &body)     // <- ARCHIVE happens here (line 211)
    }
};
...
Disposition::Rejected { reason, .. } => {
    log_line(&ts, &ip, &server, &run, &seq, "-", 400, reason, db_token);
    let _ = request.respond(Response::from_string(reason.clone()).with_status_code(400));
}
```

**Correction to the original assumption:** the parse (`classify`, line 203)
actually happens **before** the archive write (line 211) — not after. But this
does **not** weaken the evidence. Both operate on the *same already-read `body`
Vec*, so the archived bytes are byte-identical to what was parsed. The order is
irrelevant; the buffer was already truncated when `read_to_end` returned.

The error string `"...not a valid row/footer: EOF while parsing a string"` is
emitted at `ingest.rs:222` inside `parse_body_lines`, which only runs **after**
the header (line 1) parsed and passed naming validation (`ingest.rs:98-101`).
That yields `Disposition::Rejected` (not `Malformed`), which routes the archive
to the **per-server path via `write_verbatim`** — matching the observation that
the truncated files landed under `sgi-backup/...` rather than `_malformed/`.
Fully consistent: a body truncated mid-rows, with an intact header.

## 4. Body-size cap — `collector/src/main.rs:41`

```rust
const MAX_BODY: usize = 256 * 1024 * 1024;   // 256 MiB
```

Applied at `main.rs:188` (`.take(MAX_BODY as u64 + 1)`) and checked at
`main.rs:195`. **256 MiB >> the 26 MB bodies**, so this cap is *not* the
truncation source. There is no smaller cap, no `with_capacity` sizing the body,
no length const near 256 KB/384 KB anywhere in the read path.

## 5. tiny_http version + how the body is meant to be consumed

- Pinned: `tiny_http = "0.12"` (`Cargo.toml`); locked at **0.12.0** (checksum
  `389915df...cdc82`, `Cargo.lock`). Body-reader deps: `chunked_transfer 1.5.0`,
  `ascii 1.1.0`.
- From the installed source
  (`~/.cargo/registry/.../tiny_http-0.12.0/src/request.rs:186-227`),
  `as_reader()` returns a **stream that must be drained to EOF** — a single
  `read()` returns only one buffer's worth. The reader's nature depends on
  headers:
  - **Content-Length > 1024** -> `FusedReader::new(EqualReader::new(source, content_length))`
    (request.rs:214-216). `EqualReader` returns bytes until its internal `size`
    counter (= `content_length`) reaches 0, then EOF. **If the underlying socket
    hits EOF *before* `size` reaches 0, `read` returns `Ok(0)` and `read_to_end`
    returns the partial bytes as `Ok` — a silent truncation, no error.**
    (equal_reader.rs read impl + fused_reader.rs: once inner returns 0 it fuses
    to `Ok(0)` forever.)
  - **Content-Length <= 1024** -> fully buffered up front, and a short socket
    *does* error (`ConnectionAborted`, request.rs:200-208).
  - **Transfer-Encoding: chunked** -> `FusedReader::new(Decoder::new(source))`;
    terminates at the 0-length chunk or early EOF.

So the collector's `read_to_end` is the **correct** way to consume this API. The
failure mode that produces a truncated-but-`Ok` body is exactly the
EqualReader-hits-early-EOF case above — i.e. the client/connection delivered
fewer bytes than the `Content-Length` it promised.

## 6. Is Content-Length exposed? Does the handler use it?

- **Exposed:** yes — `Request::body_length() -> Option<usize>`
  (`request.rs:279-281`, backed by the `body_length` field set from
  `content_length`).
- **Used by the handler:** **no.** `handle` never calls `request.body_length()`
  and never compares `body.len()` against it. This is the collector's one real
  defect: had it checked `body.len() == body_length`, it would have caught the
  short read and returned a `read error` / 500 instead of silently archiving a
  truncated body and rejecting it as malformed framing. (It does not exonerate
  the collector of the *truncation*, but it explains why the truncation is
  invisible until the parser trips.)

## 7. Root cause (found in the agent — explains the 64 KB boundaries)

The agent POSTs via `ureq` 3.3.0 with a body it builds as a full string and
`.send(doc)` (`agent/src/etw.rs:293-304`) — `ureq` sets a correct
`Content-Length` for the full ~26 MB. But the agent's `ureq::Agent` is
configured at `agent/src/etw.rs:195-202`:

```rust
let config = ureq::Agent::config_builder()
    .timeout_connect(Some(Duration::from_secs(10)))
    .timeout_global(Some(Duration::from_secs(10)))   // <- covers the entire request, incl. body upload
    .build();
```

`timeout_global` is a wall-clock cap on the **whole** request, body upload
included. Pushing 26 MB in under 10 s needs sustained ~2.6 MB/s; on a degraded
WAN the upload is aborted mid-stream, ureq tears down the TCP connection, and
the collector's `EqualReader` sees EOF before `Content-Length` is satisfied ->
`read_to_end` returns the partial body as `Ok`. The clean 64 KB-multiple cutoffs
(256 KB, 384 KB) are OS/TCP send-buffer flush granularity — what made it onto
the wire before the 10 s gun fired. This also explains why the last 26 MB POST
*succeeded* (200) and later ones fail: it's gated on transient link speed vs. a
fixed 10 s budget, and the body has been growing.

---

## Summary of facts (no changes made)

| Ask | Finding |
|---|---|
| Read mechanism | `as_reader().take(MAX_BODY+1).read_to_end()` — drains to EOF, **hypothesis false** |
| Archive vs parse order | parse (`classify`, L203) **before** archive (L211); same buffer, evidence still holds |
| Body cap | `MAX_BODY = 256 MiB` — not the cause |
| tiny_http | 0.12.0; body is a stream that must be drained to EOF; EqualReader truncates silently on early socket EOF |
| Content-Length | exposed via `body_length()`, **not used** by handler (missing guard) |
| Likely root cause | agent's `timeout_global(10s)` aborting the 26 MB upload mid-body |

## Candidate fix directions (not yet applied — operator decision)

1. **Agent (most likely root cause):** drop or greatly raise `timeout_global`
   for the POST, or switch to a body/idle timeout rather than a global wall-clock
   cap, so a large slow upload isn't guillotined. `agent/src/etw.rs:195-202`.
2. **Collector (defense-in-depth):** after `read_to_end`, compare `body.len()`
   against `request.body_length()` and reject short reads as a 400/500 "truncated
   upload" *before* archiving, so a partial body never masquerades as malformed
   framing. `collector/src/main.rs:184-203`.
