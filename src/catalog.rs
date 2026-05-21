use std::collections::HashSet;
use std::path::PathBuf;
use std::time::SystemTime;

use crate::session::{Attention, HostId, Session, SessionId};

#[derive(Debug, Default)]
pub struct SessionCatalog {
    sessions: Vec<Session>,
}

impl SessionCatalog {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn replace_all(&mut self, mut sessions: Vec<Session>) {
        sessions.sort_by_key(|s| std::cmp::Reverse(s.last_activity));
        self.sessions = sessions;
    }

    /// Append a newly-discovered session at the tail. Returns `true` if it
    /// was inserted, `false` if a session with the same id already exists.
    /// Appending (rather than sorting in) preserves the index a UI list
    /// state holds — the dashboard's selected row stays selected when a
    /// fresh session shows up mid-run.
    pub fn add(&mut self, session: Session) -> bool {
        if self.sessions.iter().any(|s| s.id == session.id) {
            return false;
        }
        self.sessions.push(session);
        true
    }

    /// Update the attention state of a session by id. Returns the
    /// previous attention value if the session was found, or `None`
    /// otherwise. The previous value is the transition signal the
    /// M4 notifier consumes (fire only on prev ≠ `NeedsInput`, new =
    /// `NeedsInput`) — without it, every poll-driven re-derivation
    /// would look like a new transition.
    pub fn update_attention(&mut self, id: &SessionId, attention: Attention) -> Option<Attention> {
        for session in &mut self.sessions {
            if session.id == *id {
                let previous = session.attention;
                session.attention = attention;
                return Some(previous);
            }
        }
        None
    }

    /// Bump a session's `last_activity` to `mtime`, but only if the
    /// new value is newer than what the catalog already holds. Used by
    /// the main loop to keep the sidebar's "last activity" cell live
    /// across the lifetime of a running conversation — discovery sets
    /// `last_activity` once at startup; without this, the cell would
    /// otherwise sit at the discovery-time mtime forever and an active
    /// session would slowly appear to go stale. The `mtime > current`
    /// guard makes the call safe to fire on every Attention event:
    /// out-of-order events (the rare case where a poll cycle and a
    /// notify event race against the same write) can't rewind the cell.
    /// Returns `true` if the value changed.
    pub fn touch_activity(&mut self, id: &SessionId, mtime: SystemTime) -> bool {
        for session in &mut self.sessions {
            if session.id == *id {
                if mtime > session.last_activity {
                    session.last_activity = mtime;
                    return true;
                }
                return false;
            }
        }
        false
    }

    /// Apply a fresh pane-presence snapshot from the pane poller for
    /// one host: for every session on `host_id`, set `has_live_pane`
    /// to `Some(true)` iff its `project_dir` appears in `cwds`,
    /// otherwise `Some(false)`. Sessions on other hosts are untouched.
    ///
    /// A pane poller failure (no tmux, ssh hiccup) surfaces as an
    /// empty `cwds` set — every session on that host transitions to
    /// `Some(false)`, which matches the user-visible reality
    /// (Enter will fall through to `claude --resume`).
    pub fn apply_live_panes(&mut self, host_id: &HostId, cwds: &HashSet<PathBuf>) {
        for session in &mut self.sessions {
            if &session.host == host_id {
                session.has_live_pane = Some(cwds.contains(&session.project_dir));
            }
        }
    }

    /// Remove the session with the given id, returning it if found.
    /// The dashboard calls this after a successful worktree-delete so
    /// the row vanishes immediately rather than waiting for the next
    /// discovery refresh to filter on the now-missing cwd. Returning
    /// the removed [`Session`] lets the caller act on its
    /// `transcript_path` / `host` without a second lookup.
    pub fn remove_by_id(&mut self, id: &SessionId) -> Option<Session> {
        let pos = self.sessions.iter().position(|s| s.id == *id)?;
        Some(self.sessions.remove(pos))
    }

