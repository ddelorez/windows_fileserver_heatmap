//! Read-side query endpoints: leaderboard, children (split-explorer), health.
//!
//! Every read goes through `Db::lock()` — the SAME `Mutex<Connection>` ingest
//! uses (no second connection, no readonly attach). Lock, prepare, execute,
//! serialize rows to JSON, unlock. The accept loop is sequential, so a read
//! never races a write; it just waits behind one.
//!
//! SQL discipline (all verified against the bundled DuckDB v1.5.3 CLI):
//!   * subtree = first `depth` backslash segments of `path`:
//!       array_to_string(list_slice(string_split(path,'\'),1,depth),'\')
//!     Root-level paths (fewer than `depth` segments) collapse to the whole path.
//!   * Aggregate casts are MANDATORY — DuckDB promotes SUM(INTEGER) to HUGEINT
//!     and duckdb-rs FromSql is type-strict. `SUM(alloc_bytes)::BIGINT`,
//!     `SUM(? * reads + ? * writes)::DOUBLE` (the literal-free weighted sum is
//!     otherwise DECIMAL), `COUNT(DISTINCT day_index)` is already BIGINT.
//!   * EVERY user value is a bound `?` parameter — nothing is interpolated into
//!     SQL. The only string-built parts of a statement are fixed SQL fragments
//!     (optional `AND server = ?` filters) whose VALUES are still bound.
//!
//! Window anchor is chrono-free: `MAX(day_index)` within scope; the window
//! predicate is `day_index > (anchor - window)`.

use std::path::Path;

use duckdb::types::ToSqlOutput;
use duckdb::{params_from_iter, Connection, ToSql};
use serde::Serialize;

use crate::db::Db;

/// Explicit column map for the tier-log CSV (read_csv needs explicit columns so
/// a present-but-empty or oddly-typed file can't surprise the planner).
const TIER_COLUMNS: &str = "{'server':'VARCHAR','share':'VARCHAR','path_prefix':'VARCHAR','tier':'VARCHAR','migrated_on':'VARCHAR','new_server':'VARCHAR','new_share':'VARCHAR','note':'VARCHAR'}";

// ---- a tiny bound-parameter value -----------------------------------------

/// One bound SQL parameter. Lets us push heterogeneous values (the depth int,
/// the f64 weights, the optional string scopes) into a single `Vec` in the exact
/// textual order their `?` placeholders appear, then bind via `params_from_iter`.
/// Keeps ALL user input bound — never interpolated.
enum P {
    I(i64),
    F(f64),
    S(String),
}

impl ToSql for P {
    fn to_sql(&self) -> duckdb::Result<ToSqlOutput<'_>> {
        match self {
            P::I(v) => v.to_sql(),
            P::F(v) => v.to_sql(),
            P::S(v) => v.to_sql(),
        }
    }
}

// ---- knobs (query params) -------------------------------------------------

/// Resolved query knobs. All optional on the wire; defaults match the brief's
/// starting values. Echoed back in every response so curl/UI can prove what
/// actually applied.
#[derive(Debug, Clone)]
pub struct Knobs {
    pub w_read: f64,
    pub w_write: f64,
    pub window: i64,
    pub min_span: i64,
    pub depth: i64,
    pub limit: i64,
    pub server: Option<String>,
    pub share: Option<String>,
    pub parent: Option<String>,
}

impl Default for Knobs {
    fn default() -> Self {
        Knobs {
            w_read: 1.0,
            w_write: 2.0,
            window: 30,
            min_span: 3,
            depth: 1,
            limit: 50,
            server: None,
            share: None,
            parent: None,
        }
    }
}

/// Parse + validate a raw query string (the part after `?`) into [`Knobs`].
/// Returns `Err(message)` for bad input; the caller renders it as a 400 JSON
/// error. Validation per the brief: depth 1-16, window 1-3650, weights >= 0,
/// limit 1-1000, min_span >= 0.
pub fn parse_knobs(query: &str) -> Result<Knobs, String> {
    let m = parse_query_string(query);
    let mut k = Knobs::default();

    if let Some(v) = m.get("w_read") {
        k.w_read = v.parse().map_err(|_| "w_read must be a number".to_string())?;
    }
    if let Some(v) = m.get("w_write") {
        k.w_write = v.parse().map_err(|_| "w_write must be a number".to_string())?;
    }
    if let Some(v) = m.get("window") {
        k.window = v.parse().map_err(|_| "window must be an integer".to_string())?;
    }
    if let Some(v) = m.get("min_span") {
        k.min_span = v.parse().map_err(|_| "min_span must be an integer".to_string())?;
    }
    if let Some(v) = m.get("depth") {
        k.depth = v.parse().map_err(|_| "depth must be an integer".to_string())?;
    }
    if let Some(v) = m.get("limit") {
        k.limit = v.parse().map_err(|_| "limit must be an integer".to_string())?;
    }
    k.server = non_empty(m.get("server"));
    k.share = non_empty(m.get("share"));
    k.parent = non_empty(m.get("parent"));

    if !(1..=16).contains(&k.depth) {
        return Err("depth must be between 1 and 16".into());
    }
    if !(1..=3650).contains(&k.window) {
        return Err("window must be between 1 and 3650".into());
    }
    if k.w_read < 0.0 || k.w_write < 0.0 {
        return Err("weights must be >= 0".into());
    }
    if !(1..=1000).contains(&k.limit) {
        return Err("limit must be between 1 and 1000".into());
    }
    if k.min_span < 0 {
        return Err("min_span must be >= 0".into());
    }
    Ok(k)
}

