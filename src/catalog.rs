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
                if previous != attention {
                    // A live transition; now() is accurate (the
                    // transcript just changed). Drives "time in state".
                    session.attention_entered_at = Some(SystemTime::now());
                }
                return Some(previous);
            }
        }
        None
    }

    /// Apply a heuristic-derived attention update that may be
    /// suppressed by an active Claude Code hook pin. Returns the
    /// previous attention iff the update was applied (so the caller
    /// can fire a notifier transition); returns `None` if the session
    /// is hook-pinned and `event_mtime` is `<=` the pin timestamp,
    /// meaning the transcript hasn't advanced past the hook signal.
    ///
    /// Clears `hook_pinned` when the update *does* apply (and the pin
    /// was set): the transcript has progressed past the hook event, so
    /// the heuristic is once again trustworthy for this session. The
    /// next hook event will repin.
    ///
    /// `event_mtime` is `None` for initial-prime events whose mtime
    /// the watcher couldn't capture; treat those as "no signal about
    /// transcript progress" and suppress while pinned.
    pub fn apply_heuristic_attention(
        &mut self,
        id: &SessionId,
        attention: Attention,
        event_mtime: Option<SystemTime>,
    ) -> Option<Attention> {
        for session in &mut self.sessions {
            if session.id == *id {
                if let Some(pin) = session.hook_pinned {
                    match event_mtime {
                        Some(m) if m > pin => {
                            // Transcript advanced past the hook event;
                            // heuristic is authoritative again.
                            session.hook_pinned = None;
                        }
                        _ => return None,
                    }
                }
                let previous = session.attention;
                session.attention = attention;
                if previous != attention {
                    // Prefer the transcript mtime that drove this update
                    // (when the state actually changed) over wall-clock;
                    // fall back to now() for the rare mtime-less event.
                    session.attention_entered_at =
                        Some(event_mtime.unwrap_or_else(SystemTime::now));
                }
                // The heuristic is transcript-truth and can never mean
                // "blocking prompt" (a transcript-derived NeedsInput is
                // a finished turn, i.e. "done"). Any prior blocking
                // prompt is resolved once the transcript moves past it,
                // so clear the flag whenever the heuristic applies.
                session.blocking_prompt = false;
                return Some(previous);
            }
        }
        None
    }

    /// Apply a Claude Code `Notification` hook event for `id`. Forces
    /// attention to `NeedsInput` and pins the hook authority at
    /// `received_at` so subsequent heuristic-derived updates are
    /// suppressed until transcript mtime advances past it. Returns the
    /// previous attention iff the session was found, mirroring
    /// [`SessionCatalog::update_attention`] so the caller can fire a
    /// notifier transition.
    ///
    /// `blocking_prompt` distinguishes a permission/elicitation prompt
    /// (the agent is actively waiting on an answer — `true`) from an
    /// idle nudge (`false`); it drives the sidebar's "answer me" vs
    /// "done" glyph but not the notification, which fires for both. The
    /// flag is overwritten on every hook event (a later `idle_prompt`
    /// for a session that was permission-blocked downgrades it) and
    /// cleared by [`SessionCatalog::apply_heuristic_attention`] once the
    /// transcript progresses.
    ///
    /// Idempotent across rapid repeat hook events: each call just
    /// re-pins the timestamp and re-asserts `NeedsInput`; the
    /// notifier's episodic-flag suppression collapses the duplicate
    /// pings.
    pub fn apply_hook_event(
        &mut self,
        id: &SessionId,
        blocking_prompt: bool,
        received_at: SystemTime,
    ) -> Option<Attention> {
        for session in &mut self.sessions {
            if session.id == *id {
                let previous = session.attention;
                session.attention = Attention::NeedsInput;
                if previous != Attention::NeedsInput {
                    // Only an actual attention transition resets the
                    // "time in state" clock — a done→blocked escalation
                    // (NeedsInput both times, blocking flips) keeps
                    // counting from when it first stopped, reading as
                    // "awaiting your input for Xm".
                    session.attention_entered_at = Some(received_at);
                }
                session.blocking_prompt = blocking_prompt;
                session.hook_pinned = Some(received_at);
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
    /// one host. Two-stage match per session, mirroring the attach
    /// path's `resolve_pane_target` so the indicator agrees with what
    /// Enter will actually do:
    ///
    /// 1. If the session's id appears in `live_session_ids`, it has a
    ///    deterministic live pane (the resume-fallback's
    ///    `agent-mux-<id>` tmux session is up) — `Some(true)`.
    /// 2. Otherwise, if `session.project_dir` is in `cwds`, fall back
    ///    to the cwd match — `Some(true)`. (Catches externally-created
    ///    sessions and agent-mux sessions whose embedded pane is
    ///    still on tmux's auto-assigned name from spawn.)
    /// 3. Otherwise `Some(false)`.
    ///
    /// The set is keyed by `SessionId` rather than by tmux session
    /// name so the catalog never deals in tmux strings — the caller
    /// (orchestrator-level) maps the tmux convention
    /// (`agent-mux-<id>`) into ids before invoking this method.
    ///
    /// Sessions on other hosts are untouched. A pane poller failure
    /// (no tmux, ssh hiccup) surfaces as empty sets — every session
    /// on that host transitions to `Some(false)`, matching the
    /// user-visible reality (Enter will fall through to `claude
    /// --resume`).
    pub fn apply_live_panes(
        &mut self,
        host_id: &HostId,
        cwds: &HashSet<PathBuf>,
        live_session_ids: &HashSet<SessionId>,
    ) {
        for session in &mut self.sessions {
            if &session.host == host_id {
                let named = live_session_ids.contains(&session.id);
                let cwd_match = cwds.contains(&session.project_dir);
                session.has_live_pane = Some(named || cwd_match);
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
            hook_pinned: None,
            blocking_prompt: false,
            attention_entered_at: None,
            started_at: None,
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
        c.apply_live_panes(&host, &cwds, &HashSet::new());

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

        c.apply_live_panes(&a, &HashSet::new(), &HashSet::new());
        // Both alpha sessions get a snapshot decision...
        assert_eq!(c.sessions()[0].has_live_pane, Some(false));
        // ...but beta is unchanged from its default.
        assert_eq!(c.sessions()[1].has_live_pane, None);
    }

    #[test]
    fn apply_live_panes_empty_set_marks_every_session_false() {
        // Failure-mode contract: a pane poller error (no tmux server)
        // surfaces as empty cwds + names, which means every session on
        // that host is marked as not having a live pane.
        let mut c = SessionCatalog::new();
        let host = HostId("alpha".into());
        c.add(session_on("a", host.clone()));
        c.add(session_on("b", host.clone()));
        c.apply_live_panes(&host, &HashSet::new(), &HashSet::new());
        assert_eq!(c.sessions()[0].has_live_pane, Some(false));
        assert_eq!(c.sessions()[1].has_live_pane, Some(false));
    }

    #[test]
    fn apply_live_panes_marks_true_via_live_session_id_even_when_cwd_differs() {
        // Deterministic-pin wins independently of cwd — the orchestrator
        // has resolved an `agent-mux-<id>` tmux session into the
        // SessionId set, and the catalog respects it without knowing
        // the tmux naming convention itself.
        let mut c = SessionCatalog::new();
        let host = HostId("alpha".into());
        let mut s = session_on("abc", host.clone());
        s.project_dir = PathBuf::from("/work/proj");
        c.add(s);

        let mut ids = HashSet::new();
        ids.insert(SessionId("abc".into()));
        c.apply_live_panes(&host, &HashSet::new(), &ids);

        assert_eq!(c.sessions()[0].has_live_pane, Some(true));
    }

    #[test]
    fn apply_live_panes_resolves_per_session_when_two_sessions_share_a_cwd() {
        // Two sessions share a project_dir; only one has a
        // deterministic-pin live session id. The named one is `Some(true)`
        // via the id set; the unnamed one falls back to the cwd match
        // and is also `Some(true)` — preserving externally-created
        // session behaviour. The attach-side `find_pane_local` gives
        // the named session priority; here we just pin the indicator.
        let mut c = SessionCatalog::new();
        let host = HostId("alpha".into());
        let cwd = PathBuf::from("/work/proj");
        let mut a = session_on("abc", host.clone());
        a.project_dir = cwd.clone();
        let mut b = session_on("xyz", host.clone());
        b.project_dir = cwd.clone();
        c.add(a);
        c.add(b);

        let mut cwds = HashSet::new();
        cwds.insert(cwd);
        let mut ids = HashSet::new();
        ids.insert(SessionId("abc".into()));
        c.apply_live_panes(&host, &cwds, &ids);

        assert_eq!(c.sessions()[0].has_live_pane, Some(true)); // id-pinned
        assert_eq!(c.sessions()[1].has_live_pane, Some(true)); // cwd fallback
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

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs)
    }

    #[test]
    fn apply_hook_event_forces_needs_input_and_pins_timestamp() {
        let mut c = SessionCatalog::new();
        let mut s = session("a");
        s.attention = Attention::Working;
        c.add(s);
        let prev = c.apply_hook_event(&SessionId("a".into()), false, at(100));
        assert_eq!(prev, Some(Attention::Working));
        let s = &c.sessions()[0];
        assert_eq!(s.attention, Attention::NeedsInput);
        assert_eq!(s.hook_pinned, Some(at(100)));
    }

    #[test]
    fn update_attention_stamps_entered_at_only_on_a_real_transition() {
        let mut c = SessionCatalog::new();
        let mut s = session("a");
        s.attention = Attention::Working;
        s.attention_entered_at = Some(at(10));
        c.add(s);
        // No-op update (same value) must NOT restamp — "time in state"
        // keeps counting from the original entry.
        c.update_attention(&SessionId("a".into()), Attention::Working);
        assert_eq!(c.sessions()[0].attention_entered_at, Some(at(10)));
        // A real transition advances the stamp (to ~now, which is well
        // past the epoch-anchored at(10)).
        c.update_attention(&SessionId("a".into()), Attention::NeedsInput);
        let stamped = c.sessions()[0]
            .attention_entered_at
            .expect("stamped on transition");
        assert!(
            stamped > at(10),
            "transition must advance attention_entered_at"
        );
    }

    #[test]
    fn apply_heuristic_attention_stamps_entered_at_from_event_mtime() {
        let mut c = SessionCatalog::new();
        let mut s = session("a");
        s.attention = Attention::NeedsInput;
        s.attention_entered_at = Some(at(5));
        c.add(s);
        // A transition with a known transcript mtime stamps that mtime
        // (when the state actually changed) rather than wall-clock.
        c.apply_heuristic_attention(&SessionId("a".into()), Attention::Working, Some(at(50)));
        assert_eq!(c.sessions()[0].attention_entered_at, Some(at(50)));
    }

    #[test]
    fn apply_hook_event_returns_none_for_unknown_session() {
        let mut c = SessionCatalog::new();
        assert!(
            c.apply_hook_event(&SessionId("ghost".into()), false, at(0))
                .is_none()
        );
    }

    #[test]
    fn apply_heuristic_attention_suppressed_when_hook_pinned_and_mtime_not_newer() {
        // Permission-prompt scenario: hook fires at T=10, heuristic
        // polls at T=12 with the same (stale) transcript mtime=8
        // — heuristic says Working from the still-pending tool_use,
        // but we want to stay in NeedsInput.
        let mut c = SessionCatalog::new();
        c.add(session("a"));
        c.apply_hook_event(&SessionId("a".into()), false, at(10));
        let prev =
            c.apply_heuristic_attention(&SessionId("a".into()), Attention::Working, Some(at(8)));
        assert_eq!(prev, None, "stale heuristic update must be suppressed");
        let s = &c.sessions()[0];
        assert_eq!(s.attention, Attention::NeedsInput);
        assert_eq!(s.hook_pinned, Some(at(10)));
    }

    #[test]
    fn apply_heuristic_attention_clears_pin_when_mtime_advances_past_it() {
        // User approves the permission prompt at T=15; tool_result
        // lands in the transcript with a fresh mtime. Heuristic
        // derives Working; we apply it AND clear the pin so the
        // heuristic is authoritative again.
        let mut c = SessionCatalog::new();
        c.add(session("a"));
        c.apply_hook_event(&SessionId("a".into()), false, at(10));
        let prev =
            c.apply_heuristic_attention(&SessionId("a".into()), Attention::Working, Some(at(20)));
        assert_eq!(prev, Some(Attention::NeedsInput));
        let s = &c.sessions()[0];
        assert_eq!(s.attention, Attention::Working);
        assert!(
            s.hook_pinned.is_none(),
            "pin must clear once transcript advances"
        );
    }

    #[test]
    fn apply_heuristic_attention_passes_through_when_no_hook_pin_set() {
        let mut c = SessionCatalog::new();
        c.add(session("a"));
        let prev =
            c.apply_heuristic_attention(&SessionId("a".into()), Attention::NeedsInput, Some(at(5)));
        assert_eq!(prev, Some(Attention::Unknown));
        assert_eq!(c.sessions()[0].attention, Attention::NeedsInput);
    }

    #[test]
    fn apply_hook_event_sets_blocking_prompt_and_heuristic_clears_it() {
        // A permission/elicitation hook flips `blocking_prompt` so the
        // sidebar can show "answer me"; once the transcript advances
        // past the prompt the heuristic re-applies and clears it back
        // to "done". Pins both ends so the glyph never sticks on.
        let mut c = SessionCatalog::new();
        c.add(session("a"));
        c.apply_hook_event(&SessionId("a".into()), true, at(10));
        assert!(
            c.sessions()[0].blocking_prompt,
            "blocking hook must set the flag"
        );
        // Transcript advances past the pin → heuristic applies and
        // clears the flag (a transcript-derived state is never blocking).
        c.apply_heuristic_attention(&SessionId("a".into()), Attention::Working, Some(at(20)));
        assert!(
            !c.sessions()[0].blocking_prompt,
            "heuristic re-apply must clear blocking_prompt"
        );
    }

    #[test]
    fn apply_hook_event_idle_nudge_leaves_blocking_prompt_false() {
        // An idle_prompt (blocking=false) still forces NeedsInput but
        // must NOT light the "answer me" glyph — it's "done/waiting".
        let mut c = SessionCatalog::new();
        c.add(session("a"));
        c.apply_hook_event(&SessionId("a".into()), false, at(10));
        assert_eq!(c.sessions()[0].attention, Attention::NeedsInput);
        assert!(!c.sessions()[0].blocking_prompt);
    }

    #[test]
    fn apply_heuristic_attention_suppresses_when_pinned_and_event_mtime_is_none() {
        // Initial-prime events arrive without mtime — they shouldn't
        // be able to clear a pin (we'd be guessing the transcript has
        // progressed). Suppress.
        let mut c = SessionCatalog::new();
        c.add(session("a"));
        c.apply_hook_event(&SessionId("a".into()), false, at(10));
        let prev = c.apply_heuristic_attention(&SessionId("a".into()), Attention::Working, None);
        assert_eq!(prev, None);
        assert_eq!(c.sessions()[0].attention, Attention::NeedsInput);
    }
}
