//! Per-host disk cache of the last-known remote session list.
//!
//! The dashboard's startup feels instantaneous for local sessions
//! (a single `read_dir`) but pops in over multiple seconds for
//! remote ones, because each `[hosts.<name>]` has to open an SSH
//! `ControlMaster` and `find` the remote transcript directory before
//! anything is renderable. That latency compounds across hosts.
//!
//! This module trades a small staleness window for first-paint
//! responsiveness: after every successful remote discovery the
//! sessions are snapshotted to `~/.cache/agent-mux/sessions/<host>.json`,
//! and on next startup the dashboard renders the snapshot immediately
//! while the live connect runs in the background. When the live
//! discovery completes, [`crate::catalog::SessionCatalog::reconcile_host`]
//! drops cache entries the host no longer has and overlays live
//! state on the rest.
//!
//! Design notes:
//! - One file per host. A corrupt or unreadable file for one host
//!   must not poison the cache for the others.
//! - JSON over a parallel `CachedSession` struct rather than serde
//!   on [`Session`] directly. Keeps the wire format independent of
//!   the in-memory type — fields can be added to `Session` without
//!   bumping a schema version, and the cache file remains
//!   human-inspectable for debugging.
//! - Atomic write via `tmp` + `rename` so a crashed write never
//!   leaves a half-truncated file the next startup will choke on.
//! - All errors are best-effort: a missing, unreadable, or corrupt
//!   cache file silently returns an empty session list. The live
//!   discovery will repopulate it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::session::{Attention, HostId, Session, SessionId};

/// Location of the per-host snapshot directory. Returns `None` only
/// if the platform has no usable cache dir (no `$XDG_CACHE_HOME` /
/// `$HOME`); callers treat that as "caching disabled" and continue.
#[must_use]
pub fn default_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("agent-mux").join("sessions"))
}

/// Read the cached session list for `host` from `dir`. Returns an
/// empty vec for any failure (missing, unreadable, corrupt) — the
/// cache is strictly an optimisation, not a source of truth.
#[must_use]
pub fn read_for_host(dir: &Path, host: &HostId) -> Vec<Session> {
    let path = cache_file(dir, host);
    let Ok(bytes) = fs::read(&path) else {
        return Vec::new();
    };
    let cached: Vec<CachedSession> = match serde_json::from_slice(&bytes) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    cached.into_iter().map(|c| c.into_session(host)).collect()
}

/// Atomically write `sessions` for `host` into `dir`. Creates the
/// directory if missing. Errors propagate so the caller can decide
/// whether to surface them (currently they're swallowed — see
/// caller in `main.rs::connect_and_discover`).
///
/// # Errors
/// Returns the underlying `io::Error` if the cache directory cannot
/// be created, the temp file cannot be written, the rename to the
/// final path fails, or `serde_json` rejects the session list.
pub fn write_for_host(dir: &Path, host: &HostId, sessions: &[Session]) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let path = cache_file(dir, host);
    let tmp = path.with_extension("json.tmp");
    let cached: Vec<CachedSession> = sessions.iter().map(CachedSession::from_session).collect();
    let bytes = serde_json::to_vec_pretty(&cached).map_err(io::Error::other)?;
    fs::write(&tmp, bytes)?;
    fs::rename(tmp, path)?;
    Ok(())
}

fn cache_file(dir: &Path, host: &HostId) -> PathBuf {
    dir.join(format!("{}.json", host.as_str()))
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedSession {
    id: String,
    project_dir: PathBuf,
    transcript_path: PathBuf,
    /// Unix epoch seconds. Signed so a clock-skew remote that
    /// produces a pre-1970 timestamp deserialises without erroring;
    /// the conversion in [`epoch_secs_to_systemtime`] clamps to
    /// `UNIX_EPOCH` rather than panicking.
    last_activity_secs: i64,
    title: Option<String>,
    attention: CachedAttention,
    /// Parent repo path when `project_dir` is a git worktree. Optional
    /// and marked `#[serde(default)]` so caches written by earlier
    /// builds (no such field) still deserialise; those rows degrade to
    /// grouping by `project_dir` until the next live discovery
    /// refreshes them with the actual parent.
    #[serde(default)]
    parent_repo: Option<PathBuf>,
}

/// Parallel of [`Attention`] for the wire format. Lives here (not
/// derived on `Attention` itself) so the in-memory enum can evolve
/// independently of the on-disk schema.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CachedAttention {
    NeedsInput,
    Working,
    Idle,
    Unknown,
}

