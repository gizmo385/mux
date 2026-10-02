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
//! against a live outstanding spawn in the same directory on the same host;
//! on a match it renames the tmux session to the durable `agent-mux-<id>`
//! and re-keys the embedded pane.
//!
//! **Liveness, not a fixed window.** Interactive codex writes no rollout
//! until the user submits the first prompt (verified 2026-10-02 against real
//! codex 0.142.5: no file 38 s after launch, written < 1 s after submit, the
//! filename timestamp still the *launch* time). So a pending entry is kept
//! alive for as long as its provisional tmux session is: every live-panes
//! snapshot that still lists it refreshes [`PendingSpawn::last_alive`]
//! ([`PendingSpawns::observe_live`]), and an entry expires only once it has
//! gone [`ADOPTION_WINDOW`] without being seen (codex exited before its
//! first prompt, the spawn never started, or the host's pane poller went
//! silent) or has outlived [`PENDING_HARD_CAP`]. Absence from a *single*
//! snapshot is deliberately not treated as death — the pane poller emits an
//! empty snapshot when `tmux list-panes` fails transiently.
//!
//! This module is deliberately pure — no tmux, no host I/O, no clock of its
//! own (the caller passes `now`). It is the unit-testable heart of the
//! protocol; the side effects (rename, re-key, footer error) live in
//! `main.rs`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::agent::AgentKind;
use crate::session::HostId;

/// How long a pending spawn may go without its provisional tmux session
/// being seen alive before it expires. Measured from the spawn itself until
/// the first sighting, then from the most recent sighting. Long enough to
/// ride out several failed pane polls (the poller ticks every ~3 s), short
/// enough that a codex that crashed at launch surfaces as a footer error
/// promptly.
pub const ADOPTION_WINDOW: Duration = Duration::from_secs(30);

/// Absolute ceiling on how long a pending spawn may wait for its rollout,
/// however long its tmux session stays alive. Bounds the table if a user
/// parks a never-prompted codex for days; past this the entry is dropped.
pub const PENDING_HARD_CAP: Duration = Duration::from_hours(24);

/// Clock slack allowed when comparing a rollout id's embedded creation time
/// against the spawn time (see [`uuid_v7_created_at`]). The id is minted by
/// codex at launch, a beat *after* agent-mux dispatched the spawn, so a
/// legitimate match is never earlier than `spawned_at` on one clock; the
/// slack absorbs remote-host clock skew (the id is stamped on the remote's
/// clock, `spawned_at` on ours).
pub const ID_CLOCK_SLACK: Duration = Duration::from_secs(60);

/// One outstanding `DiscoverAfterSpawn` launch awaiting its rollout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSpawn {
    /// The host the agent was spawned on. Only that host's live-panes
    /// snapshots refresh this entry, and only that host's rollouts adopt it.
    pub host: HostId,
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
    /// When the spawn was dispatched — the [`PENDING_HARD_CAP`] anchor and
    /// the lower bound for the rollout id's creation time.
    pub spawned_at: SystemTime,
    /// The last moment the provisional tmux session was seen alive
    /// (initially `spawned_at`) — the [`ADOPTION_WINDOW`] anchor.
    pub last_alive: SystemTime,
}

/// The pending-spawn table. Insertion order is spawn order, which is what
/// [`adopt`](Self::adopt) relies on for its FIFO tie-break.
#[derive(Debug, Default)]
pub struct PendingSpawns {
    entries: Vec<PendingSpawn>,
}

impl PendingSpawns {
    /// Record a freshly-dispatched `DiscoverAfterSpawn` launch.
    pub fn record(
        &mut self,
        host: HostId,
        cwd: PathBuf,
        nonce: String,
        agent: AgentKind,
        now: SystemTime,
    ) {
        self.entries.push(PendingSpawn {
            host,
            cwd,
            nonce,
            agent,
            spawned_at: now,
            last_alive: now,
        });
    }

    /// True when nothing is outstanding — lets the caller skip the head
    /// read entirely on the common `NewTranscript` (no spawn in flight).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Fold one live-panes snapshot for `host` into the table: every entry
    /// on that host whose provisional session `is_live` (by nonce) has its
    /// [`PendingSpawn::last_alive`] refreshed to `now`. Entries the
    /// snapshot doesn't list are left untouched — they age toward expiry
    /// rather than dying on one possibly-failed poll.
    pub fn observe_live(&mut self, host: &HostId, is_live: impl Fn(&str) -> bool, now: SystemTime) {
        for e in &mut self.entries {
            if e.host == *host && is_live(&e.nonce) {
                e.last_alive = now;
            }
        }
    }

