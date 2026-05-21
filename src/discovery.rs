use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::host::Host;
use crate::session::{Attention, HostId, Session, SessionId};
use crate::watcher::derive_attention_from_content;
use crate::worktree;

#[must_use]
pub fn claude_projects_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("projects"))
}

/// Transcripts older than this are dropped at the `list_transcripts`
/// boundary, before any per-transcript SSH `cat` / `test -d` round-trip.
/// Measured 2026-05-18 on a long-lived remote host: 233 transcripts × 4
/// sequential ssh round-trips per session ≈ 7+ minutes wall-clock through
/// a Coder proxy, almost all of it spent on weeks-cold conversations the
/// user is never going to attach to. 30 days is the initial cut; the
/// exact value will become a config knob in M5 (see TODO under
/// `#m5 #config`). Local discovery applies the same filter for parity —
/// the user's mental model of "old session" shouldn't shift based on
/// whether the box happens to be local or remote.
pub const DISCOVERY_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Discover sessions by listing `root` (typically `~/.claude/projects`)
/// through the given `Host`. The same code path serves the local case and
/// the future SSH case — the only thing that varies is the `Host` impl.
///
/// Transcripts whose mtime is older than [`DISCOVERY_MAX_AGE`] are
/// dropped before any per-session work. See [`discover_with_cutoff`] for
/// the test-friendly variant that takes an explicit cutoff.
///
/// # Errors
/// Returns `io::Error` if `host.list_transcripts` or the per-transcript
/// reads fail. A missing `root` directory is treated as "no sessions"
/// (see [`crate::host::Host::list_transcripts`]) and yields an empty `Vec`.
pub fn discover(host: &dyn Host, root: &Path) -> io::Result<Vec<Session>> {
    let cutoff = SystemTime::now()
        .checked_sub(DISCOVERY_MAX_AGE)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    discover_with_cutoff(host, root, cutoff)
}

/// Per-transcript intermediate built during phase 1 of bulk discovery.
/// Stays local to this module — the public surface still trades only in
/// fully-formed [`Session`]s.
struct Partial<'a> {
    stat: &'a crate::host::TranscriptStat,
    content: String,
    project_dir: PathBuf,
    attention: Attention,
}

