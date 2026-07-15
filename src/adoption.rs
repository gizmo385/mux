//! Codex spawn-correlation: the pending-spawn table + adoption state
//! machine (multi-agent plan §2.4).
//!
//! Codex can't pin a session id (upstream declined `--session-id`), so the
//! `PinnedId` identity contract every other agent enjoys — minted uuid ==
//! tmux name == transcript stem == `SessionId` — breaks for it. Instead the
//! Attachment Driver spawns codex under a *provisional* tmux name
//! (`agent-mux-pending-<nonce>`), records a [`PendingSpawn`] here, and waits
//! for the rollout the watcher is already looking for. When a
//! `NewTranscript{ agent: Codex, .. }` event arrives, the main loop reads
//! its `session_meta` cwd and asks [`PendingSpawns::adopt`] to correlate it
//! against an outstanding spawn in the same directory within
//! [`ADOPTION_WINDOW`]; on a match it renames the tmux session to the
//! durable `agent-mux-<id>` and re-keys the embedded pane.
//!
//! This module is deliberately pure — no tmux, no host I/O, no clock of its
//! own (the caller passes `now`). It is the unit-testable heart of the
//! protocol; the side effects (rename, re-key, footer error) live in
//! `main.rs`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::agent::AgentKind;

/// Window within which a freshly-appeared rollout may be correlated to a
/// pending codex spawn by cwd (plan §2.4 step 2). Kept short so a stale
/// entry (codex crashed, wrong binary, `--ephemeral` slipped in) expires
/// quickly into a footer error rather than adopting an unrelated later
/// rollout that happens to share the directory.
pub const ADOPTION_WINDOW: Duration = Duration::from_secs(30);

/// One outstanding `DiscoverAfterSpawn` launch awaiting its rollout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSpawn {
    /// The `cwd` the agent was spawned in — the correlation key against a
    /// rollout's `session_meta` cwd.
    pub cwd: PathBuf,
    /// The uuid minted at spawn that names the provisional tmux session
    /// (`agent-mux-pending-<nonce>`). Handed to the Attachment Driver on
    /// adoption so it can rename that session to `agent-mux-<id>`.
    pub nonce: String,
    /// Which agent this spawn was for. Only agents whose
    /// [`crate::agent::SpawnPlan`] is `DiscoverAfterSpawn` (codex) ever
    /// register here, but the field is carried so a `NewTranscript` for a
    /// *different* agent can never adopt a codex pending (and vice versa).
    pub agent: AgentKind,
    /// When the spawn was dispatched — the [`ADOPTION_WINDOW`] anchor.
    pub spawned_at: SystemTime,
}

/// The pending-spawn table. Insertion order is spawn order, which is what
/// [`adopt`](Self::adopt) relies on for its FIFO tie-break.
#[derive(Debug, Default)]
pub struct PendingSpawns {
    entries: Vec<PendingSpawn>,
}

impl PendingSpawns {
    /// Record a freshly-dispatched `DiscoverAfterSpawn` launch.
    pub fn record(&mut self, cwd: PathBuf, nonce: String, agent: AgentKind, now: SystemTime) {
        self.entries.push(PendingSpawn {
            cwd,
            nonce,
            agent,
            spawned_at: now,
        });
    }

    /// True when nothing is outstanding — lets the caller skip the head
    /// read entirely on the common `NewTranscript` (no spawn in flight).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Correlate a rollout in `cwd` to the *oldest* live pending spawn for
    /// `agent`, removing and returning it. `None` when no live entry
    /// matches (wrong agent, wrong cwd, or the only candidates have already
    /// aged past [`ADOPTION_WINDOW`] — those belong to
    /// [`sweep_expired`](Self::sweep_expired), not here).
    ///
    /// FIFO on same-cwd collisions: because `record` appends in spawn
    /// order, the first positional match is the earliest spawn, so two
    /// codex launches in one directory adopt in birth order against
    /// file-birth order (plan Risks). The residual "two spawns, one window,
    /// rollouts land out of order" race can still cross-adopt ids — this is
    /// the same two-unnamed-sessions-one-cwd collision the codebase already
    /// documents for externally-started sessions, and is accepted.
    pub fn adopt(&mut self, agent: AgentKind, cwd: &Path, now: SystemTime) -> Option<PendingSpawn> {
        let idx = self
            .entries
            .iter()
            .position(|e| e.agent == agent && e.cwd == cwd && within_window(e.spawned_at, now))?;
        Some(self.entries.remove(idx))
    }

    /// Remove and return every entry whose [`ADOPTION_WINDOW`] has fully
    /// elapsed. Called opportunistically (each tick + each
    /// `NewTranscript`) — no timer thread. The caller surfaces each as a
    /// footer/status spawn error and drops it (plan §2.4 step 4).
    pub fn sweep_expired(&mut self, now: SystemTime) -> Vec<PendingSpawn> {
        let mut expired = Vec::new();
        let mut i = 0;
        while i < self.entries.len() {
            if within_window(self.entries[i].spawned_at, now) {
                i += 1;
            } else {
                expired.push(self.entries.remove(i));
            }
        }
        expired
    }
}