    /// Replace every session currently in the catalog whose `host` is
    /// `host_id` with the given `fresh` set. Sessions belonging to
    /// other hosts are untouched.
    ///
    /// The disk-cache fast-path seeds the catalog with stale-but-valid
    /// snapshots at startup so the dashboard renders immediately;
    /// when the live SSH discovery for that host returns, this method
    /// reconciles the two. Entries that disappeared on the remote
    /// (deleted transcripts, abandoned sessions) drop out; entries
    /// that survived get updated last-activity / attention / title
    /// from the live read; brand-new entries (sessions started after
    /// the snapshot) are appended.
    pub fn reconcile_host(&mut self, host_id: &HostId, fresh: Vec<Session>) {
        self.sessions.retain(|s| &s.host != host_id);
        self.sessions.extend(fresh);
    }

    #[must_use]
    pub fn sessions(&self) -> &[Session] {
        &self.sessions
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.sessions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::SystemTime;

    fn session(id: &str) -> Session {
        session_on(id, HostId::local())
    }

    fn session_on(id: &str, host: HostId) -> Session {
        Session {
            id: SessionId(id.to_string()),
            host,
            project_dir: PathBuf::from("/proj"),
            transcript_path: PathBuf::from(format!("/transcripts/{id}.jsonl")),
            last_activity: SystemTime::UNIX_EPOCH,
            attention: Attention::Unknown,
            title: None,
            parent_repo: None,
            has_live_pane: None,
        }
    }

    #[test]
    fn add_appends_to_empty_catalog() {
        let mut c = SessionCatalog::new();
        assert!(c.add(session("a")));
        assert_eq!(c.len(), 1);
        assert_eq!(c.sessions()[0].id.0, "a");
    }

    #[test]
    fn add_appends_at_tail_preserving_existing_order() {
        let mut c = SessionCatalog::new();
        c.replace_all(vec![session("first"), session("second")]);
        assert!(c.add(session("third")));
        let ids: Vec<&str> = c.sessions().iter().map(|s| s.id.0.as_str()).collect();
        assert_eq!(ids, vec!["first", "second", "third"]);
    }

    #[test]
    fn add_rejects_duplicate_id() {
        let mut c = SessionCatalog::new();
        c.add(session("a"));
        assert!(!c.add(session("a")));
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn reconcile_host_drops_stale_entries_and_inserts_fresh() {
        let mut c = SessionCatalog::new();
        let host = HostId("alpha".into());
        c.add(session_on("stale", host.clone()));
        c.add(session_on("kept-id", host.clone()));
        c.reconcile_host(
            &host,
            vec![
                session_on("kept-id", host.clone()),
                session_on("brand-new", host.clone()),
            ],
        );
        let ids: Vec<&str> = c.sessions().iter().map(|s| s.id.0.as_str()).collect();
        // "stale" dropped (not in fresh set); "kept-id" present
        // (was in both); "brand-new" appended.
        assert!(!ids.contains(&"stale"));
        assert!(ids.contains(&"kept-id"));
        assert!(ids.contains(&"brand-new"));
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn reconcile_host_does_not_touch_other_hosts() {
        let mut c = SessionCatalog::new();
        let a = HostId("alpha".into());
        let b = HostId("beta".into());
        c.add(session_on("a1", a.clone()));
        c.add(session_on("b1", b.clone()));
        c.reconcile_host(&a, Vec::new());
        let ids: Vec<&str> = c.sessions().iter().map(|s| s.id.0.as_str()).collect();
        assert_eq!(ids, vec!["b1"]);
    }

    #[test]
    fn reconcile_host_with_empty_fresh_set_removes_everything_for_that_host() {
        let mut c = SessionCatalog::new();
        let host = HostId("alpha".into());
        c.add(session_on("a", host.clone()));
        c.add(session_on("b", host.clone()));
        c.reconcile_host(&host, Vec::new());
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn apply_live_panes_marks_match_true_and_others_false() {
        let mut c = SessionCatalog::new();
        let host = HostId("alpha".into());
        let mut a = session_on("a", host.clone());
        a.project_dir = PathBuf::from("/work/a");
        let mut b = session_on("b", host.clone());
        b.project_dir = PathBuf::from("/work/b");
        c.add(a);
        c.add(b);

        let mut cwds = HashSet::new();
        cwds.insert(PathBuf::from("/work/a"));
        c.apply_live_panes(&host, &cwds);

        assert_eq!(c.sessions()[0].has_live_pane, Some(true));
        assert_eq!(c.sessions()[1].has_live_pane, Some(false));
    }

    #[test]
    fn apply_live_panes_does_not_touch_sessions_on_other_hosts() {
        let mut c = SessionCatalog::new();
        let a = HostId("alpha".into());
        let b = HostId("beta".into());
        c.add(session_on("a1", a.clone()));
        c.add(session_on("b1", b.clone()));

        c.apply_live_panes(&a, &HashSet::new());
        // Both alpha sessions get a snapshot decision...
        assert_eq!(c.sessions()[0].has_live_pane, Some(false));
        // ...but beta is unchanged from its default.
        assert_eq!(c.sessions()[1].has_live_pane, None);
    }

    #[test]
    fn apply_live_panes_empty_set_marks_every_session_false() {
        // Failure-mode contract: a pane poller error (no tmux server)
        // surfaces as an empty cwds set, which means every session on
        // that host is marked as not having a live pane.
        let mut c = SessionCatalog::new();
        let host = HostId("alpha".into());
        c.add(session_on("a", host.clone()));
        c.add(session_on("b", host.clone()));
        c.apply_live_panes(&host, &HashSet::new());
        assert_eq!(c.sessions()[0].has_live_pane, Some(false));
        assert_eq!(c.sessions()[1].has_live_pane, Some(false));
    }

    #[test]
    fn remove_by_id_drops_the_matching_session_and_returns_it() {
        let mut c = SessionCatalog::new();
        c.add(session("a"));
        c.add(session("b"));
        c.add(session("c"));

        let removed = c.remove_by_id(&SessionId("b".into())).expect("found");
        assert_eq!(removed.id.0, "b");
        let ids: Vec<&str> = c.sessions().iter().map(|s| s.id.0.as_str()).collect();
        // 'a' and 'c' survive in original order; 'b' is gone.
        assert_eq!(ids, vec!["a", "c"]);
    }

    #[test]
    fn remove_by_id_returns_none_for_unknown_id() {
        let mut c = SessionCatalog::new();
        c.add(session("a"));
        assert!(c.remove_by_id(&SessionId("nope".into())).is_none());
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn touch_activity_advances_to_newer_mtime() {
        let mut c = SessionCatalog::new();
        let mut s = session("a");
        s.last_activity = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(100);
        c.add(s);

        let newer = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(200);
        assert!(c.touch_activity(&SessionId("a".into()), newer));
        assert_eq!(c.sessions()[0].last_activity, newer);
    }

    #[test]
    fn touch_activity_refuses_to_rewind_to_older_mtime() {
        // Out-of-order events (e.g. a poll-tick and a notify event
        // racing the same write) must not move the cell backward.
        let mut c = SessionCatalog::new();
        let mut s = session("a");
        s.last_activity = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(200);
        c.add(s);

        let older = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(100);
        assert!(!c.touch_activity(&SessionId("a".into()), older));
        // Cell unchanged.
        assert_eq!(
            c.sessions()[0].last_activity,
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(200)
        );
    }

    #[test]
    fn touch_activity_returns_false_for_unknown_id() {
        let mut c = SessionCatalog::new();
        c.add(session("a"));
        let when = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(100);
        assert!(!c.touch_activity(&SessionId("nope".into()), when));
    }

    #[test]
    fn reconcile_host_overlays_updated_fields_on_surviving_sessions() {
        // The reconcile is implemented as drop-then-extend, so a
        // surviving id picks up the *fresh* attention/title — not
        // the cached one. Pin that contract explicitly.
        let mut c = SessionCatalog::new();
        let host = HostId("alpha".into());
        let mut cached = session_on("s1", host.clone());
        cached.attention = Attention::Idle;
        cached.title = Some("old title".into());
        c.add(cached);

        let mut live = session_on("s1", host.clone());
        live.attention = Attention::NeedsInput;
        live.title = Some("new title".into());
        c.reconcile_host(&host, vec![live]);

        let s = &c.sessions()[0];
        assert_eq!(s.attention, Attention::NeedsInput);
        assert_eq!(s.title.as_deref(), Some("new title"));
    }
}