/// Variant of [`discover`] that takes an explicit `cutoff`: transcripts
/// with mtime strictly older than `cutoff` are filtered. Lets tests pin
/// the boundary without mocking the clock.
///
/// # Errors
/// See [`discover`].
pub fn discover_with_cutoff(
    host: &dyn Host,
    root: &Path,
    cutoff: SystemTime,
) -> io::Result<Vec<Session>> {
    let stats: Vec<_> = host
        .list_transcripts(root)?
        .into_iter()
        .filter(|s| s.mtime >= cutoff)
        .collect();
    if stats.is_empty() {
        return Ok(Vec::new());
    }

    // Phase 1 — bulk-fetch every transcript in one round-trip. The
    // content is reused three times: to extract cwd/title metadata,
    // to derive initial attention, and (indirectly via the parsed
    // cwd) to drive the bulk is_dir / task.toml batches below. Over
    // a high-latency proxy this is the difference between O(N) ssh
    // round-trips and O(1).
    let transcript_paths: Vec<&Path> = stats.iter().map(|s| s.path.as_path()).collect();
    let transcript_contents = host.read_many(&transcript_paths)?;

    // Parse meta + derive attention for each transcript we could read.
    // Transcripts that failed the per-path read drop here — they won't
    // appear in the dashboard this tick, and the next poll will retry.
    let mut partials: Vec<Partial<'_>> = Vec::with_capacity(stats.len());
    for (stat, content_result) in stats.iter().zip(transcript_contents) {
        let Ok(content) = content_result else {
            continue;
        };
        let meta = parse_transcript_meta(&content);
        let project_dir = meta.cwd.clone().unwrap_or_else(|| fallback_dir(&stat.path));
        let attention = derive_attention_from_content(&content);
        partials.push(Partial {
            stat,
            content,
            project_dir,
            attention,
        });
    }

    // Phase 2 — bulk is_dir on the unique project_dirs. Multiple
    // sessions sharing a worktree (common — same repo, several
    // conversations) collapse to one stat call. Dedup is cheap
    // because partials.len() is small.
    let mut unique_dirs: Vec<PathBuf> = partials.iter().map(|p| p.project_dir.clone()).collect();
    unique_dirs.sort();
    unique_dirs.dedup();
    let unique_dir_refs: Vec<&Path> = unique_dirs.iter().map(PathBuf::as_path).collect();
    let exists = host.is_dir_many(&unique_dir_refs)?;
    let exists_by_dir: HashMap<&Path, bool> = unique_dir_refs
        .iter()
        .copied()
        .zip(exists.iter().copied())
        .collect();

    // Phase 3 — bulk-read task.toml and the worktree `.git` pointer
    // file in one round-trip each, only for project_dirs that actually
    // exist. NotFound results land per-path and turn into "no override
    // title" / "no parent repo" rather than failing the batch. Reading
    // `.git` lets the dashboard group worktree-backed sessions under
    // their parent repo instead of treating each worktree as its own
    // project (see `worktree::parse_parent_repo`); for a regular
    // checkout `.git` is a directory and `read_many` returns
    // `NotFound` (its remote `[ -f $p ]` test rejects directories),
    // which surfaces as `parent_repo = None` — exactly what we want.
    let task_dirs: Vec<&PathBuf> = unique_dirs
        .iter()
        .filter(|d| *exists_by_dir.get(d.as_path()).unwrap_or(&false))
        .collect();
    let task_toml_paths: Vec<PathBuf> = task_dirs
        .iter()
        .map(|d| worktree::task_metadata_path(d))
        .collect();
    let task_toml_path_refs: Vec<&Path> = task_toml_paths.iter().map(PathBuf::as_path).collect();
    let task_toml_results = host.read_many(&task_toml_path_refs)?;
    let task_toml_by_dir: HashMap<&Path, String> = task_dirs
        .iter()
        .map(|d| d.as_path())
        .zip(task_toml_results)
        .filter_map(|(d, r)| r.ok().map(|content| (d, content)))
        .collect();

    let git_pointer_paths: Vec<PathBuf> = task_dirs
        .iter()
        .map(|d| worktree::git_pointer_path(d))
        .collect();
    let git_pointer_path_refs: Vec<&Path> =
        git_pointer_paths.iter().map(PathBuf::as_path).collect();
    let git_pointer_results = host.read_many(&git_pointer_path_refs)?;
    let git_pointer_by_dir: HashMap<&Path, String> = task_dirs
        .iter()
        .map(|d| d.as_path())
        .zip(git_pointer_results)
        .filter_map(|(d, r)| r.ok().map(|content| (d, content)))
        .collect();

    // Phase 4 — assemble sessions from the now-resolved data. No I/O
    // beyond this point; this loop is pure CPU over in-memory inputs.
    let mut sessions = Vec::with_capacity(partials.len());
    for partial in &partials {
        let project_dir_exists = *exists_by_dir
            .get(partial.project_dir.as_path())
            .unwrap_or(&false);
        let task_toml = task_toml_by_dir
            .get(partial.project_dir.as_path())
            .map(String::as_str);
        let git_pointer = git_pointer_by_dir
            .get(partial.project_dir.as_path())
            .map(String::as_str);
        if let Some(s) = assemble_session(
            host.id(),
            &partial.stat.path,
            partial.stat.mtime,
            &partial.content,
            project_dir_exists,
            task_toml,
            git_pointer,
            partial.attention,
        ) {
            sessions.push(s);
        }
    }
    Ok(sessions)
}

