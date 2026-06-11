//! Pure parse / validate / archive-naming logic for the `/ingest` endpoint.
//!
//! Nothing here touches the network or the filesystem: the HTTP read lives in
//! `main.rs`, the archive write in `archive.rs`. Keeping this module pure means
//! the whole request contract — header parse, untrusted-name validation,
//! archive-path naming, and row/footer framing — is exercised by `cargo test`
//! with no socket and no disk.
//!
//! Wire format (NDJSON, one document per POST):
//!   line 1:  {"type":"header","server":...,"run_id":...,"dump_seq":...,...}
//!   lines:   {"type":"row",...}
//!   last:    {"type":"footer","rows":<count of row lines>}

use serde::Deserialize;

/// The header fields we read. `walked_shares` is part of the wire format but we
/// don't act on it, so it is not deserialized. `server`/`run_id`/`dump_seq` are
/// `Option` so a *missing* field is distinguishable from a *wrong-typed* one: a
/// missing field lands as `None` (a clean "missing" rejection), while a wrong
/// type fails `from_str` outright. `emitted_at` is optional at parse time (its
/// absence does not change step-2 classification); the db layer requires it.
#[derive(Debug, Deserialize)]
struct RawHeader {
    #[serde(rename = "type")]
    kind: String,
    server: Option<String>,
    run_id: Option<String>,
    dump_seq: Option<u32>,
    emitted_at: Option<String>,
}

/// A parsed, present-and-typed header. The field *values* are not yet validated
/// as safe path components — that is [`validate_naming`]'s job. `emitted_at` is
/// carried through to the db layer (first_seen/last_seen/started_at); it is
/// `Option` because step-2 classification never gated on it.
#[derive(Debug, Clone)]
pub struct Header {
    pub server: String,
    pub run_id: String,
    pub dump_seq: u32,
    pub emitted_at: Option<String>,
}

/// One fully-parsed `row` line. Parsed ONCE here (during classify) and handed to
/// the db layer in `Disposition::Accepted` — db.rs never re-parses the body.
/// `day` is the agent's raw `i32` day-index; `alloc_bytes` is `None` for a JSON
/// `null` (e.g. a restat row); `flags` defaults to empty if the key is absent.
#[derive(Debug, Clone)]
pub struct DumpRow {
    pub share: String,
    pub path: String,
    pub day: i32,
    pub reads: u32,
    pub writes: u32,
    pub alloc_bytes: Option<u64>,
    pub flags: Vec<String>,
}

/// What one request resolves to, before any I/O. The `main.rs` tail turns this
/// into an archive write + a log line + an HTTP status.
#[derive(Debug)]
pub enum Disposition {
    /// First line is not a valid header, or a naming field is unsafe. Archive
    /// the raw body to `_malformed/` and respond 400. No trusted identity.
    Malformed { reason: String },
    /// Header is valid (and names are path-safe, so the body is archived to the
    /// real per-server path), but the row/footer framing is broken. 400.
    Rejected { header: Header, reason: String },
    /// Fully valid document. Archive to the per-server path and respond 200.
    /// Carries the parsed rows so the db layer ingests without re-parsing.
    Accepted { header: Header, rows: Vec<DumpRow> },
}

/// Classify a raw request body into a [`Disposition`] — the single pure entry
/// point. Order mirrors the spec: header parse, then untrusted-name validation,
/// then row/footer framing. Naming failures are bucketed as `Malformed` so an
/// unsafe `server`/`run_id` never reaches the archive path builder.
pub fn classify(body: &[u8]) -> Disposition {
    let text = match std::str::from_utf8(body) {
        Ok(t) => t,
        Err(_) => return Disposition::Malformed { reason: "body is not valid UTF-8".into() },
    };

    let lines = split_ndjson(text);
    let Some(first) = lines.first() else {
        return Disposition::Malformed { reason: "empty body".into() };
    };

    let header = match parse_header(first) {
        Ok(h) => h,
        Err(reason) => return Disposition::Malformed { reason },
    };

    if let Err(reason) = validate_naming(&header) {
        return Disposition::Malformed { reason };
    }

    match parse_body_lines(&lines[1..]) {
        Ok(rows) => Disposition::Accepted { header, rows },
        Err(reason) => Disposition::Rejected { header, reason },
    }
}