/// Trim an optional query value, treating an empty string as absent.
fn non_empty(v: Option<&String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

// ---- leaderboard ----------------------------------------------------------

#[derive(Serialize)]
struct LeaderboardKnobs<'a> {
    w_read: f64,
    w_write: f64,
    window: i64,
    min_span: i64,
    depth: i64,
    limit: i64,
    server: Option<&'a str>,
    share: Option<&'a str>,
}

#[derive(Serialize)]
struct LeaderboardRow {
    server: String,
    share: String,
    subtree: String,
    bytes: Option<i64>,
    unknown_bytes_files: i64,
    files: i64,
    demand: f64,
    span: i64,
    density: Option<f64>,
    tier: Option<String>,
    migrated_on: Option<String>,
    note: Option<String>,
}

#[derive(Serialize)]
struct LeaderboardResponse<'a> {
    knobs: LeaderboardKnobs<'a>,
    anchor_day: Option<i64>,
    rows: Vec<LeaderboardRow>,
}

/// `GET /api/leaderboard`: hottest subtrees by density. Returns the serialized
/// JSON body. INNER-joins heat to bytes (cold subtrees do not rank), filters
/// `span >= min_span`, orders `density DESC NULLS LAST`, limits.
pub fn leaderboard(db: &Db, k: &Knobs, tier_log: Option<&Path>) -> duckdb::Result<String> {
    let kn = LeaderboardKnobs {
        w_read: k.w_read,
        w_write: k.w_write,
        window: k.window,
        min_span: k.min_span,
        depth: k.depth,
        limit: k.limit,
        server: k.server.as_deref(),
        share: k.share.as_deref(),
    };

    let conn = db.lock();
    let anchor = scope_anchor(&conn, k.server.as_deref(), k.share.as_deref())?;
    let Some(anchor) = anchor else {
        // No day_counts in scope: nothing to rank. Echo a null anchor.
        let resp = LeaderboardResponse {
            knobs: kn,
            anchor_day: None,
            rows: Vec::new(),
        };
        return Ok(serde_json::to_string(&resp).expect("serialize leaderboard"));
    };
    let threshold = anchor - k.window;

    let (sql, p) = build_leaderboard_sql(k, threshold, tier_log);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(p.iter()), |r| {
            Ok(LeaderboardRow {
                server: r.get(0)?,
                share: r.get(1)?,
                subtree: r.get(2)?,
                bytes: r.get(3)?,
                unknown_bytes_files: r.get(4)?,
                files: r.get(5)?,
                demand: r.get(6)?,
                span: r.get(7)?,
                density: r.get(8)?,
                tier: r.get(9)?,
                migrated_on: r.get(10)?,
                note: r.get(11)?,
            })
        })?
        .collect::<duckdb::Result<Vec<_>>>()?;

    let resp = LeaderboardResponse {
        knobs: kn,
        anchor_day: Some(anchor),
        rows,
    };
    Ok(serde_json::to_string(&resp).expect("serialize leaderboard"))
}

