//! The correlation engine — per-file heat counting with deferred share naming.
//!
//! Two tables:
//!   * `shares`: ShareGUID -> ShareName, learned from event 600
//!     (Smb2TreeConnectAllocate, which carries both per SCHEMA).
//!   * `heat`:   (ShareGUID, normalized path) -> the set of distinct OpenGUIDs
//!     observed against that file.
//!
//! The heat table is keyed on the open's **ShareGUID**, not the share name, and
//! names are resolved only at emission time. This means a 600 seen at ANY point
//! in the run names every open for that ShareGUID — including opens that arrived
//! before the binding 600 (the common pre-existing-mount case). A ShareGUID that
//! no 600 ever binds emits as UNKNOWN.
//!
//! Identity is intentionally out of scope here — no 500/552 user/client rejoin.
//! Pure and unit-tested; no ETW dependency.

use std::collections::{HashMap, HashSet};

use crate::events::{fmt_guid_key, GuidKey, SmbEvent};

/// Heat key: the open's ShareGUID (resolved to a name only at output time) plus
/// the case-folded path. Keying on ShareGUID — not the name — is what lets a
/// late 600 name opens that arrived before it.
type HeatKey = (Option<GuidKey>, String);

/// Emitted for a ShareGUID that no 600 has bound (yet / ever). The open is still
/// counted, never dropped.
const UNKNOWN_SHARE: &str = "UNKNOWN";

/// Format an optional ShareGUID for display.
fn fmt_share_guid(g: Option<GuidKey>) -> String {
    g.map(fmt_guid_key).unwrap_or_else(|| "<none>".to_string())
}

/// A single open as emitted live (one line per 650). `share` is resolved as
/// currently known; the authoritative names come from `resolved_summary`, which
/// re-resolves once all 600s have been seen.
#[derive(Debug, Clone)]
pub struct ResolvedAccess {
    pub share_guid: Option<GuidKey>,
    pub share: String,
    pub path: String,
    pub tree: GuidKey,
    pub open: GuidKey,
    pub access: u32,
    pub opens_on_key: u64,
}

impl std::fmt::Display for ResolvedAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}\\{}  share_guid={} open={} tree={} access=0x{:08X} [{} opens]",
            self.share,
            self.path,
            fmt_share_guid(self.share_guid),
            fmt_guid_key(self.open),
            fmt_guid_key(self.tree),
            self.access,
            self.opens_on_key,
        )
    }
}

/// One aggregated, name-resolved row of the heat table.
#[derive(Debug, Clone)]
pub struct HeatRow {
    pub share_guid: Option<GuidKey>,
    pub share: String,
    pub path: String,
    pub opens: u64,
}

impl std::fmt::Display for HeatRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:>6} opens  {}\\{}  [share_guid={}]",
            self.opens,
            self.share,
            self.path,
            fmt_share_guid(self.share_guid),
        )
    }
}

#[derive(Default)]
pub struct CorrelationEngine {
    /// ShareGUID -> ShareName, learned from 600 (Smb2TreeConnectAllocate).
    shares: HashMap<GuidKey, String>,
    /// (ShareGUID, normalized path) -> distinct OpenGUIDs counted there.
    heat: HashMap<HeatKey, HashSet<GuidKey>>,

    pub opens_total: u64,
}

impl CorrelationEngine {
    /// Ingest one normalized event. Returns `Some` for every open (the access
    /// pulse), carrying the share name as known at that instant.
    pub fn apply(&mut self, ev: &SmbEvent) -> Option<ResolvedAccess> {
        match ev {
            SmbEvent::TreeConnect { share_guid, share } => {
                // Binding can arrive at any time; because heat is keyed on
                // ShareGUID, this retroactively names earlier opens too.
                self.shares.insert(*share_guid, share.clone());
                None
            }
            SmbEvent::Open { open, tree, share_guid, path, access } => {
                self.opens_total += 1;

                // Fold case so casing-only path variants collapse; key by
                // ShareGUID so naming can be deferred to output time.
                let opens_on_key = {
                    let seen = self.heat.entry((*share_guid, path.to_lowercase())).or_default();
                    seen.insert(*open); // distinct OpenGUIDs; repeats count once
                    seen.len() as u64
                };

                Some(ResolvedAccess {
                    share_guid: *share_guid,
                    share: self.resolve_name(*share_guid),
                    path: path.clone(),
                    tree: *tree,
                    open: *open,
                    access: *access,
                    opens_on_key,
                })
            }
        }
    }