/// Split an NDJSON document into lines, dropping the single trailing empty
/// segment produced by a terminating newline. An empty line anywhere else is
/// preserved and will fail JSON parsing downstream (as it should).
pub fn split_ndjson(text: &str) -> Vec<&str> {
    let mut v: Vec<&str> = text.split('\n').collect();
    if v.last() == Some(&"") {
        v.pop();
    }
    v
}

/// Step 2: parse the first line and require `type == "header"` with the three
/// naming fields present and correctly typed. Returns the values verbatim —
/// untrusted until [`validate_naming`] vets them.
pub fn parse_header(line: &str) -> Result<Header, String> {
    let raw: RawHeader =
        serde_json::from_str(line).map_err(|e| format!("header line is not valid JSON: {e}"))?;
    if raw.kind != "header" {
        return Err(format!("first line type is {:?}, not \"header\"", raw.kind));
    }
    let server = raw.server.ok_or("header missing \"server\"")?;
    let run_id = raw.run_id.ok_or("header missing \"run_id\"")?;
    let dump_seq = raw.dump_seq.ok_or("header missing \"dump_seq\"")?;
    Ok(Header { server, run_id, dump_seq, emitted_at: raw.emitted_at })
}

/// Step 3: validate the naming fields *before* they are used as filesystem path
/// components (they come off the network). `server` must match
/// `^[a-z0-9][a-z0-9-]*$`, `run_id` must be a hyphenated UUID, `dump_seq >= 1`.
pub fn validate_naming(h: &Header) -> Result<(), String> {
    if !is_valid_server(&h.server) {
        return Err(format!("invalid server name {:?}", h.server));
    }
    if !is_valid_uuid(&h.run_id) {
        return Err(format!("invalid run_id {:?}", h.run_id));
    }
    if h.dump_seq < 1 {
        return Err(format!("dump_seq {} < 1", h.dump_seq));
    }
    Ok(())
}

