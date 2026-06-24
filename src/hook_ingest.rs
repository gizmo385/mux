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
//!
//! ## Blocking-prompt classification
//!
//! Every ingested event fires `NeedsInput`. The `notification_type` is
//! also classified ([`is_blocking_prompt`]) into "blocking prompt"
//! (`permission_prompt` / `elicitation_dialog`) vs "idle nudge", which
//! drives the sidebar's "answer me" vs "done" glyph — but not whether a
//! notification fires (both do).

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::{Duration, SystemTime};

use crate::session::SessionId;
use crate::watcher::WatcherEvent;

/// Subdirectory name (relative to the Claude Code transcripts root)
/// where hook markers land. The dot prefix keeps it out of the
/// `<root>/<project-hash>/` layout the existing transcript poller
/// walks for `.jsonl` files. Local sessions land in the local
/// `<root>/.agent-mux-hooks/`; remote sessions land in the remote's
/// `<remote-root>/.agent-mux-hooks/` and the per-host SSH poller
/// picks them up.
pub const HOOK_SUBDIR: &str = ".agent-mux-hooks";

/// Compute the hook-marker directory for a Claude Code transcripts
/// root. Used both by the dashboard's local watcher (against the
/// locally-resolved transcripts root) and by the remote-poller
/// addition (against each `[hosts.<name>].transcript_root`). The
/// unified path means a single `agent-mux hook` subcommand
/// implementation works identically on local and remote machines.
#[must_use]
pub fn hook_dir_for_transcripts_root(root: &Path) -> PathBuf {
    root.join(HOOK_SUBDIR)
}

/// Best-effort cache-dir fallback used only when the hook payload
/// arrives without a `transcript_path` field — production Claude Code
/// payloads always include it, so this path exists for malformed /
/// hand-crafted callers (e.g. our own smoke tests) and is
/// deliberately not where real markers land.
///
/// Returns `~/Library/Caches/agent-mux/hooks` on macOS,
/// `$XDG_CACHE_HOME/agent-mux/hooks` on Linux, or `None` when no
/// cache root resolves.
#[must_use]
pub fn fallback_hook_dir() -> Option<PathBuf> {
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
    /// Whether this event is a *blocking prompt* (`permission_prompt` /
    /// `elicitation_dialog`) — the agent is waiting on a specific
    /// answer — versus an idle nudge or an unlabelled event. Drives the
    /// session's `blocking_prompt` flag and the sidebar's "answer me"
    /// glyph; does not change whether a notification fires.
    pub blocking_prompt: bool,
    /// The raw payload as it landed on stdin (or whatever the subset
    /// of fields the subcommand chose to persist). Carried so future
    /// readers can extract more without changing the marker format.
    pub raw_json: String,
}

/// Read a Claude Code hook payload from `stdin_reader`, decide whether
/// it represents a user-input-required event, and (if so) persist it
/// to the cache directory as a marker file the dashboard's watcher
/// will ingest. Production callers pass `io::stdin().lock()`; tests
/// pass an in-memory cursor.
///
/// Returns `Ok(Some(path))` when a marker was written, `Ok(None)` when
/// the event was recognised but filtered out (e.g. `auth_success` —
/// fires on every successful authentication, not a user attention
/// signal). In both cases the `notification_type` value is logged to
/// `stderr_log` so the user can see (in shell scrollback) what kinds
/// of events Claude Code is actually emitting on their setup — that's
/// the dogfooding lever for refining the allowlist over time.
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
pub fn receive_hook_from_stdin<R: Read, W: Write>(
    stdin_reader: &mut R,
    fallback_dir: &Path,
    now: SystemTime,
    stderr_log: &mut W,
) -> io::Result<Option<PathBuf>> {
    let mut buf = String::new();
    stdin_reader.read_to_string(&mut buf)?;
    let session_id = parse_session_id(&buf)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing session_id field"))?;
    let notification_type = parse_notification_type(&buf);
    let type_label = notification_type.as_deref().unwrap_or("<missing>");
    if is_input_required_type(notification_type.as_deref()) {
        let target_dir =
            target_dir_from_payload(&buf).unwrap_or_else(|| fallback_dir.to_path_buf());
        let _ = writeln!(
            stderr_log,
            "agent-mux: hook notification_type={type_label} session_id={session_id} \u{2192} writing marker to {}",
            target_dir.display()
        );
        let path = persist_marker(&target_dir, &session_id, now, &buf)?;
        Ok(Some(path))
    } else {
        let _ = writeln!(
            stderr_log,
            "agent-mux: hook notification_type={type_label} session_id={session_id} \u{2192} skipped (not input-required)"
        );
        Ok(None)
    }
}

