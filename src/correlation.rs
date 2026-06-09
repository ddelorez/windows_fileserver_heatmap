//! The correlation engine — per-file read/write demand with deferred share naming.
//!
//! Two tables:
//!   * `shares`: ShareGUID -> ShareName, learned from event 600
//!     (Smb2TreeConnectAllocate, which carries both per SCHEMA).
//!   * `heat`:   (ShareGUID, normalized path) -> a sparse per-day read/write
//!     count (`BTreeMap<day_index, DayCount>`).
//!
//! The heat table is keyed on the open's **ShareGUID**, not the share name, and
//! names are resolved only at emission time. This means a 600 seen at ANY point
//! in the run names every open for that ShareGUID — including opens that arrived
//! before the binding 600 (the common pre-existing-mount case). A ShareGUID that
//! no 600 ever binds emits as UNKNOWN.
//!
//! Opens are NOT deduplicated: this provider emits one OpenGUID per open (a
//! capture showed 115 opens / 115 distinct OpenGUIDs), and path-casing collapse
//! is already handled by the lowercased key — so no per-key OpenGUID set is kept.
//! The demand signal is per-day read/write counts; metadata-only opens are
//! dropped (counted in a global diagnostic).
//!
//! Identity is intentionally out of scope here — no 500/552 user/client rejoin.
//! Pure and unit-tested; no ETW dependency.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, NaiveDate};
use chrono_tz::America::Chicago;

use crate::events::{fmt_guid_key, GuidKey, SmbEvent};
use crate::inventory::Inventory;

/// Heat key: the open's ShareGUID (resolved to a name only at output time) plus
/// the case-folded path. Keying on ShareGUID — not the name — is what lets a
/// late 600 name opens that arrived before it.
type HeatKey = (Option<GuidKey>, String);

/// Emitted for a ShareGUID that no 600 has bound (yet / ever). The open is still
/// counted, never dropped.
const UNKNOWN_SHARE: &str = "UNKNOWN";

/// Seconds between the FILETIME epoch (1601-01-01 UTC) and the Unix epoch.
const FILETIME_TO_UNIX_SECS: i64 = 11_644_473_600;

// DesiredAccess bits (Win32). Precedence is locked: any write bit -> Write,
// else read bit -> Read, else Metadata.
const FILE_READ_DATA: u32 = 0x1;
const FILE_WRITE_DATA: u32 = 0x2;
const FILE_APPEND_DATA: u32 = 0x4;
const WRITE_MASK: u32 = FILE_WRITE_DATA | FILE_APPEND_DATA; // 0x6

/// How an open touches file data, per its DesiredAccess mask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Metadata,
}

/// Classify a DesiredAccess mask. Write wins over read (an EC read-modify-write
/// is the expensive case we don't want masked by a co-set read bit); a pure
/// metadata/attribute open touches no data.
pub fn classify_access(desired: u32) -> Access {
    if desired & WRITE_MASK != 0 {
        Access::Write
    } else if desired & FILE_READ_DATA != 0 {
        Access::Read
    } else {
        Access::Metadata
    }
}

/// Convert a ferrisetw `raw_timestamp()` (FILETIME: 100 ns ticks since
/// 1601-01-01 UTC) to a day index = days since the Unix epoch in
/// America/Chicago civil (DST-aware) time.
pub fn central_civil_day(filetime_100ns: i64) -> i32 {
    let unix_secs = filetime_100ns / 10_000_000 - FILETIME_TO_UNIX_SECS;
    let utc = DateTime::from_timestamp(unix_secs, 0).unwrap_or_default();
    let local_date = utc.with_timezone(&Chicago).date_naive();
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid epoch date");
    local_date.signed_duration_since(epoch).num_days() as i32
}

/// Per-day read/write counts for one file. Sparse: only days with activity exist.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DayCount {
    pub reads: u32,
    pub writes: u32,
}