impl CachedSession {
    fn from_session(s: &Session) -> Self {
        Self {
            id: s.id.0.clone(),
            project_dir: s.project_dir.clone(),
            transcript_path: s.transcript_path.clone(),
            last_activity_secs: systemtime_to_epoch_secs(s.last_activity),
            title: s.title.clone(),
            attention: s.attention.into(),
            parent_repo: s.parent_repo.clone(),
        }
    }

    fn into_session(self, host: &HostId) -> Session {
        Session {
            id: SessionId(self.id),
            host: host.clone(),
            project_dir: self.project_dir,
            transcript_path: self.transcript_path,
            last_activity: epoch_secs_to_systemtime(self.last_activity_secs),
            attention: self.attention.into(),
            title: self.title,
            parent_repo: self.parent_repo,
            // Tmux pane state is ephemeral; the runtime pane poller
            // will set this on its first tick. Anything cached here
            // would be stale before the user could read it.
            has_live_pane: None,
            hook_pinned: None,
        }
    }
}

impl From<Attention> for CachedAttention {
    fn from(a: Attention) -> Self {
        match a {
            Attention::NeedsInput => Self::NeedsInput,
            Attention::Working => Self::Working,
            Attention::Idle => Self::Idle,
            Attention::Unknown => Self::Unknown,
        }
    }
}

impl From<CachedAttention> for Attention {
    fn from(a: CachedAttention) -> Self {
        match a {
            CachedAttention::NeedsInput => Self::NeedsInput,
            CachedAttention::Working => Self::Working,
            CachedAttention::Idle => Self::Idle,
            CachedAttention::Unknown => Self::Unknown,
        }
    }
}