    /// Resolve a ShareGUID to its ShareName as currently known, or `UNKNOWN`.
    fn resolve_name(&self, share_guid: Option<GuidKey>) -> String {
        share_guid
            .and_then(|g| self.shares.get(&g))
            .cloned()
            .unwrap_or_else(|| UNKNOWN_SHARE.to_string())
    }

    /// The aggregated heat table, names resolved at call time (so a 600 seen
    /// since the opens accrued now names them). Hottest first, with a stable
    /// tiebreak for deterministic output.
    pub fn resolved_summary(&self) -> Vec<HeatRow> {
        let mut rows: Vec<HeatRow> = self
            .heat
            .iter()
            .map(|((sg, path), opens)| HeatRow {
                share_guid: *sg,
                share: self.resolve_name(*sg),
                path: path.clone(),
                opens: opens.len() as u64,
            })
            .collect();
        rows.sort_by(|a, b| {
            b.opens
                .cmp(&a.opens)
                .then_with(|| a.share.cmp(&b.share))
                .then_with(|| a.path.cmp(&b.path))
        });
        rows
    }

    /// The resolved heat table rendered for emission (periodic + final dump).
    pub fn resolved_table(&self) -> String {
        let rows = self.resolved_summary();
        let mut out = format!(
            "=== resolved heat: {} keys, {} opens ===\n",
            rows.len(),
            self.opens_total
        );
        for r in &rows {
            out.push_str(&r.to_string());
            out.push('\n');
        }
        out
    }

    /// Distinct opens counted for a `(share_guid, path)` key. `path` is
    /// case-folded to match how the key was stored.
    #[allow(dead_code)] // query API: used by tests now, reporting later
    pub fn opens_for(&self, share_guid: Option<GuidKey>, path: &str) -> usize {
        self.heat
            .get(&(share_guid, path.to_lowercase()))
            .map_or(0, HashSet::len)
    }

    /// Number of distinct `(share_guid, path)` heat keys tracked.
    #[allow(dead_code)] // query API: used by tests now, reporting later
    pub fn distinct_keys(&self) -> usize {
        self.heat.len()
    }

