//! Baseline file-size inventory walker.
//!
//! Produces bytes-on-disk per file so the heat model can later rank by density
//! (score ÷ bytes). On-disk size is the logical EOF length rounded up to the
//! volume's cluster size — exact for the ~99.98% of files that are neither
//! sparse nor compressed; the rare ones are *flagged* (their `alloc` is
//! approximate), not silently trusted.
//!
//! Standalone this pass: nothing here is wired into the ETW/resolve/heat path.
//! In-memory only; no persistence. The `normalize_path` output is built to
//! match the 650 `Name` heat key exactly so an inventory entry can join to a
//! heat record later.

use std::collections::HashMap;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path};

use walkdir::WalkDir;
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceW;

// Win32 file-attribute bits we care about.
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x010;
const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FILE_ATTRIBUTE_COMPRESSED: u32 = 0x800;

/// Per file/dir size record.
#[derive(Debug, Clone)]
pub struct Entry {
    /// EOF length (logical size).
    pub logical: u64,
    /// `logical` rounded up to the volume cluster size. Approximate when
    /// `sparse` or `compressed` is set.
    pub alloc: u64,
    pub is_dir: bool,
    /// Flagged: `alloc` is approximate for sparse files.
    pub sparse: bool,
    /// Flagged: `alloc` is approximate for compressed files.
    pub compressed: bool,
}

/// Keyed by (ShareName, normalized_path) — the same key shape the 650 heat
/// record uses, so the two join later.
pub type Inventory = HashMap<(String, String), Entry>;

/// Result of one walk: the inventory plus the metadata needed to cross-check it.
pub struct Walk {
    pub entries: Inventory,
    pub reparse_skipped: u64,
    pub cluster_size: u64,
}

/// The classified file-attribute flags for one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attrs {
    pub is_dir: bool,
    pub sparse: bool,
    pub compressed: bool,
    pub reparse: bool,
}

/// Decode a Win32 file-attribute dword into the flags we track.
pub fn classify_attrs(a: u32) -> Attrs {
    Attrs {
        is_dir: a & FILE_ATTRIBUTE_DIRECTORY != 0,
        sparse: a & FILE_ATTRIBUTE_SPARSE_FILE != 0,
        compressed: a & FILE_ATTRIBUTE_COMPRESSED != 0,
        reparse: a & FILE_ATTRIBUTE_REPARSE_POINT != 0,
    }
}

/// Round `n` up to a whole number of clusters. `0 -> 0`; exact multiples are
/// unchanged; anything over a boundary jumps to the next cluster.
pub fn round_up(n: u64, cluster: u64) -> u64 {
    if n == 0 || cluster == 0 {
        n
    } else {
        ((n + cluster - 1) / cluster) * cluster
    }
}

/// Derive the share-relative key matching the 650 `Name` convention: strip the
/// local-root prefix, force backslash separators, drop any leading separator,
/// lowercase. (The heat key lowercases `Name`; this mirrors that.)
pub fn normalize_path(full: &Path, local_root: &Path) -> String {
    let rel = full.strip_prefix(local_root).unwrap_or(full);
    rel.to_string_lossy()
        .replace('/', "\\")
        .trim_start_matches('\\')
        .to_lowercase()
}

/// The volume root (with trailing backslash) that `GetDiskFreeSpaceW` wants,
/// e.g. `C:\` or `\\server\share\`. Falls back to the path itself if it has no
/// recognizable prefix.
fn volume_root(p: &Path) -> std::ffi::OsString {
    for comp in p.components() {
        if let Component::Prefix(prefix) = comp {
            let mut s = prefix.as_os_str().to_os_string();
            s.push("\\");
            return s;
        }
    }
    p.as_os_str().to_os_string()
}

/// Query the cluster size once, on the volume containing `local_root`.
/// cluster_size = sectors_per_cluster * bytes_per_sector.
fn cluster_size(local_root: &Path) -> std::io::Result<u64> {
    let root = volume_root(local_root);
    let wide: Vec<u16> = root.encode_wide().chain(std::iter::once(0)).collect();

    let mut sectors_per_cluster: u32 = 0;
    let mut bytes_per_sector: u32 = 0;
    // SAFETY: `wide` is a valid NUL-terminated UTF-16 string that outlives the
    // call; the two out-pointers are valid for the duration of the call.
    unsafe {
        GetDiskFreeSpaceW(
            PCWSTR(wide.as_ptr()),
            Some(&mut sectors_per_cluster),
            Some(&mut bytes_per_sector),
            None,
            None,
        )
    }
    .map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("GetDiskFreeSpaceW({}) failed: {e}", root.to_string_lossy()),
        )
    })?;

    Ok(sectors_per_cluster as u64 * bytes_per_sector as u64)
}