/// Build the leaderboard SQL + its bound params in textual `?` order:
///   depth, [server], [share], w_read, w_write, threshold, [tier_log], min_span, limit
fn build_leaderboard_sql(k: &Knobs, threshold: i64, tier_log: Option<&Path>) -> (String, Vec<P>) {
    let mut p: Vec<P> = Vec::new();
    let mut sql = String::new();

    // scoped_files: every file in scope, tagged with its depth-`depth` subtree.
    sql.push_str(
        "WITH scoped_files AS (\n  \
           SELECT file_id, server, share,\n         \
                  array_to_string(list_slice(string_split(path, '\\'), 1, ?), '\\') AS subtree,\n         \
                  alloc_bytes\n  \
           FROM files\n  WHERE 1=1",
    );
    p.push(P::I(k.depth));
    if let Some(s) = &k.server {
        sql.push_str(" AND server = ?");
        p.push(P::S(s.clone()));
    }
    if let Some(s) = &k.share {
        sql.push_str(" AND share = ?");
        p.push(P::S(s.clone()));
    }

    // bytes_block: over ALL files in the subtree (hot or cold).
    // heat_block: weighted demand + day-span over day_counts within the window.
    sql.push_str(
        "\n),\nbytes_block AS (\n  \
           SELECT server, share, subtree,\n         \
                  SUM(alloc_bytes)::BIGINT AS bytes,\n         \
                  COUNT(*) FILTER (WHERE alloc_bytes IS NULL) AS unknown_bytes_files,\n         \
                  COUNT(*) AS files\n  \
           FROM scoped_files\n  GROUP BY server, share, subtree\n),\n\
         heat_block AS (\n  \
           SELECT sf.server, sf.share, sf.subtree,\n         \
                  SUM(? * dc.reads + ? * dc.writes)::DOUBLE AS demand,\n         \
                  COUNT(DISTINCT dc.day_index) AS span\n  \
           FROM day_counts dc JOIN scoped_files sf ON sf.file_id = dc.file_id\n  \
           WHERE dc.day_index > ?\n  GROUP BY sf.server, sf.share, sf.subtree\n)\n",
    );
    p.push(P::F(k.w_read));
    p.push(P::F(k.w_write));
    p.push(P::I(threshold));

    sql.push_str(
        "SELECT b.server, b.share, b.subtree, b.bytes, b.unknown_bytes_files, b.files,\n       \
                h.demand, h.span, h.demand / NULLIF(b.bytes, 0) AS density,\n",
    );
    push_tier_select_and_join(&mut sql, &mut p, tier_log, "b");

    sql.push_str("WHERE h.span >= ?\nORDER BY density DESC NULLS LAST\nLIMIT ?");
    p.push(P::I(k.min_span));
    p.push(P::I(k.limit));

    (sql, p)
}

// ---- children (split-explorer) --------------------------------------------

#[derive(Serialize)]
struct ChildrenKnobs<'a> {
    w_read: f64,
    w_write: f64,
    window: i64,
    depth: i64,
    server: Option<&'a str>,
    share: Option<&'a str>,
}

#[derive(Serialize)]
struct ParentTotals {
    bytes: Option<i64>,
    demand: f64,
}

#[derive(Serialize)]
struct ChildRow {
    subtree: String,
    bytes: Option<i64>,
    demand: f64,
    density: Option<f64>,
    span: i64,
    pct_bytes: Option<f64>,
    pct_demand: Option<f64>,
    unknown_bytes_files: i64,
}

#[derive(Serialize)]
struct ChildrenResponse<'a> {
    knobs: ChildrenKnobs<'a>,
    parent: &'a str,
    parent_totals: ParentTotals,
    rows: Vec<ChildRow>,
}

/// One child row straight out of SQL (before pct_* are computed in Rust against
/// the parent totals).
struct RawChild {
    subtree: String,
    bytes: Option<i64>,
    demand: f64,
    density: Option<f64>,
    span: i64,
    unknown_bytes_files: i64,
}

/// `GET /api/children`: carve one subtree into its immediate children. `parent`
/// is required. NO min_span filter (carving needs the cold majority visible);
/// LEFT-joins bytes to heat so cold children still appear; orders demand DESC.
pub fn children(db: &Db, k: &Knobs) -> Result<String, String> {
    let parent = k
        .parent
        .as_deref()
        .ok_or_else(|| "parent is required".to_string())?;

    let kn = ChildrenKnobs {
        w_read: k.w_read,
        w_write: k.w_write,
        window: k.window,
        depth: k.depth,
        server: k.server.as_deref(),
        share: k.share.as_deref(),
    };

    // Child depth = (segment count of parent) + 1. parent is BACKSLASH-separated.
    let child_depth = parent.split('\\').count() as i64 + 1;

    let conn = db.lock();
    let raw = children_query(&conn, k, parent, child_depth).map_err(|e| e.to_string())?;

    // Parent totals = the sum across children (every scoped file lands in exactly
    // one child subtree, so the child sums reconstruct the parent totals). bytes
    // is None only if NO child had a known byte count.
    let mut sum_bytes: i64 = 0;
    let mut any_bytes = false;
    let mut sum_demand: f64 = 0.0;
    for c in &raw {
        if let Some(b) = c.bytes {
            sum_bytes += b;
            any_bytes = true;
        }
        sum_demand += c.demand;
    }
    let parent_bytes = any_bytes.then_some(sum_bytes);
    let parent_demand = sum_demand;

    let rows = raw
        .into_iter()
        .map(|c| {
            let pct_bytes = match (c.bytes, parent_bytes) {
                (Some(b), Some(pb)) if pb != 0 => Some(b as f64 / pb as f64),
                _ => None,
            };
            let pct_demand = if parent_demand != 0.0 {
                Some(c.demand / parent_demand)
            } else {
                None
            };
            ChildRow {
                subtree: c.subtree,
                bytes: c.bytes,
                demand: c.demand,
                density: c.density,
                span: c.span,
                pct_bytes,
                pct_demand,
                unknown_bytes_files: c.unknown_bytes_files,
            }
        })
        .collect();

    let resp = ChildrenResponse {
        knobs: kn,
        parent,
        parent_totals: ParentTotals {
            bytes: parent_bytes,
            demand: parent_demand,
        },
        rows,
    };
    Ok(serde_json::to_string(&resp).expect("serialize children"))
}