/// Pure session-assembly: given a fully-fetched payload for one
/// transcript, produce a `Session` or `None` if it's not usable
/// (missing file stem, no surviving `project_dir`). No I/O — both
/// [`build_session`] (single-shot reads for the live-discovery path)
/// and [`discover_with_cutoff`] (bulk reads at startup) compose
/// around this function.
#[allow(clippy::too_many_arguments)] // pure-data assembly; bundling into a
// struct would just push the field-by-field threading one level up to the
// two call sites (bulk discovery + single-shot build_session) without
// reducing the actual coupling.
fn assemble_session(
    host_id: &HostId,
    transcript_path: &Path,
    mtime: SystemTime,
    transcript_content: &str,
    project_dir_exists: bool,
    task_toml_content: Option<&str>,
    git_pointer_content: Option<&str>,
    attention: Attention,
) -> Option<Session> {
    let id = SessionId(transcript_path.file_stem()?.to_str()?.to_string());
    let meta = parse_transcript_meta(transcript_content);
    let project_dir = meta
        .cwd
        .clone()
        .unwrap_or_else(|| fallback_dir(transcript_path));
    if !project_dir_exists {
        return None;
    }
    let task_title = task_toml_content
        .and_then(|raw| worktree::parse_task_metadata(raw).ok())
        .map(|m| m.task);
    let title = task_title.or(meta.ai_title).or(meta.first_user_message);
    let parent_repo = git_pointer_content.and_then(worktree::parse_parent_repo);
    Some(Session {
        id,
        host: host_id.clone(),
        project_dir,
        transcript_path: transcript_path.to_path_buf(),
        last_activity: mtime,
        attention,
        title,
        parent_repo,
        has_live_pane: None,
        hook_pinned: None,
    })
}

/// Build a `Session` from a single transcript path and its mtime. Reused
/// by the transcript watcher's discovery flow when a new `.jsonl` appears
/// mid-run, so both startup discovery and live discovery produce
/// identically-shaped sessions.
///
/// Returns `Ok(None)` for transcripts that aren't usable as live
/// sessions: missing file stem (no derivable id), or a `project_dir`
/// that isn't an existing directory on disk (the worktree was deleted,
/// or the transcript predates having `cwd` metadata and we fell back to
/// the `<unknown>` literal). Either way, the user can't attach to or
/// resume such a session, so showing it in the dashboard would only
/// generate failed-attach noise.
///
/// # Errors
/// Returns `io::Error` if the transcript cannot be read through the host.
pub fn build_session(
    host: &dyn Host,
    transcript_path: &Path,
    mtime: SystemTime,
) -> io::Result<Option<Session>> {
    if transcript_path
        .file_stem()
        .and_then(|s| s.to_str())
        .is_none()
    {
        return Ok(None);
    }
    let content = host.read_to_string(transcript_path)?;
    let meta = parse_transcript_meta(&content);
    let project_dir = meta
        .cwd
        .clone()
        .unwrap_or_else(|| fallback_dir(transcript_path));
    let exists = host.is_dir(&project_dir);
    let (task_toml, git_pointer) = if exists {
        (
            host.read_to_string(&worktree::task_metadata_path(&project_dir))
                .ok(),
            host.read_to_string(&worktree::git_pointer_path(&project_dir))
                .ok(),
        )
    } else {
        (None, None)
    };
    Ok(assemble_session(
        host.id(),
        transcript_path,
        mtime,
        &content,
        exists,
        task_toml.as_deref(),
        git_pointer.as_deref(),
        Attention::Unknown,
    ))
}

#[derive(Debug, Default)]
struct TranscriptMeta {
    cwd: Option<PathBuf>,
    ai_title: Option<String>,
    /// Normalized + truncated text of the first non-empty user-authored
    /// message in the transcript. Used as a title fallback for sessions
    /// where `ai-title` hasn't surfaced yet and no `task.toml` exists —
    /// better than just the directory name when several sessions share
    /// a cwd.
    first_user_message: Option<String>,
}

/// Max display length (in chars, not bytes) for the first-user-message
/// title fallback. Long enough to be useful on a 100-col terminal next
/// to the dimmed cwd/host/age trailing spans; short enough that a
/// rambling first message doesn't dominate the row.
const FIRST_USER_MSG_MAX_CHARS: usize = 60;