fn systemtime_to_epoch_secs(t: SystemTime) -> i64 {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

fn epoch_secs_to_systemtime(s: i64) -> SystemTime {
    if s <= 0 {
        return SystemTime::UNIX_EPOCH;
    }
    // Cast safe: positive i64 fits in u64.
    #[allow(clippy::cast_sign_loss)]
    let secs = s as u64;
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_session(id: &str, host: &HostId) -> Session {
        Session {
            id: SessionId(id.to_string()),
            host: host.clone(),
            project_dir: PathBuf::from(format!("/proj/{id}")),
            transcript_path: PathBuf::from(format!("/t/{id}.jsonl")),
            last_activity: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            attention: Attention::NeedsInput,
            title: Some(format!("task {id}")),
            parent_repo: Some(PathBuf::from(format!("/repos/{id}"))),
            has_live_pane: None,
            hook_pinned: None,
        }
    }

    #[test]
    fn read_missing_file_returns_empty() {
        let dir = TempDir::new().unwrap();
        let sessions = read_for_host(dir.path(), &HostId("never-cached".into()));
        assert!(sessions.is_empty());
    }

    #[test]
    fn read_corrupt_file_returns_empty_without_panicking() {
        let dir = TempDir::new().unwrap();
        let host = HostId("oops".into());
        fs::write(cache_file(dir.path(), &host), b"{not valid json").unwrap();
        let sessions = read_for_host(dir.path(), &host);
        assert!(sessions.is_empty());
    }

    #[test]
    fn write_creates_parent_directory_if_missing() {
        let parent = TempDir::new().unwrap();
        let nested = parent.path().join("does").join("not").join("exist");
        let host = HostId("alpha".into());
        let s = sample_session("a", &host);
        write_for_host(&nested, &host, &[s]).unwrap();
        assert!(nested.join("alpha.json").exists());
    }

    #[test]
    fn write_then_read_round_trips_every_field() {
        let dir = TempDir::new().unwrap();
        let host = HostId("alpha".into());
        let s = sample_session("a", &host);
        write_for_host(dir.path(), &host, std::slice::from_ref(&s)).unwrap();
        let read = read_for_host(dir.path(), &host);
        assert_eq!(read.len(), 1);
        let r = &read[0];
        assert_eq!(r.id, s.id);
        assert_eq!(r.host, s.host);
        assert_eq!(r.project_dir, s.project_dir);
        assert_eq!(r.transcript_path, s.transcript_path);
        assert_eq!(r.last_activity, s.last_activity);
        assert_eq!(r.attention, s.attention);
        assert_eq!(r.title, s.title);
        assert_eq!(r.parent_repo, s.parent_repo);
    }

    #[test]
    fn read_legacy_cache_without_parent_repo_field_still_parses() {
        // Caches written by builds before parent_repo landed lack the
        // field. `#[serde(default)]` on `CachedSession.parent_repo`
        // lets them deserialise — those rows degrade to grouping by
        // `project_dir` until the next live discovery refreshes them.
        // Pin that contract so a future serde tweak can't silently
        // break first-paint for users with stale caches on disk.
        let dir = TempDir::new().unwrap();
        let host = HostId("legacy".into());
        let legacy_json = r#"[
            {
                "id": "old",
                "project_dir": "/proj/old",
                "transcript_path": "/t/old.jsonl",
                "last_activity_secs": 1700000000,
                "title": null,
                "attention": "idle"
            }
        ]"#;
        fs::write(cache_file(dir.path(), &host), legacy_json).unwrap();
        let read = read_for_host(dir.path(), &host);
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].parent_repo, None);
    }

    #[test]
    fn round_trip_preserves_all_attention_variants() {
        let dir = TempDir::new().unwrap();
        let host = HostId("h".into());
        for a in [
            Attention::NeedsInput,
            Attention::Working,
            Attention::Idle,
            Attention::Unknown,
        ] {
            let mut s = sample_session("a", &host);
            s.attention = a;
            write_for_host(dir.path(), &host, &[s]).unwrap();
            let read = read_for_host(dir.path(), &host);
            assert_eq!(read[0].attention, a);
        }
    }

    #[test]
    fn round_trip_preserves_title_none_distinct_from_empty_string() {
        let dir = TempDir::new().unwrap();
        let host = HostId("h".into());
        let mut s = sample_session("a", &host);
        s.title = None;
        write_for_host(dir.path(), &host, &[s]).unwrap();
        assert_eq!(read_for_host(dir.path(), &host)[0].title, None);

        let mut s = sample_session("a", &host);
        s.title = Some(String::new());
        write_for_host(dir.path(), &host, &[s]).unwrap();
        assert_eq!(
            read_for_host(dir.path(), &host)[0].title,
            Some(String::new())
        );
    }

    #[test]
    fn write_overwrites_previous_snapshot() {
        let dir = TempDir::new().unwrap();
        let host = HostId("h".into());
        write_for_host(dir.path(), &host, &[sample_session("a", &host)]).unwrap();
        write_for_host(
            dir.path(),
            &host,
            &[sample_session("b", &host), sample_session("c", &host)],
        )
        .unwrap();
        let read = read_for_host(dir.path(), &host);
        let ids: Vec<&str> = read.iter().map(|s| s.id.0.as_str()).collect();
        assert_eq!(ids, vec!["b", "c"]);
    }

    #[test]
    fn per_host_files_do_not_interfere() {
        let dir = TempDir::new().unwrap();
        let a = HostId("alpha".into());
        let b = HostId("beta".into());
        write_for_host(dir.path(), &a, &[sample_session("a1", &a)]).unwrap();
        write_for_host(dir.path(), &b, &[sample_session("b1", &b)]).unwrap();
        assert_eq!(read_for_host(dir.path(), &a)[0].id.0, "a1");
        assert_eq!(read_for_host(dir.path(), &b)[0].id.0, "b1");
    }

    #[test]
    fn write_for_host_then_remove_file_returns_empty_on_next_read() {
        // Defensive: ensure we don't accidentally cache state in
        // memory between calls — every read goes back to disk.
        let dir = TempDir::new().unwrap();
        let host = HostId("h".into());
        write_for_host(dir.path(), &host, &[sample_session("a", &host)]).unwrap();
        fs::remove_file(cache_file(dir.path(), &host)).unwrap();
        assert!(read_for_host(dir.path(), &host).is_empty());
    }

    #[test]
    fn write_is_atomic_via_tmp_file() {
        // Implementation detail worth pinning: a partial write must
        // not leave the final file truncated. We can't easily simulate
        // crash mid-write, but we can verify the tmp pattern by
        // checking that after a successful write, no `.tmp` file
        // remains alongside the real one.
        let dir = TempDir::new().unwrap();
        let host = HostId("h".into());
        write_for_host(dir.path(), &host, &[sample_session("a", &host)]).unwrap();
        let leftover_tmp = cache_file(dir.path(), &host).with_extension("json.tmp");
        assert!(!leftover_tmp.exists(), "tmp file should be renamed away");
    }
}