/// Run the children aggregation. Params in textual `?` order:
///   child_depth, parent (path=?), parent_like (path LIKE ? ESCAPE '!'),
///   [server], [share], w_read, w_write, threshold
fn children_query(
    conn: &Connection,
    k: &Knobs,
    parent: &str,
    child_depth: i64,
) -> duckdb::Result<Vec<RawChild>> {
    let parent_like = format!("{}\\%", escape_like(parent));

    // Window anchor over the children scope (under the parent + any server/share
    // knob). No rows in scope -> empty result.
    let anchor = children_anchor(conn, k, parent, &parent_like)?;
    let Some(anchor) = anchor else {
        return Ok(Vec::new());
    };
    let threshold = anchor - k.window;

    let mut p: Vec<P> = Vec::new();
    let mut sql = String::new();
    sql.push_str(
        "WITH scoped_files AS (\n  \
           SELECT file_id,\n         \
                  array_to_string(list_slice(string_split(path, '\\'), 1, ?), '\\') AS subtree,\n         \
                  alloc_bytes\n  \
           FROM files\n  WHERE (path = ? OR path LIKE ? ESCAPE '!')",
    );
    p.push(P::I(child_depth));
    p.push(P::S(parent.to_string()));
    p.push(P::S(parent_like.clone()));
    if let Some(s) = &k.server {
        sql.push_str(" AND server = ?");
        p.push(P::S(s.clone()));
    }
    if let Some(s) = &k.share {
        sql.push_str(" AND share = ?");
        p.push(P::S(s.clone()));
    }

    sql.push_str(
        "\n),\nbytes_block AS (\n  \
           SELECT subtree,\n         \
                  SUM(alloc_bytes)::BIGINT AS bytes,\n         \
                  COUNT(*) FILTER (WHERE alloc_bytes IS NULL) AS unknown_bytes_files,\n         \
                  COUNT(*) AS files\n  \
           FROM scoped_files\n  GROUP BY subtree\n),\n\
         heat_block AS (\n  \
           SELECT sf.subtree,\n         \
                  SUM(? * dc.reads + ? * dc.writes)::DOUBLE AS demand,\n         \
                  COUNT(DISTINCT dc.day_index) AS span\n  \
           FROM day_counts dc JOIN scoped_files sf ON sf.file_id = dc.file_id\n  \
           WHERE dc.day_index > ?\n  GROUP BY sf.subtree\n)\n\
         SELECT b.subtree, b.bytes, COALESCE(h.demand, 0.0) AS demand,\n        \
                COALESCE(h.demand, 0.0) / NULLIF(b.bytes, 0) AS density,\n        \
                COALESCE(h.span, 0) AS span, b.unknown_bytes_files\n  \
         FROM bytes_block b LEFT JOIN heat_block h USING (subtree)\n  \
         ORDER BY demand DESC",
    );
    p.push(P::F(k.w_read));
    p.push(P::F(k.w_write));
    p.push(P::I(threshold));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(p.iter()), |r| {
            Ok(RawChild {
                subtree: r.get(0)?,
                bytes: r.get(1)?,
                demand: r.get(2)?,
                density: r.get(3)?,
                span: r.get(4)?,
                unknown_bytes_files: r.get(5)?,
            })
        })?
        .collect::<duckdb::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Window anchor for the children scope. Params: parent, parent_like, [server], [share].
fn children_anchor(
    conn: &Connection,
    k: &Knobs,
    parent: &str,
    parent_like: &str,
) -> duckdb::Result<Option<i64>> {
    let mut p: Vec<P> = Vec::new();
    let mut sql = String::from(
        "SELECT MAX(dc.day_index)::BIGINT FROM day_counts dc JOIN files f USING (file_id) \
         WHERE (f.path = ? OR f.path LIKE ? ESCAPE '!')",
    );
    p.push(P::S(parent.to_string()));
    p.push(P::S(parent_like.to_string()));
    if let Some(s) = &k.server {
        sql.push_str(" AND f.server = ?");
        p.push(P::S(s.clone()));
    }
    if let Some(s) = &k.share {
        sql.push_str(" AND f.share = ?");
        p.push(P::S(s.clone()));
    }
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_row(params_from_iter(p.iter()), |r| r.get(0))
}

// ---- health ---------------------------------------------------------------

#[derive(Serialize)]
struct HealthServer {
    server: String,
    last_dump_at: String,
    last_dump_seq: i64,
    seconds_stale: i64,
}

#[derive(Serialize)]
struct HealthResponse {
    now: String,
    servers: Vec<HealthServer>,
}

