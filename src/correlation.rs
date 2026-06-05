//! The correlation engine — the part the spike exists to prove.
//!
//! It maintains three small lookup tables and joins them so that an "Open
//! established" event (650) resolves to (user, share, path). Connections,
//! sessions, and trees are long-lived and few; opens are the access pulse.
//!
//! Pure and unit-tested — no ETW dependency. The ETW layer feeds it
//! `SmbEvent`s and prints whatever `apply` hands back.

use std::collections::HashMap;

use crate::events::SmbEvent;
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
    session_id: u64,
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
    pub session_id: u64,
    pub tree_id: u64,
}

impl std::fmt::Display for ResolvedAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{:?}] {} @ {} | {}{}  (sess {}, tree {})",
            self.class,
            self.user,
            self.client.as_deref().unwrap_or("?"),
            self.share,
            self.path,
            self.session_id,
            self.tree_id
        )
    }
}

#[derive(Default)]
pub struct CorrelationEngine {
    conns: HashMap<u64, ConnInfo>,
    sessions: HashMap<u64, SessionInfo>,
    trees: HashMap<u64, TreeInfo>,

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
            SmbEvent::ConnAccept { conn_id, client } => {
                self.conns.insert(*conn_id, ConnInfo { client: client.clone() });
                None
            }
            SmbEvent::ConnEnd { conn_id } => {
                self.conns.remove(conn_id);
                None
            }
            SmbEvent::SessionAuth { session_id, conn_id, user } => {
                let client = conn_id
                    .and_then(|c| self.conns.get(&c))
                    .map(|c| c.client.clone());
                self.sessions
                    .insert(*session_id, SessionInfo { user: user.clone(), client });
                None
            }
            SmbEvent::SessionEnd { session_id } => {
                self.sessions.remove(session_id);
                None
            }
            SmbEvent::TreeConnect { tree_id, session_id, share } => {
                self.trees.insert(
                    *tree_id,
                    TreeInfo { session_id: *session_id, share: share.clone() },
                );
                None
            }
            SmbEvent::TreeEnd { tree_id } => {
                self.trees.remove(tree_id);
                None
            }
            SmbEvent::Open { session_id, tree_id, path } => {
                self.resolve(*session_id, *tree_id, path)
            }
        }
    }

    /// Join an open back to a user. The tree is the richer key (it yields both
    /// the share and the authoritative session linkage); we fall back to a
    /// session id carried directly on the open if the tree isn't known yet.
    fn resolve(
        &mut self,
        session_id: Option<u64>,
        tree_id: Option<u64>,
        path: &str,
    ) -> Option<ResolvedAccess> {
        self.opens_total += 1;

        let (sess_from_tree, share) = match tree_id.and_then(|t| self.trees.get(&t)) {
            Some(t) => (Some(t.session_id), Some(t.share.clone())),
            None => (None, None),
        };
        let sess = sess_from_tree.or(session_id);

        match sess.and_then(|s| self.sessions.get(&s)) {
            Some(si) => {
                self.opens_resolved += 1;
                Some(ResolvedAccess {
                    class: identity::classify(&si.user),
                    user: si.user.clone(),
                    client: si.client.clone(),
                    share: share.unwrap_or_else(|| "<unresolved-share>".into()),
                    path: path.to_string(),
                    session_id: sess.unwrap_or(0),
                    tree_id: tree_id.unwrap_or(0),
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

    fn seed_human(e: &mut CorrelationEngine) {
        e.apply(&SmbEvent::ConnAccept { conn_id: 1, client: "10.0.0.5".into() });
        e.apply(&SmbEvent::SessionAuth { session_id: 100, conn_id: Some(1), user: "CONTOSO\\alice".into() });
        e.apply(&SmbEvent::TreeConnect { tree_id: 10, session_id: 100, share: "DATA".into() });
    }

    #[test]
    fn full_chain_resolves() {
        let mut e = CorrelationEngine::default();
        seed_human(&mut e);
        let r = e
            .apply(&SmbEvent::Open {
                session_id: Some(100),
                tree_id: Some(10),
                path: "\\projects\\q3.xlsx".into(),
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
            .apply(&SmbEvent::Open { session_id: None, tree_id: Some(10), path: "x".into() })
            .expect("tree should carry the session");
        assert_eq!(r.user, "CONTOSO\\alice");
        assert_eq!(r.share, "DATA");
    }

    #[test]
    fn machine_account_is_tagged() {
        let mut e = CorrelationEngine::default();
        e.apply(&SmbEvent::SessionAuth { session_id: 200, conn_id: None, user: "CONTOSO\\SGIFS01$".into() });
        e.apply(&SmbEvent::TreeConnect { tree_id: 20, session_id: 200, share: "DATA".into() });
        let r = e
            .apply(&SmbEvent::Open { session_id: Some(200), tree_id: Some(20), path: "y".into() })
            .unwrap();
        assert_eq!(r.class, PrincipalClass::Machine);
        assert!(r.class.is_automation());
    }

    #[test]
    fn cold_start_open_is_unresolved_not_panicking() {
        let mut e = CorrelationEngine::default();
        let r = e.apply(&SmbEvent::Open { session_id: Some(999), tree_id: Some(999), path: "z".into() });
        assert!(r.is_none());
        assert_eq!(e.opens_unresolved, 1);
    }

    #[test]
    fn teardown_removes_state() {
        let mut e = CorrelationEngine::default();
        seed_human(&mut e);
        e.apply(&SmbEvent::SessionEnd { session_id: 100 });
        let r = e.apply(&SmbEvent::Open { session_id: Some(100), tree_id: Some(10), path: "z".into() });
        // Tree still maps to session 100, but the session is gone -> unresolved.
        assert!(r.is_none());
    }
}