/// Single-pass scan over an already-fetched transcript: take cwd from
/// the first line that has one, ai-title from the *last* `ai-title`
/// entry (titles refine as the session grows), and the first non-empty
/// user message for the title-fallback path. Malformed JSON lines are
/// skipped. Pure — no I/O — so both the single-shot `build_session`
/// path and the batched `discover_with_cutoff` path can share it.
fn parse_transcript_meta(raw: &str) -> TranscriptMeta {
    let mut meta = TranscriptMeta::default();
    for line in raw.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if meta.cwd.is_none()
            && let Some(cwd) = value.get("cwd").and_then(serde_json::Value::as_str)
        {
            meta.cwd = Some(PathBuf::from(cwd));
        }
        if value.get("type").and_then(serde_json::Value::as_str) == Some("ai-title")
            && let Some(title) = value.get("aiTitle").and_then(serde_json::Value::as_str)
        {
            meta.ai_title = Some(title.to_string());
        }
        if meta.first_user_message.is_none()
            && value.get("type").and_then(serde_json::Value::as_str) == Some("user")
            && value.get("toolUseResult").is_none()
            && let Some(text) = extract_user_text(&value)
            && !text.trim().is_empty()
            && !is_slash_command_envelope(&text)
        {
            meta.first_user_message = Some(normalize_for_title(&text));
        }
    }
    meta
}

/// Pull the human-authored text out of a `{"type":"user", ...}` entry.
/// Accepts the three shapes seen in practice: `message` as a plain
/// string, `message.content` as a string, or `message.content` as an
/// array of `{"type":"text", "text":"..."}` blocks (with non-text
/// blocks silently skipped). Returns `None` for shapes we don't
/// recognise rather than guessing.
fn extract_user_text(entry: &serde_json::Value) -> Option<String> {
    let message = entry.get("message")?;
    if let Some(s) = message.as_str() {
        return Some(s.to_string());
    }
    let content = message.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        let mut buf = String::new();
        for block in arr {
            if block.get("type").and_then(serde_json::Value::as_str) == Some("text")
                && let Some(text) = block.get("text").and_then(serde_json::Value::as_str)
            {
                if !buf.is_empty() {
                    buf.push(' ');
                }
                buf.push_str(text);
            }
        }
        if !buf.is_empty() {
            return Some(buf);
        }
    }
    None
}

/// True if `text` is Claude Code's slash-command wrapper (e.g.
/// `<local-command-caveat>…</local-command-caveat>` or
/// `<command-name>/clear</command-name>`) rather than human-typed
/// prose. Same family of "user entry but not human content" as
/// `toolUseResult`: surfacing it as a session title produces noise
/// like `<local-command-caveat>The messages below were genera…` for
/// any session whose first input was a slash command and which
/// hasn't had `aiTitle` generated yet.
fn is_slash_command_envelope(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("<local-command-caveat>") || trimmed.starts_with("<command-name>")
}

/// Collapse all-whitespace runs to a single space, trim, and truncate
/// to `FIRST_USER_MSG_MAX_CHARS` chars (not bytes) with an ellipsis
/// suffix when shortened. The list row renders on a single line, so a
/// multi-line first message has to be flattened before it lands there.
fn normalize_for_title(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut iter = collapsed.chars();
    let mut taken: String = iter.by_ref().take(FIRST_USER_MSG_MAX_CHARS).collect();
    if iter.next().is_some() {
        taken.push('…');
    }
    taken
}