/// `^[a-z0-9][a-z0-9-]*$` — hand-rolled so no regex crate is pulled in. Rejects
/// empty, leading `-`, uppercase, and anything with `/`, `\`, or `.` (so `..`,
/// path separators, and drive letters can never become a path component).
pub fn is_valid_server(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false, // empty, or a bad first character
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Hyphenated UUID check (8-4-4-4-12 hex groups). No `uuid` crate — we only
/// need to confirm the shape before slicing the first group for the filename.
pub fn is_valid_uuid(s: &str) -> bool {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != GROUPS.len() {
        return false;
    }
    parts
        .iter()
        .zip(GROUPS.iter())
        .all(|(p, &len)| p.len() == len && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Build the per-server archive path relative to the archive root:
/// `<server>/<first 8 chars of run_id>-<dump_seq>.ndjson`. Callers MUST have
/// passed `validate_naming` first; the run_id is then a valid UUID whose first
/// group is exactly 8 ASCII hex chars, so the slice is safe.
pub fn archive_rel_path(server: &str, run_id: &str, dump_seq: u32) -> String {
    let prefix = &run_id[..8];
    format!("{server}/{prefix}-{dump_seq}.ndjson")
}

/// One post-header line, fully parsed by `type`. An internally-tagged enum: a
/// line whose `type` is neither `row` nor `footer` fails to deserialize (unknown
/// variant), and a `row` line missing a required field fails too — both surface
/// as a rejection. This is a slight tightening over step 2 (which only checked
/// the `type` tag and framing): a malformed-but-typed row that step 2 accepted
/// now lands as `Rejected`. The archive location is unchanged (header is valid,
/// so still the per-server path); only the status flips 200 -> 400 for that
/// edge. Real agent dumps always carry complete rows.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum BodyLine {
    Row {
        share: String,
        path: String,
        day: i32,
        reads: u32,
        writes: u32,
        alloc_bytes: Option<u64>,
        #[serde(default)]
        flags: Vec<String>,
    },
    Footer {
        rows: usize,
    },
}

/// Step 5: parse + validate the post-header lines. Each must be a valid JSON
/// `row` or `footer`; there must be exactly one footer; it must be the last
/// line; and `footer.rows` must equal the number of row lines. Returns the
/// parsed rows on success. An empty slice (header with no footer) is a
/// missing-footer rejection; a lone `{"type":"footer","rows":0}` (empty dump)
/// yields an empty row vec.
pub fn parse_body_lines(lines: &[&str]) -> Result<Vec<DumpRow>, String> {
    let mut rows: Vec<DumpRow> = Vec::new();
    let mut footer_rows: Option<usize> = None;
    let n = lines.len();

    for (i, line) in lines.iter().enumerate() {
        // +2: 1-based line number, and the header was line 1.
        let lineno = i + 2;
        let parsed: BodyLine = serde_json::from_str(line)
            .map_err(|e| format!("line {lineno} is not a valid row/footer: {e}"))?;
        match parsed {
            BodyLine::Row {
                share,
                path,
                day,
                reads,
                writes,
                alloc_bytes,
                flags,
            } => {
                if footer_rows.is_some() {
                    return Err(format!("row on line {lineno} follows the footer"));
                }
                rows.push(DumpRow {
                    share,
                    path,
                    day,
                    reads,
                    writes,
                    alloc_bytes,
                    flags,
                });
            }
            BodyLine::Footer { rows: declared } => {
                if footer_rows.is_some() {
                    return Err("more than one footer".into());
                }
                if i != n - 1 {
                    return Err("footer is not the last line".into());
                }
                footer_rows = Some(declared);
            }
        }
    }

    match footer_rows {
        None => Err("missing footer".into()),
        Some(declared) if declared != rows.len() => Err(format!(
            "footer rows={declared} but counted {} row lines",
            rows.len()
        )),
        Some(_) => Ok(rows),
    }
}

/// First 8 characters of a run_id for logging (never used to build a path).
/// Tolerant of short input so a malformed value can still be logged.
pub fn run8(run_id: &str) -> String {
    run_id.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- header parse -----------------------------------------------------

    #[test]
    fn valid_header_accepted() {
        let line = r#"{"type":"header","server":"sgifs01","run_id":"5b08749f-1234-5678-9abc-def012345678","dump_seq":1,"emitted_at":"2026-06-11T00:00:00Z","walked_shares":["data"]}"#;
        let h = parse_header(line).expect("valid header");
        assert_eq!(h.server, "sgifs01");
        assert_eq!(h.dump_seq, 1);
    }

    #[test]
    fn header_missing_field_rejected() {
        // No "server".
        let line = r#"{"type":"header","run_id":"5b08749f-1234-5678-9abc-def012345678","dump_seq":1}"#;
        assert!(parse_header(line).is_err());
    }

    #[test]
    fn header_wrong_type_rejected() {
        // dump_seq is a string, not a number.
        let line = r#"{"type":"header","server":"sgifs01","run_id":"5b08749f-1234-5678-9abc-def012345678","dump_seq":"1"}"#;
        assert!(parse_header(line).is_err());
    }

    #[test]
    fn non_header_first_line_rejected() {
        let line = r#"{"type":"row","share":"data","path":"a.txt","day":20613,"reads":1,"writes":0,"alloc_bytes":10,"flags":[]}"#;
        assert!(parse_header(line).is_err());
    }

    // ---- server-name validation -------------------------------------------

    #[test]
    fn server_name_validation() {
        assert!(is_valid_server("sgifs01"));
        assert!(is_valid_server("sgi-backup"));
        assert!(!is_valid_server("SGIFS01")); // uppercase
        assert!(!is_valid_server("a/b")); // path separator
        assert!(!is_valid_server("..")); // parent-dir traversal
        assert!(!is_valid_server("")); // empty
        assert!(!is_valid_server("-lead")); // leading hyphen
        assert!(!is_valid_server("a\\b")); // backslash
    }

    #[test]
    fn uuid_validation() {
        assert!(is_valid_uuid("5b08749f-1234-5678-9abc-def012345678"));
        assert!(!is_valid_uuid("5b08749f12345678")); // no hyphens
        assert!(!is_valid_uuid("5b08749f-1234-5678-9abc-def01234567")); // last group short
        assert!(!is_valid_uuid("5b08749g-1234-5678-9abc-def012345678")); // non-hex 'g'
    }

    // ---- archive naming ---------------------------------------------------

    #[test]
    fn archive_naming() {
        let p = archive_rel_path("sgi-backup", "5b08749f-1234-5678-9abc-def012345678", 3);
        assert_eq!(p, "sgi-backup/5b08749f-3.ndjson");
    }

    // ---- footer / framing -------------------------------------------------

    fn row(p: &str) -> String {
        format!(r#"{{"type":"row","share":"data","path":"{p}","day":20613,"reads":1,"writes":0,"alloc_bytes":10,"flags":[]}}"#)
    }

    #[test]
    fn rows_mismatch_rejected() {
        let r = row("a.txt");
        let lines = vec![r.as_str(), r#"{"type":"footer","rows":2}"#];
        assert!(parse_body_lines(&lines).is_err());
    }

    #[test]
    fn footer_not_last_rejected() {
        let r = row("a.txt");
        let lines = vec![r#"{"type":"footer","rows":1}"#, r.as_str()];
        assert!(parse_body_lines(&lines).is_err());
    }

    #[test]
    fn missing_footer_rejected() {
        let r = row("a.txt");
        let lines = vec![r.as_str()];
        assert!(parse_body_lines(&lines).is_err());
    }

    #[test]
    fn empty_dump_accepted() {
        // rows == 0 with no row lines is a legal empty dump.
        let lines = vec![r#"{"type":"footer","rows":0}"#];
        assert!(parse_body_lines(&lines).unwrap().is_empty());
    }

    #[test]
    fn matching_rows_parsed() {
        let r1 = row("a.txt");
        let r2 = row("b.txt");
        let lines = vec![r1.as_str(), r2.as_str(), r#"{"type":"footer","rows":2}"#];
        let parsed = parse_body_lines(&lines).unwrap();
        assert_eq!(parsed.len(), 2);
        // Fields round-trip from the JSON row helper.
        assert_eq!(parsed[0].path, "a.txt");
        assert_eq!(parsed[0].day, 20613);
        assert_eq!(parsed[0].reads, 1);
        assert_eq!(parsed[0].alloc_bytes, Some(10));
    }

    #[test]
    fn invalid_json_row_rejected() {
        let lines = vec!["{not json", r#"{"type":"footer","rows":1}"#];
        assert!(parse_body_lines(&lines).is_err());
    }

    #[test]
    fn null_alloc_and_flags_parse() {
        // A restat-style row: alloc_bytes null, a flag present.
        let line = r#"{"type":"row","share":"data","path":"x.txt","day":1,"reads":1,"writes":0,"alloc_bytes":null,"flags":["restat"]}"#;
        let lines = vec![line, r#"{"type":"footer","rows":1}"#];
        let parsed = parse_body_lines(&lines).unwrap();
        assert_eq!(parsed[0].alloc_bytes, None);
        assert_eq!(parsed[0].flags, vec!["restat".to_string()]);
    }

    // ---- end-to-end classify (still pure: bytes in, Disposition out) -------

    #[test]
    fn classify_accepts_well_formed_document() {
        let body = format!(
            "{}\n{}\n{}\n",
            r#"{"type":"header","server":"sgifs01","run_id":"5b08749f-1234-5678-9abc-def012345678","dump_seq":1}"#,
            row("a.txt"),
            r#"{"type":"footer","rows":1}"#,
        );
        match classify(body.as_bytes()) {
            Disposition::Accepted { header, rows } => {
                assert_eq!(header.server, "sgifs01");
                assert_eq!(rows.len(), 1);
            }
            other => panic!("expected Accepted, got {other:?}"),
        }
    }

    #[test]
    fn classify_buckets_unsafe_server_as_malformed() {
        let body = format!(
            "{}\n{}\n",
            r#"{"type":"header","server":"../etc","run_id":"5b08749f-1234-5678-9abc-def012345678","dump_seq":1}"#,
            r#"{"type":"footer","rows":0}"#,
        );
        assert!(matches!(classify(body.as_bytes()), Disposition::Malformed { .. }));
    }

    #[test]
    fn classify_rejects_bad_framing_after_valid_header() {
        let body = format!(
            "{}\n{}\n",
            r#"{"type":"header","server":"sgifs01","run_id":"5b08749f-1234-5678-9abc-def012345678","dump_seq":1}"#,
            r#"{"type":"footer","rows":5}"#, // claims 5 rows, zero present
        );
        assert!(matches!(classify(body.as_bytes()), Disposition::Rejected { .. }));
    }
}