/// Format an optional ShareGUID for display.
fn fmt_share_guid(g: Option<GuidKey>) -> String {
    g.map(fmt_guid_key).unwrap_or_else(|| "<none>".to_string())
}

/// A single accepted (read/write) open as emitted live (one line per 650).
/// `share` is resolved as currently known; authoritative names come from
/// `resolved_summary`, which re-resolves once all 600s have been seen.
#[derive(Debug, Clone)]
pub struct ResolvedAccess {
    pub share_guid: Option<GuidKey>,
    pub share: String,
    pub path: String,
    pub tree: GuidKey,
    pub open: GuidKey,
    pub access: u32,
    pub kind: Access,
    pub day: i32,
}

impl std::fmt::Display for ResolvedAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}\\{}  share_guid={} open={} tree={} access=0x{:08X} {:?} day={}",
            self.share,
            self.path,
            fmt_share_guid(self.share_guid),
            fmt_guid_key(self.open),
            fmt_guid_key(self.tree),
            self.access,
            self.kind,
            self.day,
        )
    }
}

/// One aggregated, name-resolved row of the heat table.
#[derive(Debug, Clone)]
pub struct HeatRow {
    pub share_guid: Option<GuidKey>,
    pub share: String,
    pub path: String,
    pub reads: u64,
    pub writes: u64,
    /// Count of days with any (read or write) activity.
    pub active_days: u64,
}

impl std::fmt::Display for HeatRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "reads={:>7} writes={:>7} active_days={:>4}  {}\\{}  [share_guid={}]",
            self.reads,
            self.writes,
            self.active_days,
            self.share,
            self.path,
            fmt_share_guid(self.share_guid),
        )
    }
}

/// Outcome of the emit-time leaf/bytes join for one heat key. Pure: computed
/// from the heat key plus the (immutable) inventory + walked-share set; the
/// bridge never mutates the heat table (the periodic dump re-runs it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOutcome {
    /// Leaf file found in inventory — keep, attach allocated bytes.
    Leaf { alloc_bytes: u64 },
    /// Walked share, but no inventory entry for this path — keep, bytes unknown,
    /// flag for a later re-stat.
    Restat,
    /// ShareGUID never bound by a 600 — keep, bytes unknown, flag unresolved.
    UnresolvedShare,
    /// Share-root open (empty path) — drop.
    DropRoot,
    /// Inventory says this path is a directory — drop. Authority is the
    /// inventory dir flag, NOT the access mask.
    DropDir,
    /// Share was never in the walked allowlist (e.g. IPC$/admin) — drop.
    DropNonWalkedShare,
}

/// Emit-time classification of one resolved heat key against the inventory.
/// `share` is the resolved ShareName (`UNKNOWN_SHARE` if no 600 bound it);
/// `norm_path` is the lowercased share-relative path. Pure, no I/O, no mutation.
///
/// ShareName matching is case-insensitive: inventory keys and the walked set are
/// lowercased at load, and this lowercases the (600-derived) lookup ShareName.
/// Precedence is locked: root, then unresolved-share, then non-walked-share,
/// then the inventory dir/file split, then restat.
pub fn classify_key(
    share: &str,
    norm_path: &str,
    inventory: &Inventory,
    walked_shares: &HashSet<String>,
) -> KeyOutcome {
    if norm_path.is_empty() {
        return KeyOutcome::DropRoot;
    }
    if share == UNKNOWN_SHARE {
        return KeyOutcome::UnresolvedShare;
    }
    let share_lc = share.to_lowercase();
    if !walked_shares.contains(&share_lc) {
        return KeyOutcome::DropNonWalkedShare;
    }
    match inventory.get(&(share_lc, norm_path.to_string())) {
        Some(e) if e.is_dir => KeyOutcome::DropDir,
        Some(e) => KeyOutcome::Leaf { alloc_bytes: e.alloc },
        None => KeyOutcome::Restat,
    }
}