/// `GET /api/health`: per-server freshness. `now_secs` is the caller's unix
/// clock; staleness is derived from it and the stored (naive-UTC) timestamps via
/// `epoch()`. `now` and `last_dump_at` are rendered with the existing RFC3339
/// helper (chrono-free).
pub fn health(db: &Db, now_secs: i64) -> duckdb::Result<String> {
    let conn = db.lock();
    let mut stmt = conn.prepare(
        "SELECT server, \
                arg_max(last_dump_seq, last_dump_at)::BIGINT AS last_dump_seq, \
                epoch(MAX(last_dump_at))::BIGINT AS last_epoch \
         FROM runs GROUP BY server ORDER BY server",
    )?;
    let servers = stmt
        .query_map([], |r| {
            let server: String = r.get(0)?;
            let last_dump_seq: i64 = r.get(1)?;
            let last_epoch: i64 = r.get(2)?;
            Ok(HealthServer {
                server,
                last_dump_at: crate::rfc3339_utc(last_epoch),
                last_dump_seq,
                seconds_stale: now_secs - last_epoch,
            })
        })?
        .collect::<duckdb::Result<Vec<_>>>()?;

    let resp = HealthResponse {
        now: crate::rfc3339_utc(now_secs),
        servers,
    };
    Ok(serde_json::to_string(&resp).expect("serialize health"))
}

// ---- shared helpers -------------------------------------------------------

/// Window anchor = MAX(day_index) over the (optionally server/share-scoped)
/// day_counts. `None` => no rows in scope.
fn scope_anchor(
    conn: &Connection,
    server: Option<&str>,
    share: Option<&str>,
) -> duckdb::Result<Option<i64>> {
    let mut p: Vec<P> = Vec::new();
    let mut sql = String::from(
        "SELECT MAX(dc.day_index)::BIGINT FROM day_counts dc JOIN files f USING (file_id) WHERE 1=1",
    );
    if let Some(s) = server {
        sql.push_str(" AND f.server = ?");
        p.push(P::S(s.to_string()));
    }
    if let Some(s) = share {
        sql.push_str(" AND f.share = ?");
        p.push(P::S(s.to_string()));
    }
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_row(params_from_iter(p.iter()), |r| r.get(0))
}

/// Append the tier columns + (when a tier-log path is given) the longest-prefix
/// LEFT JOIN LATERAL onto read_csv. `alias` is the bytes-block alias holding
/// `server`/`share`/`subtree`. Pushes the tier-log path param when present.
fn push_tier_select_and_join(sql: &mut String, p: &mut Vec<P>, tier_log: Option<&Path>, alias: &str) {
    match tier_log {
        Some(path) => {
            sql.push_str("       t.tier, t.migrated_on, t.note\n");
            sql.push_str(&format!(
                "FROM heat_block h JOIN bytes_block {alias} USING (server, share, subtree)\n\
                 LEFT JOIN LATERAL (\n  \
                   SELECT tier, migrated_on, note\n  \
                   FROM read_csv(?, header=true, columns={TIER_COLUMNS}) tl\n  \
                   WHERE tl.server = {alias}.server AND tl.share = {alias}.share\n    \
                     AND ({alias}.subtree = tl.path_prefix OR {alias}.subtree LIKE tl.path_prefix || '\\%')\n  \
                   ORDER BY LENGTH(tl.path_prefix) DESC LIMIT 1\n\
                 ) t ON TRUE\n"
            ));
            p.push(P::S(path.to_string_lossy().into_owned()));
        }
        None => {
            sql.push_str("       NULL::VARCHAR AS tier, NULL::VARCHAR AS migrated_on, NULL::VARCHAR AS note\n");
            sql.push_str(&format!(
                "FROM heat_block h JOIN bytes_block {alias} USING (server, share, subtree)\n"
            ));
        }
    }
}

/// Escape LIKE metacharacters (`!`, `%`, `_`) with `!` so a bound `parent` value
/// is matched literally. Backslash (the path separator) is NOT special in LIKE
/// and is therefore left alone — which is also why `!`, not `\`, is the escape
/// char. The caller appends a literal `\%` AFTER this for the descendant match.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '!' || c == '%' || c == '_' {
            out.push('!');
        }
        out.push(c);
    }
    out
}

/// Parse a `key=value&key=value` query string into a map, percent-decoding both
/// keys and values. A bare `key` (no `=`) maps to an empty value.
fn parse_query_string(q: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        map.insert(percent_decode(k), percent_decode(v));
    }
    map
}