fn fallback_dir(transcript_path: &Path) -> PathBuf {
    transcript_path
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        .map_or_else(
            || PathBuf::from("<unknown>"),
            |n| PathBuf::from(n.replace('-', "/")),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::LocalHost;
    use std::fs::{self, create_dir_all};

    /// Build the standard "real cwd + project entry" scaffolding under a
    /// fresh tempdir. Returns `(tempdir, projects_root, real_cwd)` so the
    /// tempdir's lifetime extends to the end of the test.
    fn setup_with_real_cwd() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let cwd = tmp.path().join("real-cwd");
        create_dir_all(&projects).unwrap();
        create_dir_all(&cwd).unwrap();
        (tmp, projects, cwd)
    }

    /// Test-local shorthand: every test in this module discovers against
    /// the local filesystem, so wrap the explicit-host call.
    fn discover_local(root: &Path) -> io::Result<Vec<Session>> {
        discover(&LocalHost::new(), root)
    }

    /// Force a file's mtime to a specific instant. Used by the recency-
    /// filter tests to age a transcript past the cutoff without waiting
    /// 30 days. Wraps `File::set_times` so the call site stays one-line.
    fn set_mtime(path: &Path, mtime: SystemTime) {
        let times = std::fs::FileTimes::new().set_modified(mtime);
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for set_times");
        f.set_times(times).expect("set_times");
    }

    #[test]
    fn discovers_session_with_cwd_from_jsonl() {
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc-123.jsonl"),
            format!("{{\"type\":\"user\",\"cwd\":\"{}\"}}\n", cwd.display()),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id.0, "abc-123");
        assert_eq!(sessions[0].project_dir, cwd);
    }

    #[test]
    fn discovers_session_with_parent_repo_when_cwd_is_a_worktree() {
        // End-to-end: a transcript whose `cwd` is a git worktree must
        // surface `Session.parent_repo = Some(<parent>)`, derived from
        // the worktree's `.git` pointer file. This is what lets the
        // dashboard group worktree-backed sessions under the parent
        // repo header instead of fragmenting per-worktree.
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        // Lay down a `.git` pointer file in the cwd so the discovery
        // bulk-read picks it up. The exact `worktrees/<id>` suffix
        // doesn't matter for parsing — only the segment up to
        // `/.git/worktrees/` matters.
        let parent_repo = cwd.parent().unwrap().join("the-parent");
        create_dir_all(&parent_repo).unwrap();
        fs::write(
            cwd.join(".git"),
            format!(
                "gitdir: {}/.git/worktrees/cwd-task\n",
                parent_repo.display()
            ),
        )
        .unwrap();

        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("wt.jsonl"),
            format!("{{\"type\":\"user\",\"cwd\":\"{}\"}}\n", cwd.display()),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].project_dir, cwd);
        assert_eq!(
            sessions[0].parent_repo.as_deref(),
            Some(parent_repo.as_path())
        );
    }

    #[test]
    fn discovers_session_with_no_parent_repo_for_plain_checkout() {
        // Regular-checkout session: `.git` is a directory, not a
        // pointer file. The bulk-read should return `NotFound`
        // (LocalHost::read_many → fs::read_to_string of a directory
        // errors with IsADirectory on Unix; either way it's an Err)
        // and parent_repo should land as `None` — grouping falls
        // through to project_dir.
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        // Plain .git directory, no pointer file.
        create_dir_all(cwd.join(".git")).unwrap();

        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("plain.jsonl"),
            format!("{{\"type\":\"user\",\"cwd\":\"{}\"}}\n", cwd.display()),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].parent_repo, None);
    }

    #[test]
    fn stale_cwd_session_is_filtered_out() {
        // The user's scenario: a transcript whose recorded cwd points at a
        // worktree that has since been deleted. The session is no longer
        // resumable, so it should not appear in the dashboard.
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let entry = projects.join("-deleted");
        create_dir_all(&entry).unwrap();
        let gone = tmp.path().join("deleted-worktree");
        // Note: we never `create_dir_all(&gone)`.
        fs::write(
            entry.join("abc.jsonl"),
            format!("{{\"type\":\"user\",\"cwd\":\"{}\"}}\n", gone.display()),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert!(sessions.is_empty(), "got: {sessions:?}");
    }

    #[test]
    fn session_with_no_cwd_metadata_and_no_real_fallback_is_filtered() {
        // When the transcript has no cwd, build_session falls back to the
        // decoded project-dir-name (`-home-test-proj` → `/home/test/proj`).
        // The fallback path is unlikely to exist on a CI worker, so the
        // session is filtered. This is by design — such transcripts are
        // pre-cwd-metadata legacy entries that we can't attach to anyway.
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("-this-path-does-not-exist-anywhere-xyz");
        create_dir_all(&proj).unwrap();
        fs::write(proj.join("xyz.jsonl"), "{\"type\":\"system\"}\n").unwrap();

        let sessions = discover_local(tmp.path()).unwrap();
        assert!(sessions.is_empty(), "got: {sessions:?}");
    }

    #[test]
    fn returns_empty_when_root_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let sessions = discover_local(&missing).unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn extracts_ai_title_from_transcript() {
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\"}}\n\
                 {{\"type\":\"ai-title\",\"aiTitle\":\"Wire up the parser\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("Wire up the parser"));
    }

    #[test]
    fn ai_title_uses_latest_entry_when_multiple() {
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\"}}\n\
                 {{\"type\":\"ai-title\",\"aiTitle\":\"early guess\"}}\n\
                 {{\"type\":\"ai-title\",\"aiTitle\":\"refined title\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("refined title"));
    }

    #[test]
    fn task_toml_title_overrides_ai_title() {
        let tmp = tempfile::tempdir().unwrap();
        // Transcript lives in projects/<encoded>; project_dir points at a
        // separate directory containing .agent-mux/task.toml.
        let proj_dir = tmp.path().join("worktree");
        let agent_mux_dir = proj_dir.join(".agent-mux");
        create_dir_all(&agent_mux_dir).unwrap();
        fs::write(
            agent_mux_dir.join("task.toml"),
            "task = \"explicit task name\"\n\
             base_branch = \"main\"\n\
             created_at = 0\n",
        )
        .unwrap();

        let projects = tmp.path().join("projects");
        let entry = projects.join("-worktree");
        create_dir_all(&entry).unwrap();
        let cwd_line = format!("{{\"type\":\"user\",\"cwd\":\"{}\"}}\n", proj_dir.display());
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{cwd_line}\
                 {{\"type\":\"ai-title\",\"aiTitle\":\"auto title\"}}\n"
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("explicit task name"));
    }

    #[test]
    fn title_is_none_when_no_signal() {
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!("{{\"type\":\"user\",\"cwd\":\"{}\"}}\n", cwd.display()),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert!(sessions[0].title.is_none());
    }

    #[test]
    fn ignores_non_jsonl_files() {
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(entry.join("memory"), "not a session").unwrap();
        fs::write(
            entry.join("real.jsonl"),
            format!("{{\"cwd\":\"{}\"}}\n", cwd.display()),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id.0, "real");
    }

    #[test]
    fn first_user_message_is_used_when_no_ai_title() {
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"refactor the parser\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("refactor the parser"));
    }

    #[test]
    fn ai_title_takes_precedence_over_first_user_message() {
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"hi\"}}\n\
                 {{\"type\":\"ai-title\",\"aiTitle\":\"Wire the parser\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("Wire the parser"));
    }

    #[test]
    fn first_user_message_extracts_from_content_string_shape() {
        // The schema Claude Code writes in practice: message is an object
        // with role + content, content is a plain string.
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":{{\"role\":\"user\",\"content\":\"do the thing\"}}}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("do the thing"));
    }

    #[test]
    fn first_user_message_extracts_from_content_block_array() {
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"hello\"}},{{\"type\":\"text\",\"text\":\"world\"}}]}}}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("hello world"));
    }

    #[test]
    fn first_user_message_is_truncated_with_ellipsis() {
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        let long: String = "a".repeat(200);
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"{long}\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        let title = sessions[0].title.as_deref().unwrap();
        assert!(title.ends_with('…'), "got: {title}");
        assert_eq!(title.chars().count(), FIRST_USER_MSG_MAX_CHARS + 1);
    }

    #[test]
    fn first_user_message_collapses_whitespace() {
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"line one\\n\\nline two\\t\\ttabbed\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(
            sessions[0].title.as_deref(),
            Some("line one line two tabbed")
        );
    }

    #[test]
    fn tool_result_user_entries_do_not_count_as_first_user_message() {
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"toolUseResult\":{{\"stdout\":\"ok\"}}}}\n\
                 {{\"type\":\"user\",\"message\":\"the real first prompt\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("the real first prompt"));
    }

    #[test]
    fn local_command_caveat_envelope_is_skipped() {
        // The user's scenario: open Claude Code, type `/clear`, and the
        // first JSONL user entry has content like
        // "<local-command-caveat>The messages below were generated...
        // </local-command-caveat>" — Claude Code's CLI wrapper text,
        // not human prose. We should fall through to the next real
        // message (or to cwd) rather than display the envelope.
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"<local-command-caveat>The messages below were generated by the user while running local commands. DO NOT respond.</local-command-caveat>\"}}\n\
                 {{\"type\":\"user\",\"message\":\"the real first prompt\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("the real first prompt"));
    }

    #[test]
    fn command_name_envelope_is_skipped() {
        // The other slash-command shape: a user message whose content
        // starts with `<command-name>/foo</command-name>` (often
        // followed by `<command-message>` / `<command-args>` tags).
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"<command-name>/clear</command-name>\"}}\n\
                 {{\"type\":\"user\",\"message\":\"real prompt\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("real prompt"));
    }

    #[test]
    fn slash_command_envelope_with_leading_whitespace_is_still_skipped() {
        // Defensive: leading whitespace (newlines, indentation) should
        // not cause the envelope predicate to miss.
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"  \\n<local-command-caveat>x</local-command-caveat>\"}}\n\
                 {{\"type\":\"user\",\"message\":\"real prompt\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("real prompt"));
    }

    #[test]
    fn message_that_merely_mentions_envelope_tag_is_not_skipped() {
        // The predicate is anchored to the start of the trimmed text,
        // so a human message that quotes the tag (e.g. asking about
        // it) is not mistaken for an envelope.
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"what does <local-command-caveat> mean?\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(
            sessions[0].title.as_deref(),
            Some("what does <local-command-caveat> mean?")
        );
    }

    #[test]
    fn empty_user_message_is_skipped() {
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("abc.jsonl"),
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"   \"}}\n\
                 {{\"type\":\"user\",\"message\":\"second message\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let sessions = discover_local(&projects).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("second message"));
    }

    #[test]
    fn cold_transcript_is_filtered_before_per_session_work() {
        // The performance win this filter exists for: a transcript older
        // than the cutoff is dropped at the `list_transcripts` boundary,
        // so the expensive `cat`/`is_dir`/title-read round-trips never
        // happen. We can't directly observe "no SSH calls were made"
        // through the LocalHost path, but we can prove the filtered
        // transcript doesn't appear in the result even though it would
        // have been a valid session by content.
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        let path = entry.join("cold.jsonl");
        fs::write(
            &path,
            format!("{{\"type\":\"user\",\"cwd\":\"{}\"}}\n", cwd.display()),
        )
        .unwrap();
        let now = SystemTime::now();
        // Age the transcript 60 days; cutoff at 30 days means it's
        // out — strictly older than the cutoff.
        set_mtime(&path, now - Duration::from_secs(60 * 24 * 60 * 60));

        let cutoff = now - Duration::from_secs(30 * 24 * 60 * 60);
        let sessions =
            discover_with_cutoff(&LocalHost::new(), &projects, cutoff).expect("discover");
        assert!(
            sessions.is_empty(),
            "cold transcript should be filtered: {sessions:?}"
        );
    }

    #[test]
    fn warm_transcript_survives_the_cutoff() {
        // Sibling test to the cold-filter one: a transcript still inside
        // the window must reach `build_session` and appear in the result.
        // Pins both halves of the boundary so a future change that flips
        // the comparison direction blows up here.
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        let path = entry.join("warm.jsonl");
        fs::write(
            &path,
            format!("{{\"type\":\"user\",\"cwd\":\"{}\"}}\n", cwd.display()),
        )
        .unwrap();
        let now = SystemTime::now();
        // Age the transcript 5 days; cutoff at 30 days lets it through.
        set_mtime(&path, now - Duration::from_secs(5 * 24 * 60 * 60));

        let cutoff = now - Duration::from_secs(30 * 24 * 60 * 60);
        let sessions =
            discover_with_cutoff(&LocalHost::new(), &projects, cutoff).expect("discover");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id.0, "warm");
    }

    #[test]
    fn discover_default_uses_max_age_constant() {
        // Without injecting a cutoff, `discover` should apply the public
        // `DISCOVERY_MAX_AGE` constant. Pin it by constructing a
        // transcript aged just past the constant and confirming the
        // bare `discover` call filters it.
        let (_tmp, projects, cwd) = setup_with_real_cwd();
        let entry = projects.join("-real-cwd");
        create_dir_all(&entry).unwrap();
        let path = entry.join("aged.jsonl");
        fs::write(
            &path,
            format!("{{\"type\":\"user\",\"cwd\":\"{}\"}}\n", cwd.display()),
        )
        .unwrap();
        // 1 hour past the constant — well inside floating-point /
        // scheduler slack on the SystemTime::now() comparison.
        set_mtime(
            &path,
            SystemTime::now() - DISCOVERY_MAX_AGE - Duration::from_secs(60 * 60),
        );

        let sessions = discover_local(&projects).expect("discover");
        assert!(
            sessions.is_empty(),
            "discover() should honour DISCOVERY_MAX_AGE: {sessions:?}"
        );
    }
}
