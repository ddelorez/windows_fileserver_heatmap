//! Filesystem archive writer. The archive is the inspectable record of every
//! dump received — including ones we go on to reject — so writes happen before
//! row/footer validation. Two destinations:
//!
//!   * the per-server path `<root>/<server>/<run8>-<seq>.ndjson` for dumps whose
//!     header parsed and whose names are path-safe, and
//!   * `<root>/_malformed/<unix-millis>.ndjson` for everything else.
//!
//! Both write the body VERBATIM (exact bytes received) and create parent
//! directories as needed. The per-server write overwrites: re-POSTing the same
//! dump is legal and idempotent.

use std::io;
use std::path::Path;

/// Write the raw body to `<root>/<rel_path>`, creating parent directories and
/// overwriting any existing file. `rel_path` comes from
/// [`crate::ingest::archive_rel_path`] and is built only from already-validated
/// (path-safe) naming fields.
pub fn write_verbatim(root: &Path, rel_path: &str, body: &[u8]) -> io::Result<()> {
    let dest = root.join(rel_path);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&dest, body)
}

/// Write the raw body to `<root>/_malformed/<unix_millis>.ndjson`. Used for
/// dumps we reject before (or at) header/name validation, where we cannot trust
/// the names enough to build a per-server path.
pub fn write_malformed(root: &Path, unix_millis: u128, body: &[u8]) -> io::Result<()> {
    let dir = root.join("_malformed");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(format!("{unix_millis}.ndjson")), body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_creates_dirs_and_overwrites() {
        let root = tempfile::tempdir().unwrap();
        let rel = "sgi-backup/5b08749f-3.ndjson";

        write_verbatim(root.path(), rel, b"first\n").unwrap();
        let dest = root.path().join(rel);
        assert_eq!(std::fs::read(&dest).unwrap(), b"first\n");

        // Re-POST of the same dump overwrites in place (idempotent).
        write_verbatim(root.path(), rel, b"second\n").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"second\n");
    }

    #[test]
    fn malformed_lands_in_malformed_dir() {
        let root = tempfile::tempdir().unwrap();
        write_malformed(root.path(), 1_749_600_000_123, b"garbage").unwrap();
        let dest = root.path().join("_malformed").join("1749600000123.ndjson");
        assert_eq!(std::fs::read(&dest).unwrap(), b"garbage");
    }
}