/// Format one kept join row. `alloc` is `None` for restat/unresolved (shown as
/// `?`); `flags` are bracketed tokens placed before the `share_guid`.
fn fmt_join_row(r: &HeatRow, alloc: Option<u64>, flags: &[&str]) -> String {
    let bytes = match alloc {
        Some(b) => format!("{b:>9}"),
        None => format!("{:>9}", "?"),
    };
    let mut flagstr = String::new();
    for f in flags {
        flagstr.push_str(&format!("[{f}] "));
    }
    format!(
        "reads={:>4} writes={:>4} active_days={:>3} alloc_bytes={}  {}\\{}  {}[share_guid={}]\n",
        r.reads,
        r.writes,
        r.active_days,
        bytes,
        r.share,
        r.path,
        flagstr,
        fmt_share_guid(r.share_guid),
    )
}

#[derive(Default)]
pub struct CorrelationEngine {
    /// ShareGUID -> ShareName, learned from 600 (Smb2TreeConnectAllocate).
    shares: HashMap<GuidKey, String>,
    /// (ShareGUID, normalized path) -> sparse per-day read/write counts.
    heat: HashMap<HeatKey, BTreeMap<i32, DayCount>>,

    /// File-size inventory (keys lowercased) loaded once at startup, read only
    /// at emit — never touched by `apply`. Empty when no `--share` was given.
    inventory: Inventory,
    /// Set of walked ShareNames (lowercased). Non-empty iff the join is active;
    /// distinguishes a walked-but-empty share from a never-walked one.
    walked_shares: HashSet<String>,

    /// Non-metadata (read/write) opens folded into the table.
    pub opens_total: u64,
    /// Metadata-only opens dropped before the table (global diagnostic).
    pub metadata_skipped: u64,
}

impl CorrelationEngine {
    /// Ingest one normalized event. `event_filetime` is the ETW record timestamp
    /// (ferrisetw `raw_timestamp()`), used only for the day index on opens.
    /// Returns `Some` for each accepted (read/write) open; `None` for binds and
    /// for metadata-only opens.
    pub fn apply(&mut self, ev: &SmbEvent, event_filetime: i64) -> Option<ResolvedAccess> {
        match ev {
            SmbEvent::TreeConnect { share_guid, share } => {
                // Binding can arrive at any time; because heat is keyed on
                // ShareGUID, this retroactively names earlier opens too.
                self.shares.insert(*share_guid, share.clone());
                None
            }
            SmbEvent::Open { open, tree, share_guid, path, access } => {
                let kind = classify_access(*access);
                if kind == Access::Metadata {
                    // Touches no file data — not demand. Drop, count globally.
                    self.metadata_skipped += 1;
                    return None;
                }

                self.opens_total += 1;
                let day = central_civil_day(event_filetime);

                // Fold case so casing-only path variants collapse; key by
                // ShareGUID so naming can be deferred to output time.
                let dc = self
                    .heat
                    .entry((*share_guid, path.to_lowercase()))
                    .or_default()
                    .entry(day)
                    .or_default();
                match kind {
                    Access::Read => dc.reads += 1,
                    Access::Write => dc.writes += 1,
                    Access::Metadata => unreachable!("metadata returned above"),
                }

                Some(ResolvedAccess {
                    share_guid: *share_guid,
                    share: self.resolve_name(*share_guid),
                    path: path.clone(),
                    tree: *tree,
                    open: *open,
                    access: *access,
                    kind,
                    day,
                })
            }
        }
    }

    /// Load the startup inventory + walked-share allowlist (both lowercased).
    /// Call once before processing; the join is emit-only and never mutates
    /// these, and `apply` never reads them.
    pub fn load_inventory(&mut self, inventory: Inventory, walked_shares: HashSet<String>) {
        self.inventory = inventory;
        self.walked_shares = walked_shares;
    }