/// True while `spawned_at` is within [`ADOPTION_WINDOW`] of `now`
/// (inclusive of the exact boundary). Clock skew (`now` before
/// `spawned_at`) is treated as within-window — a just-spawned entry is
/// never mistaken for expired.
fn within_window(spawned_at: SystemTime, now: SystemTime) -> bool {
    now.duration_since(spawned_at)
        .map_or(true, |elapsed| elapsed <= ADOPTION_WINDOW)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd(p: &str) -> PathBuf {
        PathBuf::from(p)
    }

    /// A base instant plus a helper to offset by seconds, so window
    /// boundaries are exercised without touching the real clock.
    fn t0() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000)
    }
    fn plus(base: SystemTime, secs: u64) -> SystemTime {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn pending_then_adopted_within_window() {
        let mut table = PendingSpawns::default();
        let now = t0();
        table.record(cwd("/w/proj"), "nonce-1".into(), AgentKind::Codex, now);
        let adopted = table.adopt(AgentKind::Codex, Path::new("/w/proj"), plus(now, 5));
        assert_eq!(
            adopted,
            Some(PendingSpawn {
                cwd: cwd("/w/proj"),
                nonce: "nonce-1".into(),
                agent: AgentKind::Codex,
                spawned_at: now,
            })
        );
        // Consumed: a second adopt finds nothing.
        assert!(
            table
                .adopt(AgentKind::Codex, Path::new("/w/proj"), plus(now, 6))
                .is_none()
        );
        assert!(table.is_empty());
    }

    #[test]
    fn pending_then_expired_after_window() {
        let mut table = PendingSpawns::default();
        let now = t0();
        table.record(cwd("/w/proj"), "nonce-1".into(), AgentKind::Codex, now);
        // One second past the window: swept, not adoptable.
        let expired = table.sweep_expired(plus(now, ADOPTION_WINDOW.as_secs() + 1));
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].nonce, "nonce-1");
        assert!(table.is_empty());
    }

    #[test]
    fn expired_entry_is_not_adopted() {
        let mut table = PendingSpawns::default();
        let now = t0();
        table.record(cwd("/w/proj"), "nonce-1".into(), AgentKind::Codex, now);
        // Just past the window: adopt must decline (it belongs to sweep).
        let past = plus(now, ADOPTION_WINDOW.as_secs() + 1);
        assert!(
            table
                .adopt(AgentKind::Codex, Path::new("/w/proj"), past)
                .is_none()
        );
    }

    #[test]
    fn boundary_is_inclusive_live() {
        let mut table = PendingSpawns::default();
        let now = t0();
        table.record(cwd("/w/proj"), "nonce-1".into(), AgentKind::Codex, now);
        // Exactly at the window: still adoptable, not swept.
        let at = plus(now, ADOPTION_WINDOW.as_secs());
        assert!(table.sweep_expired(at).is_empty());
        assert!(
            table
                .adopt(AgentKind::Codex, Path::new("/w/proj"), at)
                .is_some()
        );
    }

    #[test]
    fn fifo_on_same_cwd() {
        let mut table = PendingSpawns::default();
        let now = t0();
        table.record(cwd("/w/proj"), "first".into(), AgentKind::Codex, now);
        table.record(
            cwd("/w/proj"),
            "second".into(),
            AgentKind::Codex,
            plus(now, 1),
        );
        // First rollout adopts the earliest spawn…
        let a = table.adopt(AgentKind::Codex, Path::new("/w/proj"), plus(now, 2));
        assert_eq!(a.unwrap().nonce, "first");
        // …the next rollout adopts the later one.
        let b = table.adopt(AgentKind::Codex, Path::new("/w/proj"), plus(now, 3));
        assert_eq!(b.unwrap().nonce, "second");
        assert!(table.is_empty());
    }

    #[test]
    fn non_matching_agent_is_ignored() {
        // A Pi (or Claude) NewTranscript must never adopt a codex pending,
        // even in the same cwd within the window.
        let mut table = PendingSpawns::default();
        let now = t0();
        table.record(cwd("/w/proj"), "nonce-1".into(), AgentKind::Codex, now);
        assert!(
            table
                .adopt(AgentKind::Pi, Path::new("/w/proj"), plus(now, 1))
                .is_none()
        );
        // The codex entry is untouched and still adoptable by codex.
        assert!(
            table
                .adopt(AgentKind::Codex, Path::new("/w/proj"), plus(now, 1))
                .is_some()
        );
    }

    #[test]
    fn wrong_cwd_is_ignored() {
        let mut table = PendingSpawns::default();
        let now = t0();
        table.record(cwd("/w/proj"), "nonce-1".into(), AgentKind::Codex, now);
        assert!(
            table
                .adopt(AgentKind::Codex, Path::new("/w/other"), plus(now, 1))
                .is_none()
        );
    }

    #[test]
    fn adopt_on_empty_table_is_none() {
        // A codex NewTranscript with no pending spawn (e.g. a session the
        // user started outside agent-mux) is a no-op.
        let mut table = PendingSpawns::default();
        assert!(
            table
                .adopt(AgentKind::Codex, Path::new("/w/proj"), t0())
                .is_none()
        );
    }

    #[test]
    fn sweep_only_removes_expired_and_keeps_live() {
        let mut table = PendingSpawns::default();
        let now = t0();
        table.record(cwd("/old"), "old".into(), AgentKind::Codex, now);
        table.record(
            cwd("/new"),
            "new".into(),
            AgentKind::Codex,
            plus(now, ADOPTION_WINDOW.as_secs()),
        );
        // At now+window+1: the first is expired, the second is exactly at
        // its own window (still live).
        let expired = table.sweep_expired(plus(now, ADOPTION_WINDOW.as_secs() + 1));
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].nonce, "old");
        // The live one survives and is still adoptable.
        assert!(!table.is_empty());
        assert!(
            table
                .adopt(
                    AgentKind::Codex,
                    Path::new("/new"),
                    plus(now, ADOPTION_WINDOW.as_secs())
                )
                .is_some()
        );
    }
}
