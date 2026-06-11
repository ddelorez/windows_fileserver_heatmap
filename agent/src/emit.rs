//! NDJSON framing for one flush's resolved heat snapshot.
//!
//! One document per flush, newline-delimited JSON:
//!   * a `header` line  — run-scoped identity + this dump's seq/timestamp,
//!   * one `row` line per (share, path, day), cumulative since run start,
//!   * a `footer` line  — the row count.
//!
//! The line shapes are an internally-tagged enum keyed on `"type"`, so serde
//! emits `{"type":"header",...}` / `{"type":"row",...}` / `{"type":"footer",...}`
//! with the tag first and the remaining keys in declaration order — matching the
//! documented shape. `alloc_bytes` is `Option<u64>` with NO skip attribute, so
//! `None` serializes as JSON `null` (never omitted), as required.
//!
//! This module owns the JSON only; `CorrelationEngine::emit_rows` produces the
//! domain rows (and is where the kept/dropped bridge lives).

use serde::Serialize;

use crate::correlation::EmitRow;

/// Run-scoped header metadata, owned by the flusher thread (not the engine):
/// `dump_seq` increments every flush, `emitted_at` is stamped per flush, and
/// `run_id`/`server` are minted once at trace start.
pub struct Meta<'a> {
    pub server: &'a str,
    pub run_id: &'a str,
    pub dump_seq: u32,
    pub emitted_at: &'a str,
    /// Walked-share allowlist (lowercased); `document` sorts it for the header.
    pub walked_shares: &'a [String],
}

/// One NDJSON line. `#[serde(tag = "type")]` makes this internally tagged; the
/// `rename_all` lowercases the variant name into the `"type"` value.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Line<'a> {
    Header {
        server: &'a str,
        run_id: &'a str,
        dump_seq: u32,
        emitted_at: &'a str,
        walked_shares: &'a [String],
    },
    Row {
        share: &'a str,
        path: &'a str,
        day: i32,
        reads: u32,
        writes: u32,
        alloc_bytes: Option<u64>,
        flags: &'a [&'a str],
    },
    Footer {
        rows: usize,
    },
}

/// Serialize one flush's snapshot as an NDJSON document: header line, one row
/// line per `EmitRow`, then a footer with the row count. An empty `rows` still
/// emits header + `{"type":"footer","rows":0}`. A trailing newline keeps
/// concatenated dumps line-delimited.
pub fn document(meta: &Meta, rows: &[EmitRow]) -> String {
    // Header walked_shares are sorted here so the contract holds regardless of
    // the order the caller pulled them out of the (unordered) allowlist set.
    let mut walked = meta.walked_shares.to_vec();
    walked.sort();

    let mut out = String::new();
    push_line(
        &mut out,
        &Line::Header {
            server: meta.server,
            run_id: meta.run_id,
            dump_seq: meta.dump_seq,
            emitted_at: meta.emitted_at,
            walked_shares: &walked,
        },
    );
    for r in rows {
        push_line(
            &mut out,
            &Line::Row {
                share: &r.share,
                path: &r.path,
                day: r.day,
                reads: r.reads,
                writes: r.writes,
                alloc_bytes: r.alloc_bytes,
                flags: &r.flags,
            },
        );
    }
    push_line(&mut out, &Line::Footer { rows: rows.len() });
    out
}

