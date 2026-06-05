//! The correlation engine — the part the spike exists to prove.
//!
//! It maintains three small lookup tables and joins them so that an "Open
//! established" event (650) resolves to (user, share, path). Connections,
//! sessions, and trees are long-lived and few; opens are the access pulse.
//!
//! Pure and unit-tested — no ETW dependency. The ETW layer feeds it
//! `SmbEvent`s and prints whatever `apply` hands back.

use std::collections::HashMap;

use crate::events::{fmt_guid_key, GuidKey, SmbEvent};
use crate::identity::{self, PrincipalClass};

#[derive(Debug, Clone)]
struct ConnInfo {
    client: String,
}

#[derive(Debug, Clone)]
struct SessionInfo {
    user: String,
    client: Option<String>,
}

#[derive(Debug, Clone)]
struct TreeInfo {
    session: GuidKey,
    share: String,
}

/// A fully resolved file access — the tuple the spike is trying to produce.
#[derive(Debug, Clone)]
pub struct ResolvedAccess {
    pub class: PrincipalClass,
    pub user: String,
    pub client: Option<String>,
    pub share: String,
    pub path: String,
    pub session: GuidKey,
    pub tree: GuidKey,
    pub access: u32,
}

impl std::fmt::Display for ResolvedAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{:?}] {} @ {} | {}\\{}  (sess {}, tree {}) access=0x{:08X}",
            self.class,
            self.user,
            self.client.as_deref().unwrap_or("?"),
            self.share,
            self.path,
            fmt_guid_key(self.session),
            fmt_guid_key(self.tree),
            self.access,
        )
    }
}

#[derive(Default)]
pub struct CorrelationEngine {
    conns: HashMap<GuidKey, ConnInfo>,
    sessions: HashMap<GuidKey, SessionInfo>,
    trees: HashMap<GuidKey, TreeInfo>,

    // Spike metrics — the resolved/unresolved ratio is how you judge success.
    pub opens_total: u64,
    pub opens_resolved: u64,
    pub opens_unresolved: u64,
}

impl CorrelationEngine {
    /// Ingest one normalized event. Returns `Some` only when an open fully
    /// resolves to a user.
    pub fn apply(&mut self, ev: &SmbEvent) -> Option<ResolvedAccess> {
        match ev {
            SmbEvent::ConnAccept { conn, client } => {
                self.conns.insert(*conn, ConnInfo { client: client.clone() });
                None
            }
            SmbEvent::ConnEnd { conn } => {
                self.conns.remove(conn);
                None
            }
            SmbEvent::SessionAuth { session, conn, user, domain } => {
                let client = conn
                    .and_then(|c| self.conns.get(&c))
                    .map(|c| c.client.clone());
                // Compose DOMAIN\user before storing so classification (and the
                // resolved output) see the full principal.
                let principal = identity::compose(domain, user);
                self.sessions
                    .insert(*session, SessionInfo { user: principal, client });
                None
            }
            SmbEvent::SessionEnd { session } => {
                self.sessions.remove(session);
                None
            }
            SmbEvent::TreeConnect { tree, session, share } => {
                self.trees.insert(
                    *tree,
                    TreeInfo { session: *session, share: share.clone() },
                );
                None
            }
            SmbEvent::TreeEnd { tree } => {
                self.trees.remove(tree);
                None
            }
            SmbEvent::Open { session, tree, path, access } => {
                self.resolve(*session, *tree, path, *access)
            }
        }
    }

    /// Join an open back to a user. The tree is the richer key (it yields both
    /// the share and the authoritative session linkage); we fall back to a
    /// session id carried directly on the open if the tree isn't known yet.
    fn resolve(
        &mut self,
        session: Option<GuidKey>,
        tree: Option<GuidKey>,
        path: &str,
        access: u32,
    ) -> Option<ResolvedAccess> {
        self.opens_total += 1;

        let (sess_from_tree, share) = match tree.and_then(|t| self.trees.get(&t)) {
            Some(t) => (Some(t.session), Some(t.share.clone())),
            None => (None, None),
        };
        let sess = sess_from_tree.or(session);

        match sess.and_then(|s| self.sessions.get(&s)) {
            Some(si) => {
                self.opens_resolved += 1;
                Some(ResolvedAccess {
                    class: identity::classify(&si.user),
                    user: si.user.clone(),
                    client: si.client.clone(),
                    share: share.unwrap_or_else(|| "<unresolved-share>".into()),
                    path: path.to_string(),
                    session: sess.unwrap_or(0),
                    tree: tree.unwrap_or(0),
                    access,
                })
            }
            None => {
                // Expected at cold start for sessions/trees that predate the
                // trace. The `--rundown` flag closes this gap; see README.
                self.opens_unresolved += 1;
                None
            }
        }
    }

