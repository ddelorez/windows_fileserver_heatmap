//! The correlation engine — share-name resolution + per-file heat counting.
//!
//! It keeps two tables:
//!   * `shares`: ShareGUID -> ShareName, learned from event 600
//!     (Smb2TreeConnectAllocate, which carries both per SCHEMA).
//!   * `heat`:   (ShareName, normalized path) -> the set of distinct OpenGUIDs
//!     observed against that file.
//!
//! Each Smb2FileOpen (650) resolves its share via its OWN ShareGUID (the old
//! TreeConnectGUID -> ShareName indirection is retired) and is counted against
//! the (share, lowercased-path) heat key. Casing-only path variants collapse to
//! one key; each distinct OpenGUID counts as one open.
//!
//! Identity is intentionally out of scope here — there is no 500/552 user or
//! client rejoin. Pure and unit-tested; no ETW dependency.

use std::collections::{HashMap, HashSet};

use crate::events::{fmt_guid_key, GuidKey, SmbEvent};

/// The per-file heat key: resolved share name + case-folded path.
type HeatKey = (String, String);

/// Emitted for an open whose ShareGUID we haven't seen a 600 for. The open is
/// still counted (never dropped), just bucketed under this sentinel share.
const UNKNOWN_SHARE: &str = "UNKNOWN";

/// A single resolved open — what the engine hands back per 650.
#[derive(Debug, Clone)]
pub struct ResolvedAccess {
    pub share: String,
    pub path: String,
    pub tree: GuidKey,
    pub open: GuidKey,
    pub access: u32,
    /// Distinct opens counted so far against this open's (share, path) key.
    pub opens_on_key: u64,
}

impl std::fmt::Display for ResolvedAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}\\{}  (open {}, tree {}) access=0x{:08X}  [{} opens]",
            self.share,
            self.path,
            fmt_guid_key(self.open),
            fmt_guid_key(self.tree),
            self.access,
            self.opens_on_key,
        )
    }
}

#[derive(Default)]
pub struct CorrelationEngine {
    /// ShareGUID -> ShareName, learned from 600 (Smb2TreeConnectAllocate).
    shares: HashMap<GuidKey, String>,
    /// (ShareName, normalized path) -> distinct OpenGUIDs counted there.
    heat: HashMap<HeatKey, HashSet<GuidKey>>,

    // Spike metrics — the resolved/unknown ratio is how you judge success.
    pub opens_total: u64,
    pub opens_resolved: u64,
    pub opens_unresolved: u64,
}

impl CorrelationEngine {
    /// Ingest one normalized event. Returns `Some` for every open (the access
    /// pulse), whether or not its share resolved.
    pub fn apply(&mut self, ev: &SmbEvent) -> Option<ResolvedAccess> {
        match ev {
            SmbEvent::TreeConnect { share_guid, share } => {
                self.shares.insert(*share_guid, share.clone());
                None
            }
            SmbEvent::Open { open, tree, share_guid, path, access } => {
                self.resolve(*open, *tree, *share_guid, path, *access)
            }
        }
    }

    /// Resolve an open's share by its OWN ShareGUID, then count it against the
    /// (share, normalized-path) heat key. Unknown shares are emitted as
    /// `UNKNOWN` and still counted — never dropped.
    fn resolve(
        &mut self,
        open: GuidKey,
        tree: GuidKey,
        share_guid: Option<GuidKey>,
        path: &str,
        access: u32,
    ) -> Option<ResolvedAccess> {
        self.opens_total += 1;

        let share = match share_guid.and_then(|g| self.shares.get(&g)) {
            Some(name) => {
                self.opens_resolved += 1;
                name.clone()
            }
            None => {
                self.opens_unresolved += 1;
                UNKNOWN_SHARE.to_string()
            }
        };

        // Fold case into the key so casing-only path variants collapse to one
        // bucket; insert by OpenGUID so distinct opens count once each (and a
        // repeated OpenGUID does not inflate the count).
        let key: HeatKey = (share.clone(), path.to_lowercase());
        let seen = self.heat.entry(key).or_default();
        seen.insert(open);
        let opens_on_key = seen.len() as u64;

        Some(ResolvedAccess {
            share,
            path: path.to_string(),
            tree,
            open,
            access,
            opens_on_key,
        })
    }