/// Append one serialized line + `\n`. Our `Line` has only string/number/array
/// fields, so `to_string` cannot actually fail here — but we log rather than
/// panic so a framing surprise never takes down a live capture.
fn push_line(out: &mut String, line: &Line) {
    match serde_json::to_string(line) {
        Ok(s) => {
            out.push_str(&s);
            out.push('\n');
        }
        Err(e) => eprintln!("emit: serialize failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta<'a>(walked: &'a [String]) -> Meta<'a> {
        Meta {
            server: "sgi-backup",
            run_id: "0123abcd-0000-0000-0000-000000000000",
            dump_seq: 3,
            emitted_at: "2026-06-10T00:00:00+00:00",
            walked_shares: walked,
        }
    }

    fn leaf_row(path: &str, day: i32, reads: u32, writes: u32, alloc: u64) -> EmitRow {
        EmitRow {
            share: "heattest".into(),
            path: path.into(),
            day,
            reads,
            writes,
            alloc_bytes: Some(alloc),
            flags: vec![],
        }
    }

    // Split a document into its lines, dropping the trailing empty from the
    // final newline.
    fn lines(doc: &str) -> Vec<serde_json::Value> {
        doc.lines()
            .map(|l| serde_json::from_str(l).expect("each line is valid JSON"))
            .collect()
    }

    #[test]
    fn backslash_in_path_is_json_escaped() {
        let rows = [leaf_row("folder1\\txtdoc1.txt", 20613, 2, 0, 4096)];
        let doc = document(&meta(&["heattest".into()]), &rows);
        // Raw document text carries an escaped backslash (\\), not a literal one.
        assert!(doc.contains(r#""path":"folder1\\txtdoc1.txt""#));
        // And it round-trips back to the single-backslash value.
        let parsed = lines(&doc);
        assert_eq!(parsed[1]["path"], "folder1\\txtdoc1.txt");
    }

    #[test]
    fn header_walked_shares_are_sorted() {
        let walked = vec!["zeta".to_string(), "alpha".to_string(), "mike".to_string()];
        let doc = document(&meta(&walked), &[]);
        let parsed = lines(&doc);
        assert_eq!(parsed[0]["type"], "header");
        assert_eq!(parsed[0]["walked_shares"][0], "alpha");
        assert_eq!(parsed[0]["walked_shares"][1], "mike");
        assert_eq!(parsed[0]["walked_shares"][2], "zeta");
    }

    #[test]
    fn empty_table_emits_header_and_zero_footer() {
        let doc = document(&meta(&["heattest".into()]), &[]);
        let parsed = lines(&doc);
        assert_eq!(parsed.len(), 2); // header + footer only
        assert_eq!(parsed[0]["type"], "header");
        assert_eq!(parsed[1]["type"], "footer");
        assert_eq!(parsed[1]["rows"], 0);
    }

    #[test]
    fn footer_count_matches_row_lines() {
        let rows = [
            leaf_row("a.txt", 20613, 1, 0, 10),
            leaf_row("b.txt", 20613, 0, 1, 20),
            leaf_row("c.txt", 20614, 2, 2, 30),
        ];
        let doc = document(&meta(&["heattest".into()]), &rows);
        let parsed = lines(&doc);
        let row_lines = parsed.iter().filter(|v| v["type"] == "row").count();
        assert_eq!(row_lines, 3);
        let footer = parsed.last().unwrap();
        assert_eq!(footer["type"], "footer");
        assert_eq!(footer["rows"], row_lines);
    }

    #[test]
    fn null_alloc_bytes_serializes_as_json_null_with_flag() {
        let restat = EmitRow {
            share: "heattest".into(),
            path: "missing.txt".into(),
            day: 20614,
            reads: 1,
            writes: 0,
            alloc_bytes: None,
            flags: vec!["restat"],
        };
        let doc = document(&meta(&["heattest".into()]), &[restat]);
        // Literal JSON null, not an omitted key.
        assert!(doc.contains(r#""alloc_bytes":null"#));
        let parsed = lines(&doc);
        assert!(parsed[1]["alloc_bytes"].is_null());
        assert_eq!(parsed[1]["flags"][0], "restat");
    }

    #[test]
    fn header_tag_is_first_and_type_values_are_correct() {
        let doc = document(&meta(&["heattest".into()]), &[leaf_row("a.txt", 1, 1, 0, 8)]);
        // The internally-tagged enum puts "type" first on every line.
        for line in doc.lines() {
            assert!(line.starts_with(r#"{"type":"#), "tag must lead: {line}");
        }
        let parsed = lines(&doc);
        assert_eq!(parsed[0]["type"], "header");
        assert_eq!(parsed[1]["type"], "row");
        assert_eq!(parsed[2]["type"], "footer");
    }
}