/// Minimal application/x-www-form-urlencoded decode: `+` -> space, `%XX` -> byte.
/// A malformed `%` escape is passed through literally.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => match (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Hex digit -> nibble; `None` for a non-hex byte.
fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use serde_json::Value;
    use std::io::Write;

    // Hand-planted fixture. Server 's1', share 'data'. Two runs (r1 primary, r2
    // for the same-day case). Days 100..105 (>=5 distinct days). file 4 has NULL
    // alloc_bytes (the NULL-bytes subtree, projects\gamma). file 5 is root-level
    // (one segment). file 1 is written on day 100 by BOTH r1 and r2 — the
    // two-runs-same-day case (span must count day 100 once; demand sums both).
    //
    // Hand-computed truth (w_read=1, w_write=2, window=30 => all days in window):
    //
    //   per-file demand:
    //     f1: r1 d100=8, d101=8, d102=4; r2 d100=10  => 30  (reads only)
    //     f2: d100 (5+1*2)=7, d103=5               => 12
    //     f3: d100 (1+4*2)=9, d101 (1+4*2)=9, d104 (0+2*2)=4 => 22 (write-heavy)
    //     f4: d105=9                                => 9
    //     f5: d105=7                                => 7
    //
    //   depth 1:
    //     projects     -> files {1,2,3,4}: bytes 6000, unknown 1, files 4,
    //                     demand 73, span 6 (days 100..105), density 73/6000
    //     rootfile.txt -> file {5}: bytes 300, demand 7, span 1
    //   depth 2:
    //     projects\alpha -> {1,2}: bytes 3000, demand 42, span 4 (100..103)
    //     projects\beta  -> {3}:   bytes 3000, demand 22, span 3 (100,101,104)
    //     projects\gamma -> {4}:   bytes NULL, demand 9,  span 1, density NULL
    //     rootfile.txt   -> {5}:   bytes 300,  demand 7,  span 1
    const FIXTURE_SQL: &str = r#"
INSERT INTO files (file_id, server, share, path, alloc_bytes, restat, unresolved_share, first_seen, last_seen) VALUES
 (1,'s1','data','projects\alpha\a.tif',1000,false,false,'2026-01-01','2026-01-01'),
 (2,'s1','data','projects\alpha\b.tif',2000,false,false,'2026-01-01','2026-01-01'),
 (3,'s1','data','projects\beta\c.shp', 3000,false,false,'2026-01-01','2026-01-01'),
 (4,'s1','data','projects\gamma\d.dat',NULL,true, false,'2026-01-01','2026-01-01'),
 (5,'s1','data','rootfile.txt',         300,false,false,'2026-01-01','2026-01-01');

INSERT INTO runs (run_id, server, started_at, last_dump_seq, last_dump_at) VALUES
 (CAST('11111111-1111-1111-1111-111111111111' AS UUID),'s1','2026-01-01',2,'2026-06-10 00:00:00'),
 (CAST('22222222-2222-2222-2222-222222222222' AS UUID),'s1','2026-01-01',1,'2026-06-09 00:00:00'),
 (CAST('33333333-3333-3333-3333-333333333333' AS UUID),'s2','2026-01-01',5,'2026-06-08 00:00:00');

INSERT INTO day_counts (file_id, run_id, day_index, reads, writes) VALUES
 (1,CAST('11111111-1111-1111-1111-111111111111' AS UUID),100,8,0),
 (1,CAST('11111111-1111-1111-1111-111111111111' AS UUID),101,8,0),
 (1,CAST('11111111-1111-1111-1111-111111111111' AS UUID),102,4,0),
 (1,CAST('22222222-2222-2222-2222-222222222222' AS UUID),100,10,0),
 (2,CAST('11111111-1111-1111-1111-111111111111' AS UUID),100,5,1),
 (2,CAST('11111111-1111-1111-1111-111111111111' AS UUID),103,5,0),
 (3,CAST('11111111-1111-1111-1111-111111111111' AS UUID),100,1,4),
 (3,CAST('11111111-1111-1111-1111-111111111111' AS UUID),101,1,4),
 (3,CAST('11111111-1111-1111-1111-111111111111' AS UUID),104,0,2),
 (4,CAST('11111111-1111-1111-1111-111111111111' AS UUID),105,9,0),
 (5,CAST('11111111-1111-1111-1111-111111111111' AS UUID),105,7,0);