    pub fn stats_line(&self) -> String {
        let named = self
            .heat
            .keys()
            .filter(|(sg, _)| sg.is_some_and(|g| self.shares.contains_key(&g)))
            .count();
        format!(
            "opens: {} total | keys: {} ({} named, {} UNKNOWN) | shares bound: {}",
            self.opens_total,
            self.heat.len(),
            named,
            self.heat.len() - named,
            self.shares.len(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Opaque u128 stand-ins for the production GUID keys.
    const SHARE_DATA: GuidKey = 0x0000_0000_0000_0000_0000_0000_0000_0AAA;
    const SHARE_HOME: GuidKey = 0x0000_0000_0000_0000_0000_0000_0000_0BBB;
    const TREE_1: GuidKey = 0x0000_0000_0000_0000_0000_0000_0000_0010;
    const OPEN_1: GuidKey = 0x0000_0000_0000_0000_0000_0000_0000_0001;
    const OPEN_2: GuidKey = 0x0000_0000_0000_0000_0000_0000_0000_0002;

    fn open(open: GuidKey, share_guid: Option<GuidKey>, path: &str) -> SmbEvent {
        SmbEvent::Open { open, tree: TREE_1, share_guid, path: path.into(), access: 0x0012_0089 }
    }

    fn row_for<'a>(rows: &'a [HeatRow], path: &str) -> &'a HeatRow {
        rows.iter().find(|r| r.path == path).expect("row present")
    }

    #[test]
    fn share_name_resolves_at_output_via_shareguid() {
        let mut e = CorrelationEngine::default();
        e.apply(&SmbEvent::TreeConnect { share_guid: SHARE_DATA, share: "DATA".into() });
        let r = e
            .apply(&open(OPEN_1, Some(SHARE_DATA), "\\projects\\q3.xlsx"))
            .expect("an open is always emitted");
        assert_eq!(r.share, "DATA");
        assert_eq!(r.share_guid, Some(SHARE_DATA));
    }

    #[test]
    fn late_600_names_opens_that_arrived_before_it() {
        let mut e = CorrelationEngine::default();
        // Pre-existing mount: the open arrives BEFORE its binding 600.
        let early = e
            .apply(&open(OPEN_1, Some(SHARE_DATA), "\\pre\\mount.db"))
            .unwrap();
        assert_eq!(early.share, "UNKNOWN"); // not yet bound at open time

        // The binding 600 shows up later in the run.
        e.apply(&SmbEvent::TreeConnect { share_guid: SHARE_DATA, share: "DATA".into() });

        // The aggregated summary now names that earlier open.
        let rows = e.resolved_summary();
        let row = row_for(&rows, "\\pre\\mount.db");
        assert_eq!(row.share, "DATA");
        assert_eq!(row.share_guid, Some(SHARE_DATA));
        assert_eq!(row.opens, 1);
    }

    #[test]
    fn unbound_shareguid_emits_unknown_and_is_not_dropped() {
        let mut e = CorrelationEngine::default();
        // No 600 ever binds SHARE_HOME.
        let r = e
            .apply(&open(OPEN_1, Some(SHARE_HOME), "\\x"))
            .expect("an unbound share must NOT drop the open");
        assert_eq!(r.share, "UNKNOWN");
        assert_eq!(e.opens_for(Some(SHARE_HOME), "\\x"), 1);
        let rows = e.resolved_summary();
        assert_eq!(row_for(&rows, "\\x").share, "UNKNOWN");
    }

    #[test]
    fn casing_only_paths_collapse_and_distinct_opens_count_separately() {
        let mut e = CorrelationEngine::default();
        e.apply(&SmbEvent::TreeConnect { share_guid: SHARE_DATA, share: "DATA".into() });
        // Two DISTINCT opens whose Name differs only in case.
        e.apply(&open(OPEN_1, Some(SHARE_DATA), "\\Reports\\Q3.XLSX"));
        let r2 = e
            .apply(&open(OPEN_2, Some(SHARE_DATA), "\\reports\\q3.xlsx"))
            .unwrap();
        // (a) the casing-only variants collapse to a single heat key.
        assert_eq!(e.distinct_keys(), 1);
        // (b) the two distinct OpenGUIDs count as 2 against that key.
        assert_eq!(e.opens_for(Some(SHARE_DATA), "\\reports\\q3.xlsx"), 2);
        assert_eq!(r2.opens_on_key, 2);
    }

    #[test]
    fn repeated_openguid_counts_once() {
        let mut e = CorrelationEngine::default();
        e.apply(&SmbEvent::TreeConnect { share_guid: SHARE_DATA, share: "DATA".into() });
        e.apply(&open(OPEN_1, Some(SHARE_DATA), "\\a\\b.txt"));
        let r = e.apply(&open(OPEN_1, Some(SHARE_DATA), "\\A\\B.TXT")).unwrap();
        assert_eq!(r.opens_on_key, 1);
        assert_eq!(e.opens_for(Some(SHARE_DATA), "\\a\\b.txt"), 1);
    }

    #[test]
    fn distinct_shares_are_separate_keys() {
        let mut e = CorrelationEngine::default();
        e.apply(&SmbEvent::TreeConnect { share_guid: SHARE_DATA, share: "DATA".into() });
        e.apply(&SmbEvent::TreeConnect { share_guid: SHARE_HOME, share: "HOME".into() });
        e.apply(&open(OPEN_1, Some(SHARE_DATA), "\\f"));
        e.apply(&open(OPEN_2, Some(SHARE_HOME), "\\f"));
        assert_eq!(e.distinct_keys(), 2);
    }
}