/// Walk `local_root`, building an in-memory inventory keyed by
/// (`share`, normalized_path). Does not follow reparse points (and counts them
/// as skipped); does not open files (directory-enumeration metadata only).
pub fn walk(share: &str, local_root: &Path) -> std::io::Result<Walk> {
    let cluster = cluster_size(local_root)?;
    let mut entries = Inventory::new();
    let mut reparse_skipped = 0u64;

    for entry in WalkDir::new(local_root).follow_links(false) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue, // unreadable dir/file: skip, keep walking
        };
        // Skip the root itself (depth 0) — its normalized path is empty.
        if entry.depth() == 0 {
            continue;
        }

        // metadata() reuses the directory-enumeration data on Windows and does
        // not open the file.
        let md = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let attrs = classify_attrs(md.file_attributes());

        // Reparse points: count and skip, never recurse or size.
        if attrs.reparse {
            reparse_skipped += 1;
            continue;
        }

        let logical = md.len();
        let rec = Entry {
            logical,
            alloc: round_up(logical, cluster),
            is_dir: attrs.is_dir,
            sparse: attrs.sparse,
            compressed: attrs.compressed,
        };
        let key = (share.to_string(), normalize_path(entry.path(), local_root));
        entries.insert(key, rec);
    }

    Ok(Walk { entries, reparse_skipped, cluster_size: cluster })
}

impl Walk {
    /// A human summary for cross-checking a walk.
    pub fn summary(&self) -> String {
        let mut files = 0u64;
        let mut dirs = 0u64;
        let mut total_logical = 0u64;
        let mut total_alloc = 0u64;
        let mut sparse = 0u64;
        let mut compressed = 0u64;

        for e in self.entries.values() {
            if e.is_dir {
                dirs += 1;
            } else {
                files += 1;
                total_logical += e.logical;
                total_alloc += e.alloc;
            }
            if e.sparse {
                sparse += 1;
            }
            if e.compressed {
                compressed += 1;
            }
        }

        // Top 10 files by alloc (path tiebreak for determinism).
        let mut top: Vec<(&(String, String), &Entry)> =
            self.entries.iter().filter(|(_, e)| !e.is_dir).collect();
        top.sort_by(|a, b| b.1.alloc.cmp(&a.1.alloc).then_with(|| a.0 .1.cmp(&b.0 .1)));

        let tb = |bytes: u64| bytes as f64 / 1_000_000_000_000.0;
        let mut out = String::new();
        out.push_str(&format!("cluster_size: {} bytes\n", self.cluster_size));
        out.push_str(&format!("files: {files}   dirs: {dirs}\n"));
        out.push_str(&format!(
            "logical: {total_logical} bytes ({:.3} TB)\n",
            tb(total_logical)
        ));
        out.push_str(&format!(
            "alloc:   {total_alloc} bytes ({:.3} TB)\n",
            tb(total_alloc)
        ));
        out.push_str(&format!(
            "sparse: {sparse}   compressed: {compressed}   reparse-skipped: {}\n",
            self.reparse_skipped
        ));
        out.push_str("top 10 files by alloc:\n");
        for ((_, path), e) in top.into_iter().take(10) {
            out.push_str(&format!(
                "  {:>14} alloc  ({:>14} logical)  {path}\n",
                e.alloc, e.logical
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_matches_650_name_convention() {
        let root = Path::new(r"C:\data");
        // Share-relative, backslash separators, no leading sep, lowercased.
        assert_eq!(
            normalize_path(Path::new(r"C:\data\Reports\Q3.XLSX"), root),
            "reports\\q3.xlsx"
        );
        // Already at the share root level.
        assert_eq!(normalize_path(Path::new(r"C:\data\Readme.TXT"), root), "readme.txt");
        // A nested path with mixed case folds fully.
        assert_eq!(
            normalize_path(Path::new(r"C:\data\Team\Sub Dir\File.DAT"), root),
            "team\\sub dir\\file.dat"
        );
    }

    #[test]
    fn round_up_to_cluster() {
        let c = 4096;
        assert_eq!(round_up(0, c), 0); // empty file occupies nothing
        assert_eq!(round_up(4096, c), 4096); // exact multiple unchanged
        assert_eq!(round_up(4097, c), 8192); // one over rounds to next cluster
        assert_eq!(round_up(1, c), 4096); // any non-zero takes a whole cluster
        assert_eq!(round_up(8192, c), 8192);
    }

    #[test]
    fn attribute_classification() {
        // Plain file: nothing set.
        assert_eq!(
            classify_attrs(0x020 /* ARCHIVE */),
            Attrs { is_dir: false, sparse: false, compressed: false, reparse: false }
        );
        // Directory.
        assert!(classify_attrs(FILE_ATTRIBUTE_DIRECTORY).is_dir);
        // Sparse + compressed together.
        let a = classify_attrs(FILE_ATTRIBUTE_SPARSE_FILE | FILE_ATTRIBUTE_COMPRESSED);
        assert!(a.sparse && a.compressed && !a.is_dir && !a.reparse);
        // Reparse point (e.g. a junction), with the directory bit also set.
        let a = classify_attrs(FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY);
        assert!(a.reparse && a.is_dir);
    }
}
