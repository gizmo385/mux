use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::host::Host;
use crate::session::{Attention, Session, SessionId};
use crate::worktree;

#[must_use]
pub fn claude_projects_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("projects"))
}

/// Discover sessions by listing `root` (typically `~/.claude/projects`)
/// through the given `Host`. The same code path serves the local case and
/// the future SSH case — the only thing that varies is the `Host` impl.
///
/// # Errors
/// Returns `io::Error` if `host.list_transcripts` or the per-transcript
/// reads fail. A missing `root` directory is treated as "no sessions"
/// (see [`crate::host::Host::list_transcripts`]) and yields an empty `Vec`.
pub fn discover(host: &dyn Host, root: &Path) -> io::Result<Vec<Session>> {
    let mut sessions = Vec::new();
    for stat in host.list_transcripts(root)? {
        if let Some(s) = build_session(host, &stat.path, stat.mtime)? {
            sessions.push(s);
        }
    }
    Ok(sessions)
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
    let id = match transcript_path.file_stem().and_then(|s| s.to_str()) {
        Some(s) => SessionId(s.to_string()),
        None => return Ok(None),
    };
    let transcript_meta = read_transcript_meta(host, transcript_path)?;
    let project_dir = transcript_meta
        .cwd
        .unwrap_or_else(|| fallback_dir(transcript_path));
    if !host.is_dir(&project_dir) {
        return Ok(None);
    }
    let title = task_toml_title(host, &project_dir)
        .or(transcript_meta.ai_title)
        .or(transcript_meta.first_user_message);
    Ok(Some(Session {
        id,
        host: host.id().clone(),
        project_dir,
        transcript_path: transcript_path.to_path_buf(),
        last_activity: mtime,
        attention: Attention::Unknown,
        title,
    }))
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

/// Single-pass scan: take cwd from the first line that has one,
/// ai-title from the *last* `{"type":"ai-title",...}` entry (titles
/// refine as the session grows), and the first non-empty user message
/// for the title-fallback path. Malformed JSON lines are skipped.
fn read_transcript_meta(host: &dyn Host, transcript_path: &Path) -> io::Result<TranscriptMeta> {
    let raw = host.read_to_string(transcript_path)?;
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
    Ok(meta)
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

fn task_toml_title(host: &dyn Host, project_dir: &Path) -> Option<String> {
    let raw = host
        .read_to_string(&worktree::task_metadata_path(project_dir))
        .ok()?;
    worktree::parse_task_metadata(&raw).ok().map(|m| m.task)
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
}
