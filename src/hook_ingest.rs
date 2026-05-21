//! Claude Code `Notification` hook ingress.
//!
//! Two halves, glued by a known directory under the user's cache root:
//!
//! 1. **Producer** ([`receive_hook_from_stdin`]): the `agent-mux hook`
//!    CLI subcommand. Claude Code spawns this as a hook command per
//!    its `~/.claude/settings.json` config, hands us the event JSON on
//!    stdin, and we write a marker file into the cache directory via
//!    atomic `tmp + rename`. Fire-and-forget — the subcommand process
//!    exits as soon as the rename completes, so Claude Code's hook
//!    pipeline never blocks on agent-mux's UI thread.
//!
//! 2. **Consumer** ([`spawn_hook_watcher`]): the dashboard process. A
//!    `notify`-backed watch on the same directory ingests new marker
//!    files, parses them, and emits [`WatcherEvent::Hook`] into the
//!    main event channel. The main loop forwards to
//!    `SessionCatalog::apply_hook_event`, which forces `NeedsInput`
//!    and pins hook authority for the affected session.
//!
//! The cache file is the synchronisation point. If the dashboard isn't
//! running when a hook fires, the marker stays on disk until the next
//! startup (the watcher's initial sweep picks it up). If two hooks
//! fire in quick succession, both markers land independently and the
//! notifier's episodic-flag suppression collapses the duplicate
//! `NeedsInput` dispatches.
//!
//! ## Why file-based, not socket/HTTP
//!
//! The hook command is a separate process. File-based ingress means we
//! don't have to stand up a long-running server inside the dashboard
//! or worry about socket placement / port conflicts. The cost is one
//! filesystem round-trip per event, which is well under the human
//! perception threshold for "the notification fired."
//!
//! ## What's deliberately out of scope (Phase 1)
//!
//! - Remote sessions. The hook runs on the machine where `claude`
//!   runs; for remotes that's the remote box, which has no way to
//!   reach this local cache directory. Phase 2 will write markers
//!   under the remote's `transcript_root` so the existing SSH-backed
//!   poller picks them up.
//! - Auto-installing the hook into `~/.claude/settings.json`. Phase 1
//!   asks the user to edit it themselves; a dedicated `agent-mux
//!   install-hooks` subcommand is filed under TODO.
//! - Matcher differentiation (`permission_prompt` vs `idle_prompt`).
//!   Any Notification event for a known `session_id` fires
//!   `NeedsInput` today.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::{Duration, SystemTime};

use crate::session::SessionId;
use crate::watcher::WatcherEvent;

/// Directory under the user's cache root where the hook subcommand
/// writes marker files and the dashboard watches for them. Picked at
/// runtime via [`default_hook_dir`] (uses `dirs::cache_dir()`).
pub const HOOK_DIR_NAME: &str = "agent-mux/hooks";

/// Default per-platform hook directory: `~/Library/Caches/agent-mux/hooks`
/// on macOS, `$XDG_CACHE_HOME/agent-mux/hooks` (typically
/// `~/.cache/agent-mux/hooks`) on Linux. `None` when no cache root
/// resolves — the subcommand surfaces this loudly rather than silently
/// writing to `$PWD`.
#[must_use]
pub fn default_hook_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|c| c.join("agent-mux").join("hooks"))
}

/// One Claude Code `Notification` hook event, distilled to the fields
/// agent-mux actually uses. The full incoming JSON is preserved as
/// `raw` so future fields (matcher value, `transcript_path`) can be
/// surfaced without changing the marker format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookEvent {
    pub session_id: SessionId,
    pub received_at: SystemTime,
    /// The raw payload as it landed on stdin (or whatever the subset
    /// of fields the subcommand chose to persist). Today we only care
    /// about `session_id`; carried so future readers can extract more.
    pub raw_json: String,
}