    /// Resolve a ShareGUID to its ShareName as currently known, or `UNKNOWN`.
    fn resolve_name(&self, share_guid: Option<GuidKey>) -> String {
        share_guid
            .and_then(|g| self.shares.get(&g))
            .cloned()
            .unwrap_or_else(|| UNKNOWN_SHARE.to_string())
    }

    /// The aggregated heat table, names resolved at call time (so a 600 seen
    /// since the opens accrued now names them). Hottest first (by total
    /// accesses), with a stable tiebreak for deterministic output.
    pub fn resolved_summary(&self) -> Vec<HeatRow> {
        let mut rows: Vec<HeatRow> = self
            .heat
            .iter()
            .map(|((sg, path), days)| {
                let reads: u64 = days.values().map(|d| d.reads as u64).sum();
                let writes: u64 = days.values().map(|d| d.writes as u64).sum();
                let active_days =
                    days.values().filter(|d| d.reads > 0 || d.writes > 0).count() as u64;
                HeatRow {
                    share_guid: *sg,
                    share: self.resolve_name(*sg),
                    path: path.clone(),
                    reads,
                    writes,
                    active_days,
                }
            })
            .collect();
        rows.sort_by(|a, b| {
            (b.reads + b.writes)
                .cmp(&(a.reads + a.writes))
                .then_with(|| a.share.cmp(&b.share))
                .then_with(|| a.path.cmp(&b.path))
        });
        rows
    }

    /// The resolved heat table rendered for emission (periodic + final dump).
    ///
    /// With no inventory loaded (`--share` absent) this emits the pre-join table
    /// unchanged. With an inventory it runs the emit-time bridge per key: drops
    /// non-leaves (dir/share-root/non-walked-share) and attaches allocated bytes
    /// to leaves, flagging restat/unresolved-share rows. The header reconciles —
    /// emitted (leaf+restat+unresolved) + dropped == raw keys.
    pub fn resolved_table(&self) -> String {
        let rows = self.resolved_summary();

        if self.walked_shares.is_empty() {
            // No inventory: emit the pre-join table unchanged.
            let mut out = format!(
                "=== resolved heat: {} keys, {} r/w opens ===\n",
                rows.len(),
                self.opens_total
            );
            for r in &rows {
                out.push_str(&r.to_string());
                out.push('\n');
            }
            out.push_str(&format!("metadata-skipped: {}\n", self.metadata_skipped));
            return out;
        }

        let mut body = String::new();
        let (mut leaves, mut restat, mut unresolved) = (0u64, 0u64, 0u64);
        let (mut d_dir, mut d_root, mut d_nonwalked) = (0u64, 0u64, 0u64);
        for r in &rows {
            match classify_key(&r.share, &r.path, &self.inventory, &self.walked_shares) {
                KeyOutcome::Leaf { alloc_bytes } => {
                    leaves += 1;
                    body.push_str(&fmt_join_row(r, Some(alloc_bytes), &[]));
                }
                KeyOutcome::Restat => {
                    restat += 1;
                    body.push_str(&fmt_join_row(r, None, &["restat"]));
                }
                KeyOutcome::UnresolvedShare => {
                    unresolved += 1;
                    body.push_str(&fmt_join_row(r, None, &["unresolved-share"]));
                }
                KeyOutcome::DropDir => d_dir += 1,
                KeyOutcome::DropRoot => d_root += 1,
                KeyOutcome::DropNonWalkedShare => d_nonwalked += 1,
            }
        }
        let kept = leaves + restat + unresolved;
        let mut out = format!(
            "=== resolved heat: {} rows ({} leaves, {} restat, {} unresolved-share), {} r/w opens ===\n",
            kept, leaves, restat, unresolved, self.opens_total
        );
        out.push_str(&format!(
            "dropped: {} dir, {} share-root, {} non-walked-share\n",
            d_dir, d_root, d_nonwalked
        ));
        out.push_str(&body);
        out.push_str(&format!("metadata-skipped: {}\n", self.metadata_skipped));
        out
    }