"#;

    fn fixture() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("q.duckdb")).unwrap();
        {
            let conn = db.lock();
            conn.execute_batch(FIXTURE_SQL).unwrap();
        }
        (dir, db)
    }

    fn knobs(depth: i64, min_span: i64) -> Knobs {
        Knobs {
            depth,
            min_span,
            ..Knobs::default()
        }
    }

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    /// Find the row whose `subtree` equals `name`.
    fn row<'a>(v: &'a Value, name: &str) -> &'a Value {
        v["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["subtree"] == Value::from(name))
            .unwrap_or_else(|| panic!("no row with subtree {name} in {v}"))
    }

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "expected {b}, got {a}");
    }

    // §12.3 reconciliation gate, depth 1.
    #[test]
    fn leaderboard_depth1_reconciliation() {
        let (_d, db) = fixture();
        let v = parse(&leaderboard(&db, &knobs(1, 3), None).unwrap());

        assert_eq!(v["anchor_day"], Value::from(105));
        // min_span=3 keeps only `projects` (span 6); rootfile.txt (span 1) drops.
        assert_eq!(v["rows"].as_array().unwrap().len(), 1);

        let p = row(&v, "projects");
        assert_eq!(p["server"], Value::from("s1"));
        assert_eq!(p["bytes"], Value::from(6000));
        assert_eq!(p["unknown_bytes_files"], Value::from(1));
        assert_eq!(p["files"], Value::from(4));
        approx(p["demand"].as_f64().unwrap(), 73.0);
        assert_eq!(p["span"], Value::from(6));
        approx(p["density"].as_f64().unwrap(), 73.0 / 6000.0);
        assert!(p["tier"].is_null());
    }

    // §12.3 reconciliation gate, depth 2 — and the same-day-two-runs invariant:
    // projects\alpha demand (42) INCLUDES r2's day-100 contribution, while its
    // span (4) counts day 100 only once.
    #[test]
    fn leaderboard_depth2_reconciliation_and_span_distinct() {
        let (_d, db) = fixture();
        let v = parse(&leaderboard(&db, &knobs(2, 3), None).unwrap());

        // alpha (density .014) ranks above beta (.00733); gamma & rootfile drop
        // (span 1 < 3).
        let names: Vec<&str> = v["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["subtree"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["projects\\alpha", "projects\\beta"]);

        let alpha = row(&v, "projects\\alpha");
        assert_eq!(alpha["bytes"], Value::from(3000));
        assert_eq!(alpha["files"], Value::from(2));
        approx(alpha["demand"].as_f64().unwrap(), 42.0); // 30 (incl. r2's 10) + 12
        assert_eq!(alpha["span"], Value::from(4)); // day 100 counted once
        approx(alpha["density"].as_f64().unwrap(), 42.0 / 3000.0);

        let beta = row(&v, "projects\\beta");
        approx(beta["demand"].as_f64().unwrap(), 22.0);
        assert_eq!(beta["span"], Value::from(3));
    }

    // Root-level file (fewer than `depth` segments) collapses to its own
    // subtree and is reachable when min_span permits.
    #[test]
    fn root_level_file_is_its_own_subtree() {
        let (_d, db) = fixture();
        let v = parse(&leaderboard(&db, &knobs(2, 1), None).unwrap());
        let root = row(&v, "rootfile.txt");
        assert_eq!(root["bytes"], Value::from(300));
        assert_eq!(root["span"], Value::from(1));
        approx(root["demand"].as_f64().unwrap(), 7.0);
    }

    // Knob re-rank: alpha is read-heavy (reads 40, writes 1), beta is
    // write-heavy (reads 2, writes 10), equal bytes. Bumping w_write flips them.
    #[test]
    fn knob_reweight_reranks() {
        let (_d, db) = fixture();

        let light = Knobs { depth: 2, min_span: 3, w_read: 1.0, w_write: 2.0, ..Knobs::default() };
        let heavy = Knobs { depth: 2, min_span: 3, w_read: 1.0, w_write: 20.0, ..Knobs::default() };

        let order = |k: &Knobs| -> Vec<String> {
            let v = parse(&leaderboard(&db, k, None).unwrap());
            v["rows"].as_array().unwrap().iter()
                .map(|r| r["subtree"].as_str().unwrap().to_string())
                .collect()
        };
        let lo = order(&light);
        let ho = order(&heavy);
        assert_eq!(lo[0], "projects\\alpha"); // read-heavy wins at low w_write
        assert_eq!(ho[0], "projects\\beta"); // write-heavy wins at high w_write
        assert_ne!(lo, ho);
    }

    // children carve of `projects` -> alpha, beta, gamma. NO min_span filter, so
    // the cold/NULL gamma stays visible. pct_* and parent_totals reconcile.
    #[test]
    fn children_carve_reconciliation() {
        let (_d, db) = fixture();
        let k = Knobs { parent: Some("projects".into()), ..Knobs::default() };
        let v = parse(&children(&db, &k).unwrap());

        assert_eq!(v["parent"], Value::from("projects"));
        assert_eq!(v["parent_totals"]["bytes"], Value::from(6000)); // 3000+3000+NULL
        approx(v["parent_totals"]["demand"].as_f64().unwrap(), 73.0);

        // Ordered by demand DESC: alpha(42) > beta(22) > gamma(9).
        let names: Vec<&str> = v["rows"].as_array().unwrap().iter()
            .map(|r| r["subtree"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["projects\\alpha", "projects\\beta", "projects\\gamma"]);

        let alpha = row(&v, "projects\\alpha");
        approx(alpha["pct_bytes"].as_f64().unwrap(), 0.5); // 3000/6000
        approx(alpha["pct_demand"].as_f64().unwrap(), 42.0 / 73.0);

        // gamma: NULL bytes -> bytes/density/pct_bytes all null, but still listed.
        let gamma = row(&v, "projects\\gamma");
        assert!(gamma["bytes"].is_null());
        assert!(gamma["density"].is_null());
        assert!(gamma["pct_bytes"].is_null());
        approx(gamma["pct_demand"].as_f64().unwrap(), 9.0 / 73.0);
        assert_eq!(gamma["unknown_bytes_files"], Value::from(1));
    }

    // A child segment containing a literal % must not act as a wildcard. Carving
    // `projects\beta` should never pull in `projects\beta`-prefixed siblings.
    #[test]
    fn children_missing_parent_is_empty() {
        let (_d, db) = fixture();
        // A parent with no descendants -> empty rows, zeroed totals.
        let k = Knobs { parent: Some("nope".into()), ..Knobs::default() };
        let v = parse(&children(&db, &k).unwrap());
        assert_eq!(v["rows"].as_array().unwrap().len(), 0);
        assert!(v["parent_totals"]["bytes"].is_null());
        approx(v["parent_totals"]["demand"].as_f64().unwrap(), 0.0);
    }

    // tier-log: write a tiny CSV; longest-prefix wins (alpha gets its own `cold`
    // entry over the ancestor `projects` `warm`; beta inherits `projects`).
    #[test]
    fn tier_log_present_longest_prefix() {
        let (dir, db) = fixture();
        let csv = dir.path().join("tier-log.csv");
        let mut f = std::fs::File::create(&csv).unwrap();
        write!(
            f,
            "server,share,path_prefix,tier,migrated_on,new_server,new_share,note\n\
             s1,data,projects,warm,2026-01-01,,,project area\n\
             s1,data,projects\\alpha,cold,2026-02-01,arch1,archive,alpha migrated\n"
        )
        .unwrap();
        drop(f);

        let v = parse(&leaderboard(&db, &knobs(2, 3), Some(&csv)).unwrap());

        let alpha = row(&v, "projects\\alpha");
        assert_eq!(alpha["tier"], Value::from("cold")); // own entry beats ancestor
        assert_eq!(alpha["migrated_on"], Value::from("2026-02-01"));
        assert_eq!(alpha["note"], Value::from("alpha migrated"));

        let beta = row(&v, "projects\\beta");
        assert_eq!(beta["tier"], Value::from("warm")); // inherits `projects`
        assert_eq!(beta["note"], Value::from("project area"));
    }

    // tier-log absent (path does not exist): the endpoint guards read_csv, so the
    // query still runs and tier columns come back NULL.
    #[test]
    fn tier_log_absent_yields_null_columns() {
        let (dir, db) = fixture();
        let missing = dir.path().join("does-not-exist.csv");
        // Mirror the handler's per-request existence guard.
        let tl = Some(missing.as_path()).filter(|p| p.exists());
        assert!(tl.is_none());
        let v = parse(&leaderboard(&db, &knobs(2, 3), tl).unwrap());
        let alpha = row(&v, "projects\\alpha");
        assert!(alpha["tier"].is_null());
        assert!(alpha["migrated_on"].is_null());
        assert!(alpha["note"].is_null());
    }

    #[test]
    fn health_per_server_staleness() {
        let (_d, db) = fixture();
        // 2026-06-10T00:00:00Z = 1_781_049_600; pin "now" 300s later.
        let now = 1_781_049_600 + 300;
        let v = parse(&health(&db, now).unwrap());

        let servers = v["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 2); // s1, s2 (ordered)
        let s1 = &servers[0];
        assert_eq!(s1["server"], Value::from("s1"));
        assert_eq!(s1["last_dump_seq"], Value::from(2)); // arg_max picks the latest run
        assert_eq!(s1["last_dump_at"], Value::from("2026-06-10T00:00:00Z"));
        assert_eq!(s1["seconds_stale"], Value::from(300));
        // s2 is older (2026-06-08) -> larger staleness.
        assert!(servers[1]["seconds_stale"].as_i64().unwrap() > 300);
    }

    // Param validation: garbage -> Err (the handler maps to 400); defaults parse.
    #[test]
    fn parse_knobs_happy_and_garbage() {
        let k = parse_knobs("w_read=3&w_write=0.5&depth=2&window=10&limit=5&server=s1").unwrap();
        approx(k.w_read, 3.0);
        approx(k.w_write, 0.5);
        assert_eq!(k.depth, 2);
        assert_eq!(k.server.as_deref(), Some("s1"));

        assert!(parse_knobs("depth=99").is_err()); // out of 1..16
        assert!(parse_knobs("w_read=-1").is_err()); // negative weight
        assert!(parse_knobs("limit=0").is_err()); // out of 1..1000
        assert!(parse_knobs("window=banana").is_err()); // non-numeric
    }

    // Percent-decoding of a backslash subtree (parent=projects%5Calpha).
    #[test]
    fn parse_knobs_percent_decodes_backslash() {
        let k = parse_knobs("parent=projects%5Calpha").unwrap();
        assert_eq!(k.parent.as_deref(), Some("projects\\alpha"));
    }
}