    pub fn stats_line(&self) -> String {
        format!(
            "opens: {} total, {} resolved, {} unresolved | live: {} conns, {} sessions, {} trees",
            self.opens_total,
            self.opens_resolved,
            self.opens_unresolved,
            self.conns.len(),
            self.sessions.len(),
            self.trees.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The keys are GUIDs in production; the engine treats them as opaque u128
    // `GuidKey`s, so the fixtures use plain integer literals as stand-in GUIDs.
    const CONN_A: GuidKey = 0x0000_0000_0000_0000_0000_0000_0000_0001;
    const SESS_A: GuidKey = 0x0000_0000_0000_0000_0000_0000_0000_0100;
    const TREE_A: GuidKey = 0x0000_0000_0000_0000_0000_0000_0000_0010;
    const SESS_B: GuidKey = 0x0000_0000_0000_0000_0000_0000_0000_0200;
    const TREE_B: GuidKey = 0x0000_0000_0000_0000_0000_0000_0000_0020;

    fn seed_human(e: &mut CorrelationEngine) {
        e.apply(&SmbEvent::ConnAccept { conn: CONN_A, client: "10.0.0.5".into() });
        e.apply(&SmbEvent::SessionAuth { session: SESS_A, conn: Some(CONN_A), user: "alice".into(), domain: "CONTOSO".into() });
        e.apply(&SmbEvent::TreeConnect { tree: TREE_A, session: SESS_A, share: "DATA".into() });
    }

    #[test]
    fn full_chain_resolves() {
        let mut e = CorrelationEngine::default();
        seed_human(&mut e);
        let r = e
            .apply(&SmbEvent::Open {
                session: Some(SESS_A),
                tree: Some(TREE_A),
                path: "\\projects\\q3.xlsx".into(),
                access: 0x0012_0089,
            })
            .expect("should resolve");
        assert_eq!(r.user, "CONTOSO\\alice");
        assert_eq!(r.share, "DATA");
        assert_eq!(r.class, PrincipalClass::Human);
        assert_eq!(r.client.as_deref(), Some("10.0.0.5"));
        assert_eq!(e.opens_resolved, 1);
    }

    #[test]
    fn resolves_from_tree_alone() {
        // Open carries no session id; the tree supplies the linkage.
        let mut e = CorrelationEngine::default();
        seed_human(&mut e);
        let r = e
            .apply(&SmbEvent::Open { session: None, tree: Some(TREE_A), path: "x".into(), access: 0 })
            .expect("tree should carry the session");
        assert_eq!(r.user, "CONTOSO\\alice");
        assert_eq!(r.share, "DATA");
    }

    #[test]
    fn machine_account_is_tagged() {
        let mut e = CorrelationEngine::default();
        e.apply(&SmbEvent::SessionAuth { session: SESS_B, conn: None, user: "SGIFS01$".into(), domain: "CONTOSO".into() });
        e.apply(&SmbEvent::TreeConnect { tree: TREE_B, session: SESS_B, share: "DATA".into() });
        let r = e
            .apply(&SmbEvent::Open { session: Some(SESS_B), tree: Some(TREE_B), path: "y".into(), access: 0 })
            .unwrap();
        assert_eq!(r.class, PrincipalClass::Machine);
        assert!(r.class.is_automation());
    }

    #[test]
    fn cold_start_open_is_unresolved_not_panicking() {
        let mut e = CorrelationEngine::default();
        let r = e.apply(&SmbEvent::Open { session: Some(0xDEAD), tree: Some(0xBEEF), path: "z".into(), access: 0 });
        assert!(r.is_none());
        assert_eq!(e.opens_unresolved, 1);
    }

    #[test]
    fn teardown_removes_state() {
        let mut e = CorrelationEngine::default();
        seed_human(&mut e);
        e.apply(&SmbEvent::SessionEnd { session: SESS_A });
        let r = e.apply(&SmbEvent::Open { session: Some(SESS_A), tree: Some(TREE_A), path: "z".into(), access: 0 });
        // Tree still maps to session SESS_A, but the session is gone -> unresolved.
        assert!(r.is_none());
    }
}