    /// Per-day counts for a `(share_guid, path)` key (path is case-folded).
    #[allow(dead_code)] // query API: used by tests now, reporting later
    pub fn day_count(&self, share_guid: Option<GuidKey>, path: &str, day: i32) -> DayCount {
        self.heat
            .get(&(share_guid, path.to_lowercase()))
            .and_then(|days| days.get(&day))
            .copied()
            .unwrap_or_default()
    }

    /// Number of distinct day entries for a `(share_guid, path)` key.
    #[allow(dead_code)] // query API: used by tests now, reporting later
    pub fn day_entries(&self, share_guid: Option<GuidKey>, path: &str) -> usize {
        self.heat
            .get(&(share_guid, path.to_lowercase()))
            .map_or(0, BTreeMap::len)
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
            "r/w opens: {} | metadata-skipped: {} | keys: {} ({} named, {} UNKNOWN) | shares bound: {}",
            self.opens_total,
            self.metadata_skipped,
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

    // Locked masks from the spec.
    const READ: u32 = 0x0012_0089;
    const WRITE: u32 = 0x0012_019F;
    const META: u32 = 0x0012_0080;

    // FILETIME for a given Unix-seconds instant.
    fn ft(unix_secs: i64) -> i64 {
        (unix_secs + FILETIME_TO_UNIX_SECS) * 10_000_000
    }

    // A winter (CST) instant; whole days apart stay one Chicago day apart.
    fn ft_day_a() -> i64 {
        ft(1_610_690_400) // 2021-01-15 06:00:00Z -> 2021-01-15 00:00 CST (day 18642)
    }
    fn ft_day_b() -> i64 {
        ft(1_610_690_400 + 86_400) // 2021-01-16 (day 18643)
    }

    fn open(open: GuidKey, share_guid: Option<GuidKey>, path: &str, access: u32) -> SmbEvent {
        SmbEvent::Open { open, tree: TREE_1, share_guid, path: path.into(), access }
    }

    fn row_for<'a>(rows: &'a [HeatRow], path: &str) -> &'a HeatRow {
        rows.iter().find(|r| r.path == path).expect("row present")
    }

    #[test]
    fn classify_access_precedence() {
        assert_eq!(classify_access(0x0012_0089), Access::Read);
        assert_eq!(classify_access(0x0012_019F), Access::Write);
        assert_eq!(classify_access(0x0012_0080), Access::Metadata);
        assert_eq!(classify_access(0x80), Access::Metadata);
        assert_eq!(classify_access(0x0010_0080), Access::Metadata);
        // Write bit set alongside read bit -> Write wins.
        assert_eq!(classify_access(FILE_READ_DATA | FILE_WRITE_DATA), Access::Write);
        assert_eq!(classify_access(FILE_APPEND_DATA), Access::Write);
    }

    #[test]
    fn central_civil_day_cst_and_cdt() {
        // CST (UTC-6): 2021-01-15 06:00:00Z == 2021-01-15 00:00 local -> 18642.
        assert_eq!(central_civil_day(ft(1_610_690_400)), 18642);
        // The same 05:00Z wall-clock in winter is still the PREVIOUS Chicago day
        // (offset -6): 2021-01-15 05:00Z -> 2021-01-14 23:00 CST -> 18641.
        assert_eq!(central_civil_day(ft(1_610_686_800)), 18641);
        // CDT (UTC-5): 2021-07-15 05:00:00Z == 2021-07-15 00:00 local -> 18823.
        // Under CST that instant would be 18822 — confirms the offset flipped.
        assert_eq!(central_civil_day(ft(1_626_325_200)), 18823);
    }

    #[test]
    fn metadata_open_does_not_touch_table_but_bumps_counter() {
        let mut e = CorrelationEngine::default();
        e.apply(&SmbEvent::TreeConnect { share_guid: SHARE_DATA, share: "DATA".into() }, ft_day_a());
        let r = e.apply(&open(OPEN_1, Some(SHARE_DATA), "\\m.txt", META), ft_day_a());
        assert!(r.is_none());
        assert_eq!(e.distinct_keys(), 0);
        assert_eq!(e.metadata_skipped, 1);
        assert_eq!(e.opens_total, 0);
    }

    #[test]
    fn reads_and_writes_land_in_the_right_counter() {
        let mut e = CorrelationEngine::default();
        let r = e
            .apply(&open(OPEN_1, Some(SHARE_DATA), "\\a.txt", READ), ft_day_a())
            .unwrap();
        assert_eq!(r.kind, Access::Read);
        e.apply(&open(OPEN_2, Some(SHARE_DATA), "\\a.txt", WRITE), ft_day_a());

        let dc = e.day_count(Some(SHARE_DATA), "\\a.txt", 18642);
        assert_eq!(dc, DayCount { reads: 1, writes: 1 });
        assert_eq!(e.opens_total, 2);
    }

    #[test]
    fn same_key_same_day_counts_add_on_one_entry() {
        let mut e = CorrelationEngine::default();
        e.apply(&open(OPEN_1, Some(SHARE_DATA), "\\a.txt", READ), ft_day_a());
        e.apply(&open(OPEN_2, Some(SHARE_DATA), "\\a.txt", READ), ft_day_a());
        // No dedup: two distinct opens on the same day add up.
        assert_eq!(e.day_entries(Some(SHARE_DATA), "\\a.txt"), 1);
        assert_eq!(e.day_count(Some(SHARE_DATA), "\\a.txt", 18642).reads, 2);
    }

    #[test]
    fn same_key_different_days_make_two_entries() {
        let mut e = CorrelationEngine::default();
        e.apply(&open(OPEN_1, Some(SHARE_DATA), "\\a.txt", READ), ft_day_a());
        e.apply(&open(OPEN_2, Some(SHARE_DATA), "\\a.txt", READ), ft_day_b());
        assert_eq!(e.day_entries(Some(SHARE_DATA), "\\a.txt"), 2);
        assert_eq!(e.day_count(Some(SHARE_DATA), "\\a.txt", 18642).reads, 1);
        assert_eq!(e.day_count(Some(SHARE_DATA), "\\a.txt", 18643).reads, 1);
    }

    #[test]
    fn casing_variant_paths_collapse_to_one_key() {
        let mut e = CorrelationEngine::default();
        e.apply(&open(OPEN_1, Some(SHARE_DATA), "\\Reports\\Q3.XLSX", READ), ft_day_a());
        e.apply(&open(OPEN_2, Some(SHARE_DATA), "\\reports\\q3.xlsx", WRITE), ft_day_a());
        assert_eq!(e.distinct_keys(), 1);
        let dc = e.day_count(Some(SHARE_DATA), "\\reports\\q3.xlsx", 18642);
        assert_eq!(dc, DayCount { reads: 1, writes: 1 });
    }

    #[test]
    fn active_days_counts_nonzero_days() {
        let mut e = CorrelationEngine::default();
        e.apply(&SmbEvent::TreeConnect { share_guid: SHARE_DATA, share: "DATA".into() }, ft_day_a());
        e.apply(&open(OPEN_1, Some(SHARE_DATA), "\\a.txt", READ), ft_day_a());
        e.apply(&open(OPEN_2, Some(SHARE_DATA), "\\a.txt", WRITE), ft_day_b());
        let rows = e.resolved_summary();
        let row = row_for(&rows, "\\a.txt");
        assert_eq!(row.reads, 1);
        assert_eq!(row.writes, 1);
        assert_eq!(row.active_days, 2);
        assert_eq!(row.share, "DATA");
    }

    #[test]
    fn late_600_names_opens_that_arrived_before_it() {
        let mut e = CorrelationEngine::default();
        // Pre-existing mount: the open arrives BEFORE its binding 600.
        let early = e
            .apply(&open(OPEN_1, Some(SHARE_DATA), "\\pre\\mount.db", READ), ft_day_a())
            .unwrap();
        assert_eq!(early.share, "UNKNOWN");
        e.apply(&SmbEvent::TreeConnect { share_guid: SHARE_DATA, share: "DATA".into() }, ft_day_a());
        let rows = e.resolved_summary();
        assert_eq!(row_for(&rows, "\\pre\\mount.db").share, "DATA");
    }

    #[test]
    fn unbound_shareguid_emits_unknown_and_is_not_dropped() {
        let mut e = CorrelationEngine::default();
        e.apply(&open(OPEN_1, Some(SHARE_HOME), "\\x", READ), ft_day_a());
        let rows = e.resolved_summary();
        assert_eq!(row_for(&rows, "\\x").share, "UNKNOWN");
        assert_eq!(e.day_count(Some(SHARE_HOME), "\\x", 18642).reads, 1);
    }

    #[test]
    fn distinct_shares_are_separate_keys() {
        let mut e = CorrelationEngine::default();
        e.apply(&open(OPEN_1, Some(SHARE_DATA), "\\f", READ), ft_day_a());
        e.apply(&open(OPEN_2, Some(SHARE_HOME), "\\f", READ), ft_day_a());
        assert_eq!(e.distinct_keys(), 2);
    }

    // --- emit-time bridge (pure classifier) -------------------------------

    use crate::inventory::Entry;

    fn inv_file(alloc: u64) -> Entry {
        Entry { logical: alloc, alloc, is_dir: false, sparse: false, compressed: false }
    }
    fn inv_dir() -> Entry {
        Entry { logical: 0, alloc: 0, is_dir: true, sparse: false, compressed: false }
    }
    fn walked(names: &[&str]) -> HashSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn classify_key_one_case_per_outcome() {
        // Inventory keys are lowercased at load (as main does).
        let mut inv = Inventory::new();
        inv.insert(("heattest".into(), "folder3\\txtdoc4.txt".into()), inv_file(4096));
        inv.insert(("heattest".into(), "folder3".into()), inv_dir());
        let w = walked(&["heattest"]);

        // leaf + bytes
        assert_eq!(
            classify_key("HeatTest", "folder3\\txtdoc4.txt", &inv, &w),
            KeyOutcome::Leaf { alloc_bytes: 4096 }
        );
        // restat: walked share, path absent from inventory
        assert_eq!(classify_key("HeatTest", "new.txt", &inv, &w), KeyOutcome::Restat);
        // unresolved-share: no 600 bound the GUID (kept, not dropped)
        assert_eq!(
            classify_key(UNKNOWN_SHARE, "anything.txt", &inv, &w),
            KeyOutcome::UnresolvedShare
        );
        // drop dir: authority is the inventory dir flag
        assert_eq!(classify_key("HeatTest", "folder3", &inv, &w), KeyOutcome::DropDir);
        // drop share-root: empty path
        assert_eq!(classify_key("HeatTest", "", &inv, &w), KeyOutcome::DropRoot);
        // drop non-walked share: how IPC$/admin shares fall out
        assert_eq!(
            classify_key("IPC$", "srvsvc", &inv, &w),
            KeyOutcome::DropNonWalkedShare
        );
    }

    #[test]
    fn classify_key_sharename_is_case_insensitive() {
        // Inventory + walked set lowercased at load; a 600-derived ShareName in
        // any case still joins.
        let mut inv = Inventory::new();
        inv.insert(("heattest".into(), "a.txt".into()), inv_file(8192));
        let w = walked(&["heattest"]);
        assert_eq!(
            classify_key("HEATTEST", "a.txt", &inv, &w),
            KeyOutcome::Leaf { alloc_bytes: 8192 }
        );
        assert_eq!(
            classify_key("HeAtTeSt", "a.txt", &inv, &w),
            KeyOutcome::Leaf { alloc_bytes: 8192 }
        );
    }
}