/// Derive the marker target directory from a hook payload's
/// `transcript_path` field. Real Claude Code payloads always include
/// this — the path is `<transcripts-root>/<project-hash>/<session-id>.jsonl`,
/// so the transcripts root is the parent of the parent, and the hook
/// dir is `<root>/.agent-mux-hooks/`.
///
/// Returns `None` when the payload doesn't carry a usable
/// `transcript_path` (malformed payloads, hand-crafted test calls,
/// future schema changes). The caller falls back to its
/// platform-defined location.
#[must_use]
fn target_dir_from_payload(payload: &str) -> Option<PathBuf> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let transcript_path = v.as_object()?.get("transcript_path")?.as_str()?;
    let root = Path::new(transcript_path).parent()?.parent()?;
    Some(hook_dir_for_transcripts_root(root))
}

/// Notification types we treat as "user input is required for this
/// session." Anything in this allowlist surfaces as a `NeedsInput`
/// transition in the dashboard; anything else is logged and dropped.
///
/// The allowlist is hardcoded rather than configurable because (a)
/// the per-platform name space is finite and documented by Claude
/// Code, and (b) the user-facing tuning knob would be redundant —
/// disabling notifications altogether is already covered by
/// `[notifications] enabled = false`. If a future Claude Code adds a
/// new input-required notification type, dogfooding will surface it
/// via the `stderr_log` line above and we add it here.
///
/// Excluded on purpose:
/// - `auth_success` — fires on every successful authentication,
///   not a user attention signal.
/// - `elicitation_complete` / `elicitation_response` — those are
///   *post-input* events. The matching `elicitation_dialog` event is
///   the input-required signal; once the user has acted, the
///   completion event re-firing a notification would be redundant
///   noise.
const INPUT_REQUIRED_NOTIFICATION_TYPES: &[&str] =
    &["permission_prompt", "idle_prompt", "elicitation_dialog"];

/// The subset of input-required types that mean the agent is *blocked
/// waiting on a specific answer* (a permission request or an
/// elicitation dialog), as distinct from an idle nudge. These flip the
/// session's `blocking_prompt` so the sidebar shows an "answer me"
/// glyph instead of the generic "done / waiting" one. `idle_prompt`
/// and an unlabelled event stay `false`: input is wanted, but it's not
/// a blocking question.
const BLOCKING_PROMPT_NOTIFICATION_TYPES: &[&str] = &["permission_prompt", "elicitation_dialog"];

/// True iff `notification_type` denotes a blocking prompt (see
/// [`BLOCKING_PROMPT_NOTIFICATION_TYPES`]). A missing/unknown type is
/// `false` — we know input is wanted (it passed the input-required
/// filter) but not that the agent is blocked on a specific answer, so
/// the conservative display is the generic "done" glyph.
#[must_use]
fn is_blocking_prompt(notification_type: Option<&str>) -> bool {
    notification_type.is_some_and(|t| BLOCKING_PROMPT_NOTIFICATION_TYPES.contains(&t))
}

/// True iff `notification_type` should fire a `NeedsInput` notification.
/// A missing value (`None`) is treated as input-required: an unknown
/// payload shape should stay loud rather than silently quiet (matches
/// the conservative-fallback pattern in `derive_attention_from_content`).
#[must_use]
fn is_input_required_type(notification_type: Option<&str>) -> bool {
    match notification_type {
        None => true,
        Some(t) => INPUT_REQUIRED_NOTIFICATION_TYPES.contains(&t),
    }
}

/// Pull `notification_type` out of the hook JSON. Returns `None` when
/// the field is absent, non-string, or empty — caller decides what to
/// do with that (currently: treat as input-required, see
/// [`is_input_required_type`]).
#[must_use]
fn parse_notification_type(payload: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let raw = v.as_object()?.get("notification_type")?.as_str()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
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
        .map_or(0, |d| d.as_millis())
}