    /// Correlate a rollout in `cwd` on `host` to the *oldest* live pending
    /// spawn for `agent`, removing and returning it. `None` when no live
    /// entry matches (wrong agent/host/cwd, the only candidates have
    /// expired — those belong to [`sweep_expired`](Self::sweep_expired) —
    /// or the rollout predates every candidate spawn).
    ///
    /// `id_created_at` is the rollout id's embedded creation time when it
    /// has one ([`uuid_v7_created_at`]); a rollout created more than
    /// [`ID_CLOCK_SLACK`] before a candidate's spawn can't be that spawn's
    /// (it's an unrelated codex the user launched earlier in the same
    /// directory whose first prompt — and so whose rollout — only now
    /// landed), so that candidate is skipped.
    ///
    /// FIFO on same-cwd collisions: because `record` appends in spawn
    /// order, the first positional match is the earliest spawn, so two
    /// codex launches in one directory adopt in birth order against
    /// file-birth order (plan Risks). With rollouts landing on *first
    /// prompt* rather than launch, two same-cwd spawns prompted in the
    /// opposite order can still cross-adopt ids — the same
    /// two-unnamed-sessions-one-cwd collision the codebase already
    /// documents for externally-started sessions, and accepted.
    pub fn adopt(
        &mut self,
        host: &HostId,
        agent: AgentKind,
        cwd: &Path,
        id_created_at: Option<SystemTime>,
        now: SystemTime,
    ) -> Option<PendingSpawn> {
        let idx = self.entries.iter().position(|e| {
            e.host == *host
                && e.agent == agent
                && e.cwd == cwd
                && is_live(e, now)
                && id_created_at.is_none_or(|created| not_before_spawn(created, e.spawned_at))
        })?;
        Some(self.entries.remove(idx))
    }

    /// Remove and return every expired entry (see the module docs). Called
    /// opportunistically (each tick + each `NewTranscript`) — no timer
    /// thread. The caller surfaces each as a footer/status spawn error and
    /// drops it (plan §2.4 step 4).
    pub fn sweep_expired(&mut self, now: SystemTime) -> Vec<PendingSpawn> {
        let mut expired = Vec::new();
        let mut i = 0;
        while i < self.entries.len() {
            if is_live(&self.entries[i], now) {
                i += 1;
            } else {
                expired.push(self.entries.remove(i));
            }
        }
        expired
    }
}

/// True while `e` was seen alive within [`ADOPTION_WINDOW`] of `now`
/// (inclusive of the exact boundary) and hasn't outlived
/// [`PENDING_HARD_CAP`]. Clock skew (`now` before the anchor) is treated
/// as live — a just-spawned entry is never mistaken for expired.
fn is_live(e: &PendingSpawn, now: SystemTime) -> bool {
    let within = |anchor: SystemTime, limit: Duration| {
        now.duration_since(anchor)
            .map_or(true, |elapsed| elapsed <= limit)
    };
    within(e.last_alive, ADOPTION_WINDOW) && within(e.spawned_at, PENDING_HARD_CAP)
}

/// True unless `created` is more than [`ID_CLOCK_SLACK`] before `spawned_at`.
fn not_before_spawn(created: SystemTime, spawned_at: SystemTime) -> bool {
    spawned_at
        .duration_since(created)
        .map_or(true, |earlier_by| earlier_by <= ID_CLOCK_SLACK)
}