    // Heat-query API: exercised by the unit tests now and by the heat-reporting
    // step later. No production consumer reads the map yet, hence the allow.
    /// Distinct opens counted for a `(share, path)` key. `path` is case-folded
    /// to match how the key was stored.
    #[allow(dead_code)]
    pub fn opens_for(&self, share: &str, path: &str) -> usize {
        self.heat
            .get(&(share.to_string(), path.to_lowercase()))
            .map_or(0, HashSet::len)
    }

    /// Number of distinct `(share, path)` heat keys tracked.
    #[allow(dead_code)]
    pub fn distinct_keys(&self) -> usize {
        self.heat.len()
    }

    pub fn stats_line(&self) -> String {
        format!(
            "opens: {} total, {} resolved, {} unknown-share | live: {} shares, {} heat keys",
            self.opens_total,
            self.opens_resolved,
            self.opens_unresolved,
            self.shares.len(),
            self.heat.len(),
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

    #[test]
    fn share_resolves_via_its_own_shareguid() {
        let mut e = CorrelationEngine::default();
        e.apply(&SmbEvent::TreeConnect { share_guid: SHARE_DATA, share: "DATA".into() });
        let r = e
            .apply(&open(OPEN_1, Some(SHARE_DATA), "\\projects\\q3.xlsx"))
            .expect("an open is always emitted");
        assert_eq!(r.share, "DATA");
        assert_eq!(e.opens_resolved, 1);
        assert_eq!(e.opens_unresolved, 0);
    }

    #[test]
    fn unknown_shareguid_emits_unknown_and_is_not_dropped() {
        let mut e = CorrelationEngine::default();
        // No 600 seen for this ShareGUID.
        let r = e
            .apply(&open(OPEN_1, Some(SHARE_HOME), "\\x"))
            .expect("an unknown share must NOT drop the open");
        assert_eq!(r.share, "UNKNOWN");
        assert_eq!(e.opens_unresolved, 1);
        assert_eq!(e.opens_for("UNKNOWN", "\\x"), 1);
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
        assert_eq!(e.opens_for("DATA", "\\reports\\q3.xlsx"), 2);
        assert_eq!(r2.opens_on_key, 2);
    }

    #[test]
    fn repeated_openguid_counts_once() {
        let mut e = CorrelationEngine::default();
        e.apply(&SmbEvent::TreeConnect { share_guid: SHARE_DATA, share: "DATA".into() });
        e.apply(&open(OPEN_1, Some(SHARE_DATA), "\\a\\b.txt"));
        let r = e.apply(&open(OPEN_1, Some(SHARE_DATA), "\\A\\B.TXT")).unwrap();
        // Same OpenGUID seen twice is still one distinct open.
        assert_eq!(r.opens_on_key, 1);
        assert_eq!(e.opens_for("DATA", "\\a\\b.txt"), 1);
    }

    #[test]
    fn distinct_shares_are_separate_keys() {
        let mut e = CorrelationEngine::default();
        e.apply(&SmbEvent::TreeConnect { share_guid: SHARE_DATA, share: "DATA".into() });
        e.apply(&SmbEvent::TreeConnect { share_guid: SHARE_HOME, share: "HOME".into() });
        e.apply(&open(OPEN_1, Some(SHARE_DATA), "\\f"));
        e.apply(&open(OPEN_2, Some(SHARE_HOME), "\\f"));
        // Same path under different shares are different keys.
        assert_eq!(e.distinct_keys(), 2);
    }
}