/// Parse a marker file into a [`HookEvent`]. Used by the local
/// startup-sweep + notify-event path; the remote-host poller uses
/// [`parse_marker_content`] instead because it already has the file
/// content in hand (via `Host::read_many`).
///
/// # Errors
///
/// Returns `io::Error` for unreadable files or payloads without a
/// `session_id`.
pub fn parse_marker(path: &Path) -> io::Result<HookEvent> {
    let raw = fs::read_to_string(path)?;
    parse_marker_content(path, &raw)
}

/// Parse hook-event metadata out of an already-read marker payload.
/// Used by the remote-host poller after bulk-reading hooks dir
/// contents over SSH; `parse_marker` is the local sibling that does
/// its own read first.
///
/// `path` is used only to derive `received_at` from the filename's
/// millisecond prefix; the file at that path doesn't have to exist on
/// the local filesystem (it lives on the remote in the remote-poller
/// case). When the filename doesn't carry a millisecond prefix and a
/// local stat is also unavailable, `received_at` defaults to the Unix
/// epoch — caller can treat that as a degenerate-but-still-usable
/// signal.
///
/// # Errors
///
/// Returns `io::Error` if the payload lacks a `session_id` field.
pub fn parse_marker_content(path: &Path, raw: &str) -> io::Result<HookEvent> {
    let session_id = parse_session_id(raw)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing session_id field"))?;
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
        blocking_prompt: is_blocking_prompt(parse_notification_type(raw).as_deref()),
        raw_json: raw.to_string(),
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
        blocking_prompt: event.blocking_prompt,
    });
    let _ = fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::mpsc;
    use tempfile::TempDir;

    /// Test helper: fire `receive_hook_from_stdin` against an
    /// in-memory payload + stderr buffer. Returns the marker path (if
    /// any) and the captured stderr text so each test can assert on
    /// both the filesystem effect and the observability log.
    fn dispatch(payload: &str, hook_dir: &Path) -> (io::Result<Option<PathBuf>>, String) {
        let mut input = Cursor::new(payload.as_bytes().to_vec());
        let mut log = Vec::new();
        let result = receive_hook_from_stdin(
            &mut input,
            hook_dir,
            SystemTime::UNIX_EPOCH + Duration::from_millis(1_234_567),
            &mut log,
        );
        (result, String::from_utf8(log).unwrap())
    }

    #[test]
    fn receive_writes_marker_for_permission_prompt() {
        let tmp = TempDir::new().unwrap();
        let payload = r#"{"session_id":"abc-123","hook_event_name":"Notification","notification_type":"permission_prompt"}"#;
        let (result, log) = dispatch(payload, tmp.path());
        let path = result.expect("write marker").expect("marker written");
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(name.ends_with("-abc-123.json"), "unexpected name {name:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), payload);
        assert!(
            log.contains("notification_type=permission_prompt") && log.contains("writing marker"),
            "stderr log should include type + action: {log}"
        );
    }

    #[test]
    fn receive_writes_marker_for_idle_prompt() {
        let tmp = TempDir::new().unwrap();
        let payload = r#"{"session_id":"a","notification_type":"idle_prompt"}"#;
        let (result, _) = dispatch(payload, tmp.path());
        assert!(result.unwrap().is_some());
    }

    #[test]
    fn receive_writes_marker_for_elicitation_dialog() {
        let tmp = TempDir::new().unwrap();
        let payload = r#"{"session_id":"a","notification_type":"elicitation_dialog"}"#;
        let (result, _) = dispatch(payload, tmp.path());
        assert!(result.unwrap().is_some());
    }

    #[test]
    fn receive_skips_auth_success_notifications() {
        // The dogfooding signal that drove this filter: auth_success
        // fires on every successful authentication, including the
        // moment a long-running tool completes and Claude Code
        // re-authenticates. Was the loudest source of the
        // "notifications fire any time a tool finishes" complaint.
        let tmp = TempDir::new().unwrap();
        let payload = r#"{"session_id":"a","notification_type":"auth_success"}"#;
        let (result, log) = dispatch(payload, tmp.path());
        assert!(
            result.unwrap().is_none(),
            "auth_success should not write a marker"
        );
        assert!(
            log.contains("notification_type=auth_success") && log.contains("skipped"),
            "stderr log should record the skip: {log}"
        );
        assert!(
            fs::read_dir(tmp.path()).unwrap().next().is_none(),
            "no marker file should be created"
        );
    }

    #[test]
    fn receive_skips_elicitation_complete_and_response() {
        // Post-input completion events — the matching
        // elicitation_dialog already fired the user-attention signal;
        // firing again on the response would be redundant noise.
        let tmp = TempDir::new().unwrap();
        for nt in ["elicitation_complete", "elicitation_response"] {
            let payload = format!(r#"{{"session_id":"a","notification_type":"{nt}"}}"#);
            let (result, log) = dispatch(&payload, tmp.path());
            assert!(result.unwrap().is_none(), "{nt} must be filtered");
            assert!(log.contains("skipped"), "{nt}: {log}");
        }
    }

    #[test]
    fn receive_writes_marker_when_notification_type_field_is_missing() {
        // Conservative fallback: an unknown/older payload shape stays
        // loud rather than silently quiet. Matches the
        // missing-stop_reason fallback in derive_attention_from_content.
        let tmp = TempDir::new().unwrap();
        let payload = r#"{"session_id":"a","hook_event_name":"Notification"}"#;
        let (result, log) = dispatch(payload, tmp.path());
        assert!(result.unwrap().is_some());
        assert!(
            log.contains("notification_type=<missing>"),
            "log shape: {log}"
        );
    }

    #[test]
    fn receive_skips_unrecognised_notification_type_values() {
        // Defensive: a future schema bump could add a new non-input
        // event type we don't know about. Filter it out (with a log
        // line so dogfooding surfaces it for allowlist refinement).
        let tmp = TempDir::new().unwrap();
        let payload = r#"{"session_id":"a","notification_type":"some_future_event"}"#;
        let (result, log) = dispatch(payload, tmp.path());
        assert!(result.unwrap().is_none());
        assert!(
            log.contains("notification_type=some_future_event") && log.contains("skipped"),
            "log shape: {log}"
        );
    }

    #[test]
    fn is_blocking_prompt_only_true_for_permission_and_elicitation() {
        // The "answer me" glyph fires only for prompts that block on a
        // specific user answer. An idle nudge, an unknown type, or a
        // missing field is "done/waiting", not blocked.
        assert!(is_blocking_prompt(Some("permission_prompt")));
        assert!(is_blocking_prompt(Some("elicitation_dialog")));
        assert!(!is_blocking_prompt(Some("idle_prompt")));
        assert!(!is_blocking_prompt(Some("auth_success")));
        assert!(!is_blocking_prompt(None));
    }

    #[test]
    fn parse_marker_content_sets_blocking_prompt_from_notification_type() {
        let path = Path::new("/x/1700000000000-a.json");
        let permission = parse_marker_content(
            path,
            r#"{"session_id":"a","notification_type":"permission_prompt"}"#,
        )
        .unwrap();
        assert!(permission.blocking_prompt, "permission_prompt is blocking");
        let idle = parse_marker_content(
            path,
            r#"{"session_id":"a","notification_type":"idle_prompt"}"#,
        )
        .unwrap();
        assert!(!idle.blocking_prompt, "idle_prompt is not blocking");
    }

    #[test]
    fn receive_accepts_camel_case_session_id() {
        let tmp = TempDir::new().unwrap();
        let payload = r#"{"sessionId":"abc","notification_type":"permission_prompt"}"#;
        let (result, _) = dispatch(payload, tmp.path());
        assert!(result.unwrap().is_some());
    }

    #[test]
    fn receive_rejects_payload_without_session_id() {
        let tmp = TempDir::new().unwrap();
        let payload =
            r#"{"hook_event_name":"Notification","notification_type":"permission_prompt"}"#;
        let (result, _) = dispatch(payload, tmp.path());
        let err = result.expect_err("should reject");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn receive_rejects_malformed_json() {
        let tmp = TempDir::new().unwrap();
        let (result, _) = dispatch("not valid json", tmp.path());
        assert!(result.is_err());
    }

    #[test]
    fn receive_sanitises_session_id_against_filesystem_hostile_characters() {
        // Defensive: today's session ids are UUIDs, but a future
        // schema with slashes would otherwise create subdirectories.
        let tmp = TempDir::new().unwrap();
        let payload =
            r#"{"session_id":"weird/id with spaces","notification_type":"permission_prompt"}"#;
        let (result, _) = dispatch(payload, tmp.path());
        let path = result.unwrap().expect("marker written");
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