/// The creation time embedded in an RFC 9562 version-7 UUID (its leading
/// 48 bits are Unix milliseconds), or `None` for any other id shape. Codex
/// thread ids — and so rollout ids — are v7 and minted at launch, which
/// makes this a cheap, read-free "was this rollout born after our spawn?"
/// check. Agent-neutral: it inspects only the standard UUID layout, so a
/// non-v7 id simply opts out of the guard.
#[must_use]
pub fn uuid_v7_created_at(id: &str) -> Option<SystemTime> {
    let hex: String = id.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    if hex.as_bytes()[12] != b'7' {
        return None;
    }
    let millis = u64::from_str_radix(&hex[..12], 16).ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_millis(millis))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd(p: &str) -> PathBuf {
        PathBuf::from(p)
    }

    fn local() -> HostId {
        HostId::local()
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
        table.record(
            local(),
            cwd("/w/proj"),
            "nonce-1".into(),
            AgentKind::Codex,
            now,
        );
        let adopted = table.adopt(
            &local(),
            AgentKind::Codex,
            Path::new("/w/proj"),
            None,
            plus(now, 5),
        );
        assert_eq!(
            adopted,
            Some(PendingSpawn {
                host: local(),
                cwd: cwd("/w/proj"),
                nonce: "nonce-1".into(),
                agent: AgentKind::Codex,
                spawned_at: now,
                last_alive: now,
            })
        );
        // Consumed: a second adopt finds nothing.
        assert!(
            table
                .adopt(
                    &local(),
                    AgentKind::Codex,
                    Path::new("/w/proj"),
                    None,
                    plus(now, 6)
                )
                .is_none()
        );
        assert!(table.is_empty());
    }

    #[test]
    fn pending_then_expired_after_window() {
        let mut table = PendingSpawns::default();
        let now = t0();
        table.record(
            local(),
            cwd("/w/proj"),
            "nonce-1".into(),
            AgentKind::Codex,
            now,
        );
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
        table.record(
            local(),
            cwd("/w/proj"),
            "nonce-1".into(),
            AgentKind::Codex,
            now,
        );
        // Just past the window: adopt must decline (it belongs to sweep).
        let past = plus(now, ADOPTION_WINDOW.as_secs() + 1);
        assert!(
            table
                .adopt(&local(), AgentKind::Codex, Path::new("/w/proj"), None, past)
                .is_none()
        );
    }

    #[test]
    fn boundary_is_inclusive_live() {
        let mut table = PendingSpawns::default();
        let now = t0();
        table.record(
            local(),
            cwd("/w/proj"),
            "nonce-1".into(),
            AgentKind::Codex,
            now,
        );
        // Exactly at the window: still adoptable, not swept.
        let at = plus(now, ADOPTION_WINDOW.as_secs());
        assert!(table.sweep_expired(at).is_empty());
        assert!(
            table
                .adopt(&local(), AgentKind::Codex, Path::new("/w/proj"), None, at)
                .is_some()
        );
    }

    #[test]
    fn fifo_on_same_cwd() {
        let mut table = PendingSpawns::default();
        let now = t0();
        table.record(
            local(),
            cwd("/w/proj"),
            "first".into(),
            AgentKind::Codex,
            now,
        );
        table.record(
            local(),
            cwd("/w/proj"),
            "second".into(),
            AgentKind::Codex,
            plus(now, 1),
        );
        // First rollout adopts the earliest spawn…
        let a = table.adopt(
            &local(),
            AgentKind::Codex,
            Path::new("/w/proj"),
            None,
            plus(now, 2),
        );
        assert_eq!(a.unwrap().nonce, "first");
        // …the next rollout adopts the later one.
        let b = table.adopt(
            &local(),
            AgentKind::Codex,
            Path::new("/w/proj"),
            None,
            plus(now, 3),
        );
        assert_eq!(b.unwrap().nonce, "second");
        assert!(table.is_empty());
    }

    #[test]
    fn non_matching_agent_is_ignored() {
        // A Pi (or Claude) NewTranscript must never adopt a codex pending,
        // even in the same cwd within the window.
        let mut table = PendingSpawns::default();
        let now = t0();
        table.record(
            local(),
            cwd("/w/proj"),
            "nonce-1".into(),
            AgentKind::Codex,
            now,
        );
        assert!(
            table
                .adopt(
                    &local(),
                    AgentKind::Pi,
                    Path::new("/w/proj"),
                    None,
                    plus(now, 1)
                )
                .is_none()
        );
        // The codex entry is untouched and still adoptable by codex.
        assert!(
            table
                .adopt(
                    &local(),
                    AgentKind::Codex,
                    Path::new("/w/proj"),
                    None,
                    plus(now, 1)
                )
                .is_some()
        );
    }

    #[test]
    fn wrong_cwd_is_ignored() {
        let mut table = PendingSpawns::default();
        let now = t0();
        table.record(
            local(),
            cwd("/w/proj"),
            "nonce-1".into(),
            AgentKind::Codex,
            now,
        );
        assert!(
            table
                .adopt(
                    &local(),
                    AgentKind::Codex,
                    Path::new("/w/other"),
                    None,
                    plus(now, 1)
                )
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
                .adopt(&local(), AgentKind::Codex, Path::new("/w/proj"), None, t0())
                .is_none()
        );
    }

    #[test]
    fn sweep_only_removes_expired_and_keeps_live() {
        let mut table = PendingSpawns::default();
        let now = t0();
        table.record(local(), cwd("/old"), "old".into(), AgentKind::Codex, now);
        table.record(
            local(),
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
                    &local(),
                    AgentKind::Codex,
                    Path::new("/new"),
                    None,
                    plus(now, ADOPTION_WINDOW.as_secs())
                )
                .is_some()
        );
    }

    /// Simulate the pane poller: one live-panes snapshot every 3 s from
    /// `from` up to and including `to` (seconds past `base`), each listing
    /// `nonce` as alive on `host`.
    fn poll_alive(
        table: &mut PendingSpawns,
        host: &HostId,
        nonce: &str,
        base: SystemTime,
        from: u64,
        to: u64,
    ) {
        let mut s = from;
        while s <= to {
            table.observe_live(host, |n| n == nonce, plus(base, s));
            s += 3;
        }
    }

    #[test]
    fn live_pending_survives_past_window_then_adopts_late_rollout() {
        // Interactive codex writes no rollout until the first prompt: the
        // user dawdles for five minutes while the pending session stays
        // alive, then the rollout lands and must still adopt.
        let mut table = PendingSpawns::default();
        let now = t0();
        table.record(local(), cwd("/w/proj"), "n1".into(), AgentKind::Codex, now);
        poll_alive(&mut table, &local(), "n1", now, 3, 300);
        assert!(table.sweep_expired(plus(now, 301)).is_empty());
        let adopted = table.adopt(
            &local(),
            AgentKind::Codex,
            Path::new("/w/proj"),
            Some(plus(now, 1)),
            plus(now, 301),
        );
        assert_eq!(adopted.map(|p| p.nonce), Some("n1".to_string()));
    }

    #[test]
    fn pending_expires_once_its_session_stops_being_seen() {
        // Seen alive for a minute, then codex exits before any prompt: the
        // session drops out of every later snapshot and the entry expires
        // one window after the last sighting.
        let mut table = PendingSpawns::default();
        let now = t0();
        table.record(local(), cwd("/w/proj"), "n1".into(), AgentKind::Codex, now);
        poll_alive(&mut table, &local(), "n1", now, 3, 60);
        // Later snapshots list only other sessions.
        let mut s = 63;
        while s <= 120 {
            table.observe_live(&local(), |n| n == "someone-else", plus(now, s));
            s += 3;
        }
        assert!(
            table
                .sweep_expired(plus(now, 60 + ADOPTION_WINDOW.as_secs()))
                .is_empty(),
            "still within one window of the last sighting"
        );
        let expired = table.sweep_expired(plus(now, 61 + ADOPTION_WINDOW.as_secs()));
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].nonce, "n1");
    }

    #[test]
    fn a_single_empty_snapshot_does_not_kill_a_pending() {
        // The pane poller sends an empty snapshot when `tmux list-panes`
        // fails; one such blip mustn't expire a live pending.
        let mut table = PendingSpawns::default();
        let now = t0();
        table.record(local(), cwd("/w/proj"), "n1".into(), AgentKind::Codex, now);
        poll_alive(&mut table, &local(), "n1", now, 3, 27);
        table.observe_live(&local(), |_| false, plus(now, 30));
        poll_alive(&mut table, &local(), "n1", now, 33, 90);
        assert!(table.sweep_expired(plus(now, 91)).is_empty());
    }

    #[test]
    fn snapshots_from_another_host_do_not_keep_a_pending_alive() {
        let mut table = PendingSpawns::default();
        let now = t0();
        let remote = HostId("devbox".into());
        table.record(
            remote.clone(),
            cwd("/w/proj"),
            "n1".into(),
            AgentKind::Codex,
            now,
        );
        // Only the *local* poller reports a session by that name.
        poll_alive(&mut table, &local(), "n1", now, 3, 60);
        let expired = table.sweep_expired(plus(now, ADOPTION_WINDOW.as_secs() + 1));
        assert_eq!(expired.len(), 1);
    }

    #[test]
    fn rollout_on_another_host_is_not_adopted() {
        let mut table = PendingSpawns::default();
        let now = t0();
        table.record(local(), cwd("/w/proj"), "n1".into(), AgentKind::Codex, now);
        let remote = HostId("devbox".into());
        assert!(
            table
                .adopt(
                    &remote,
                    AgentKind::Codex,
                    Path::new("/w/proj"),
                    None,
                    plus(now, 1)
                )
                .is_none()
        );
    }

    #[test]
    fn hard_cap_expires_even_a_live_pending() {
        let mut table = PendingSpawns::default();
        let now = t0();
        table.record(local(), cwd("/w/proj"), "n1".into(), AgentKind::Codex, now);
        let cap = PENDING_HARD_CAP.as_secs();
        table.observe_live(&local(), |n| n == "n1", plus(now, cap + 1));
        let expired = table.sweep_expired(plus(now, cap + 2));
        assert_eq!(expired.len(), 1);
    }

    #[test]
    fn rollout_born_before_the_spawn_is_not_adopted() {
        // An unrelated codex launched in the same cwd ten minutes before our
        // spawn gets its first prompt now; its rollout (id minted at *its*
        // launch) must not steal our pending.
        let mut table = PendingSpawns::default();
        let now = t0();
        table.record(local(), cwd("/w/proj"), "n1".into(), AgentKind::Codex, now);
        let older = now - Duration::from_secs(600);
        assert!(
            table
                .adopt(
                    &local(),
                    AgentKind::Codex,
                    Path::new("/w/proj"),
                    Some(older),
                    plus(now, 5)
                )
                .is_none()
        );
        // Within the clock slack (remote skew) still adopts.
        let skewed = now - ID_CLOCK_SLACK + Duration::from_secs(1);
        assert!(
            table
                .adopt(
                    &local(),
                    AgentKind::Codex,
                    Path::new("/w/proj"),
                    Some(skewed),
                    plus(now, 5)
                )
                .is_some()
        );
    }

    #[test]
    fn rollout_older_than_a_later_spawn_is_not_pinned_on_it() {
        // Two live pendings in one cwd, spawned 5 min apart. A rollout
        // born *before* the second spawn (beyond the slack) can only be the
        // first one's; FIFO already picks it. The interesting case is the
        // reverse: with the first adopted, a rollout older than the second
        // spawn must not be pinned on it.
        let mut table = PendingSpawns::default();
        let now = t0();
        table.record(local(), cwd("/w/proj"), "a".into(), AgentKind::Codex, now);
        table.record(
            local(),
            cwd("/w/proj"),
            "b".into(),
            AgentKind::Codex,
            plus(now, 300),
        );
        let alive = |n: &str| n == "a" || n == "b";
        let mut s = 3;
        while s <= 400 {
            table.observe_live(&local(), alive, plus(now, s));
            s += 3;
        }
        let a = table.adopt(
            &local(),
            AgentKind::Codex,
            Path::new("/w/proj"),
            Some(plus(now, 1)),
            plus(now, 400),
        );
        assert_eq!(a.map(|p| p.nonce), Some("a".to_string()));
        // Another rollout born near the first spawn: not b's.
        assert!(
            table
                .adopt(
                    &local(),
                    AgentKind::Codex,
                    Path::new("/w/proj"),
                    Some(plus(now, 2)),
                    plus(now, 400)
                )
                .is_none()
        );
        // A rollout born after b's spawn is.
        let b = table.adopt(
            &local(),
            AgentKind::Codex,
            Path::new("/w/proj"),
            Some(plus(now, 301)),
            plus(now, 400),
        );
        assert_eq!(b.map(|p| p.nonce), Some("b".to_string()));
    }

    #[test]
    fn uuid_v7_created_at_reads_the_embedded_millis() {
        // A real codex 0.142.5 thread id (v7) from the 2026-10-02 probe.
        let at = uuid_v7_created_at("01a0fb75-6201-76e1-baef-961ad236621f").expect("v7");
        let millis = at
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        assert_eq!(millis, 0x01a0_fb75_6201);
        // v4 (claude-style) and malformed ids opt out of the guard.
        assert_eq!(
            uuid_v7_created_at("3f2b8c1e-9d4a-4e6f-8b21-7c5d9e0a1b2c"),
            None
        );
        assert_eq!(uuid_v7_created_at("not-a-uuid"), None);
    }
}