/// Read a Claude Code hook payload from `stdin_reader`, persist it to
/// the cache directory as a marker file the dashboard's watcher will
/// ingest, and return the resolved marker path. Production callers
/// pass `io::stdin().lock()`; tests pass an in-memory cursor.
///
/// The marker file name is `<unix-millis>-<session_id>.json` so file
/// ordering on disk matches event ordering (the timestamp prefix
/// sorts lexically and the session id makes accidental collisions
/// unique). Written via `<name>.tmp` + rename to ensure the watcher
/// never sees a half-written file.
///
/// # Errors
///
/// Propagates I/O errors from stdin read or the marker write. A
/// payload without a `session_id` field returns
/// [`io::ErrorKind::InvalidData`] — we have nothing to correlate to a
/// catalog session, so dropping silently would be worse than failing
/// loudly.
pub fn receive_hook_from_stdin<R: Read>(
    stdin_reader: &mut R,
    hook_dir: &Path,
    now: SystemTime,
) -> io::Result<PathBuf> {
    let mut buf = String::new();
    stdin_reader.read_to_string(&mut buf)?;
    let session_id = parse_session_id(&buf)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing session_id field"))?;
    persist_marker(hook_dir, &session_id, now, &buf)
}

/// Pull `session_id` out of the hook JSON. Tolerant of surrounding
/// whitespace and accepts both `"session_id"` and `"sessionId"`
/// spellings (Claude Code documents the `snake_case` form but a
/// future schema bump or fork could swap conventions and silent
/// failure would be a hard-to-debug regression).
#[must_use]
fn parse_session_id(payload: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let obj = v.as_object()?;
    let raw = obj
        .get("session_id")
        .or_else(|| obj.get("sessionId"))?
        .as_str()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

/// Atomic marker-file write. Creates `hook_dir` if absent (the first
/// hook fire on a fresh install would otherwise fail). Writes content
/// to `<name>.tmp`, fsyncs, renames to `<name>` — the rename is
/// atomic on the same filesystem so the watcher never sees a partial
/// JSON line.
fn persist_marker(
    hook_dir: &Path,
    session_id: &str,
    now: SystemTime,
    payload: &str,
) -> io::Result<PathBuf> {
    fs::create_dir_all(hook_dir)?;
    let stamp = unix_millis(now);
    // Strip filesystem-hostile characters from session_id defensively.
    // Claude Code's session ids are UUIDs in practice so this is a
    // belt-and-braces — a future schema where ids carry slashes
    // would otherwise create unwanted subdirectories.
    let safe_id: String = session_id
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = format!("{stamp:013}-{safe_id}.json");
    let final_path = hook_dir.join(&name);
    let tmp_path = hook_dir.join(format!("{name}.tmp"));
    {
        let mut f = File::create(&tmp_path)?;
        f.write_all(payload.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp_path, &final_path)?;
    Ok(final_path)
}

fn unix_millis(t: SystemTime) -> u128 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Parse a marker file into a [`HookEvent`]. Used by both the
/// startup-sweep path and the live notify-event path.
///
/// # Errors
///
/// Returns `io::Error` for unreadable files or payloads without a
/// `session_id`.
pub fn parse_marker(path: &Path) -> io::Result<HookEvent> {
    let raw = fs::read_to_string(path)?;
    let session_id = parse_session_id(&raw)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing session_id field"))?;
    // `received_at` comes from the filename's millisecond prefix when
    // present, falling back to the file's mtime. Filename-derived is
    // the source of truth (it's what the producer stamped); mtime is
    // only used if a marker arrived from somewhere that didn't use
    // `persist_marker`.
    let received_at = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.split('-').next())
        .and_then(|s| s.parse::<u64>().ok())
        .map(|millis| SystemTime::UNIX_EPOCH + Duration::from_millis(millis))
        .or_else(|| fs::metadata(path).and_then(|m| m.modified()).ok())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    Ok(HookEvent {
        session_id: SessionId(session_id),
        received_at,
        raw_json: raw,
    })
}

/// Spawn the background hook-marker watcher. A `notify` watch on
/// `hook_dir` translates new marker files into [`WatcherEvent::Hook`]
/// events on the shared channel. Marker files are deleted after a
/// successful ingest so the directory doesn't grow without bound — a
/// failed ingest leaves the marker in place so the next startup
/// retries it via the initial sweep.
///
/// Returns the `notify::RecommendedWatcher` so the caller can hold it
/// for the dashboard's lifetime; dropping it tears the backend down.
///
/// # Errors
///
/// Surfaces any error from creating the watcher or the initial
/// directory scan. The directory itself is created if absent.
pub fn spawn_hook_watcher(
    hook_dir: &Path,
    event_tx: &Sender<WatcherEvent>,
) -> notify::Result<notify::RecommendedWatcher> {
    use notify::{Event, EventKind, RecursiveMode, Watcher};

    fs::create_dir_all(hook_dir).map_err(notify::Error::io)?;
    sweep_existing_markers(hook_dir, event_tx);

    let tx_for_handler = event_tx.clone();
    let dir_for_handler = hook_dir.to_path_buf();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
        // Watcher errors are best-effort: a notify hiccup
        // shouldn't kill the dashboard. Subsequent events
        // recover; the startup sweep on next launch catches
        // anything that landed during a blackout window.
        if let Ok(ev) = result {
            if !matches!(ev.kind, EventKind::Create(_) | EventKind::Modify(_)) {
                return;
            }
            for path in ev.paths {
                ingest_marker(&dir_for_handler, &path, &tx_for_handler);
            }
        }
    })?;
    watcher.watch(hook_dir, RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

/// On startup, drain any marker files that landed while the dashboard
/// wasn't running. Same parse + emit path as the live watcher; failed
/// parses leave the offending file in place for human inspection.
fn sweep_existing_markers(hook_dir: &Path, tx: &Sender<WatcherEvent>) {
    let Ok(entries) = fs::read_dir(hook_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        ingest_marker(hook_dir, &path, tx);
    }
}

/// Parse one marker file and emit the corresponding event. On success
/// delete the file (so the directory doesn't grow without bound); on
/// failure leave it in place so a developer can inspect the bad
/// payload after the fact.
fn ingest_marker(hook_dir: &Path, path: &Path, tx: &Sender<WatcherEvent>) {
    // Ignore `.tmp` files — they're mid-write atomic-rename artifacts
    // that the watcher might glimpse via Create event before the rename
    // lands. The real marker arrives moments later as the rename target.
    if path.extension().is_some_and(|e| e == "tmp") {
        return;
    }
    if path.parent() != Some(hook_dir) {
        return;
    }
    let Ok(event) = parse_marker(path) else {
        return;
    };
    let _ = tx.send(WatcherEvent::Hook {
        id: event.session_id,
        received_at: event.received_at,
    });
    let _ = fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::mpsc;
    use tempfile::TempDir;

    #[test]
    fn receive_writes_marker_with_session_id_in_filename() {
        let tmp = TempDir::new().unwrap();
        let payload = r#"{"session_id":"abc-123","hook_event_name":"Notification"}"#;
        let mut input = Cursor::new(payload.as_bytes());
        let path = receive_hook_from_stdin(
            &mut input,
            tmp.path(),
            SystemTime::UNIX_EPOCH + Duration::from_millis(1_234_567),
        )
        .expect("write marker");
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(name.ends_with("-abc-123.json"), "unexpected name {name:?}");
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents, payload);
    }

    #[test]
    fn receive_accepts_camel_case_session_id() {
        let tmp = TempDir::new().unwrap();
        let payload = r#"{"sessionId":"abc"}"#;
        let mut input = Cursor::new(payload.as_bytes());
        let path = receive_hook_from_stdin(&mut input, tmp.path(), SystemTime::UNIX_EPOCH).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn receive_rejects_payload_without_session_id() {
        let tmp = TempDir::new().unwrap();
        let mut input = Cursor::new(br#"{"hook_event_name":"Notification"}"#.as_slice());
        let err = receive_hook_from_stdin(&mut input, tmp.path(), SystemTime::UNIX_EPOCH)
            .expect_err("should reject");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn receive_rejects_malformed_json() {
        let tmp = TempDir::new().unwrap();
        let mut input = Cursor::new(b"not valid json".as_slice());
        assert!(receive_hook_from_stdin(&mut input, tmp.path(), SystemTime::UNIX_EPOCH).is_err());
    }

    #[test]
    fn receive_sanitises_session_id_against_filesystem_hostile_characters() {
        // Defensive: today's session ids are UUIDs, but a future
        // schema with slashes would otherwise create subdirectories.
        let tmp = TempDir::new().unwrap();
        let payload = r#"{"session_id":"weird/id with spaces"}"#;
        let mut input = Cursor::new(payload.as_bytes());
        let path = receive_hook_from_stdin(&mut input, tmp.path(), SystemTime::UNIX_EPOCH).unwrap();
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(!name.contains('/'), "slash leaked into filename: {name:?}");
        assert!(!name.contains(' '), "space leaked into filename: {name:?}");
    }

    #[test]
    fn parse_marker_pulls_received_at_from_filename_prefix() {
        let tmp = TempDir::new().unwrap();
        let payload = r#"{"session_id":"x"}"#;
        let path = tmp.path().join("0000001234567-x.json");
        fs::write(&path, payload).unwrap();
        let ev = parse_marker(&path).unwrap();
        assert_eq!(ev.session_id.0, "x");
        assert_eq!(
            ev.received_at,
            SystemTime::UNIX_EPOCH + Duration::from_millis(1_234_567),
        );
    }

    #[test]
    fn sweep_emits_events_for_existing_markers_at_startup() {
        let tmp = TempDir::new().unwrap();
        let payload = r#"{"session_id":"sweep-target"}"#;
        let path = tmp.path().join("0000000000001-sweep-target.json");
        fs::write(&path, payload).unwrap();
        let (tx, rx) = mpsc::channel();
        sweep_existing_markers(tmp.path(), &tx);
        let evs: Vec<_> = rx.try_iter().collect();
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            WatcherEvent::Hook { id, .. } => assert_eq!(id.0, "sweep-target"),
            other => panic!("expected Hook, got {other:?}"),
        }
        // Successful ingest deletes the marker so the next sweep
        // doesn't re-emit it.
        assert!(!path.exists(), "marker should be deleted after ingest");
    }

    #[test]
    fn ingest_skips_tmp_files() {
        // The atomic-rename producer writes to `<name>.tmp` then
        // renames. The notify backend can glimpse the .tmp via Create
        // before the rename; we must not ingest those.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("0000-x.json.tmp");
        fs::write(&path, r#"{"session_id":"x"}"#).unwrap();
        let (tx, rx) = mpsc::channel();
        ingest_marker(tmp.path(), &path, &tx);
        assert_eq!(rx.try_iter().count(), 0);
        assert!(path.exists(), ".tmp should not be deleted by ingest");
    }

    #[test]
    fn ingest_ignores_files_outside_the_hook_dir() {
        // Belt-and-braces: a notify Event whose paths somehow include
        // a file outside hook_dir shouldn't get ingested or deleted.
        let tmp = TempDir::new().unwrap();
        let other_dir = tmp.path().join("other");
        fs::create_dir(&other_dir).unwrap();
        let path = other_dir.join("0000-x.json");
        fs::write(&path, r#"{"session_id":"x"}"#).unwrap();
        let (tx, rx) = mpsc::channel();
        ingest_marker(tmp.path(), &path, &tx);
        assert_eq!(rx.try_iter().count(), 0);
        assert!(path.exists(), "out-of-dir file must not be deleted");
    }

    #[test]
    fn ingest_leaves_unparseable_markers_in_place_for_inspection() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("0000-bad.json");
        fs::write(&path, "garbage").unwrap();
        let (tx, rx) = mpsc::channel();
        ingest_marker(tmp.path(), &path, &tx);
        assert_eq!(rx.try_iter().count(), 0);
        assert!(path.exists(), "bad marker should stay for human inspection");
    }
}
