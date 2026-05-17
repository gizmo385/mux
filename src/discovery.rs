use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::session::{Attention, Host, Session, SessionId};
use crate::worktree;

#[must_use]
pub fn claude_projects_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("projects"))
}

/// Discover sessions by scanning `root` (typically `~/.claude/projects`).
///
/// # Errors
/// Returns `io::Error` if a project subdirectory or transcript file is
/// unreadable. A missing `root` directory is treated as "no sessions"
/// and yields an empty `Vec`.
pub fn discover_local(root: &Path) -> io::Result<Vec<Session>> {
    let mut sessions = Vec::new();
    let entries = match fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(sessions),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        for jsonl in fs::read_dir(entry.path())? {
            let jsonl = jsonl?;
            let path = jsonl.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(s) = build_session(&path)? {
                sessions.push(s);
            }
        }
    }
    Ok(sessions)
}

/// Build a `Session` from a single transcript path. Reused by the
/// transcript watcher's discovery flow when a new `.jsonl` appears
/// mid-run, so both startup discovery and live discovery produce
/// identically-shaped sessions.
///
/// # Errors
/// Returns `io::Error` if metadata or the transcript itself cannot be
/// read. Returns `Ok(None)` when the path has no usable file stem (i.e.
/// no derivable session id).
pub fn build_session(transcript_path: &Path) -> io::Result<Option<Session>> {
    let id = match transcript_path.file_stem().and_then(|s| s.to_str()) {
        Some(s) => SessionId(s.to_string()),
        None => return Ok(None),
    };
    let metadata = fs::metadata(transcript_path)?;
    let last_activity = metadata.modified()?;
    let transcript_meta = read_transcript_meta(transcript_path)?;
    let project_dir = transcript_meta
        .cwd
        .unwrap_or_else(|| fallback_dir(transcript_path));
    let title = task_toml_title(&project_dir).or(transcript_meta.ai_title);
    Ok(Some(Session {
        id,
        host: Host::Local,
        project_dir,
        transcript_path: transcript_path.to_path_buf(),
        last_activity,
        attention: Attention::Unknown,
        title,
    }))
}

#[derive(Debug, Default)]
struct TranscriptMeta {
    cwd: Option<PathBuf>,
    ai_title: Option<String>,
}

/// Single-pass scan: take cwd from the first line that has one, and
/// ai-title from the *last* `{"type":"ai-title",...}` entry (titles get
/// refined as the session grows). Malformed JSON lines are skipped.
fn read_transcript_meta(transcript_path: &Path) -> io::Result<TranscriptMeta> {
    let file = File::open(transcript_path)?;
    let reader = BufReader::new(file);
    let mut meta = TranscriptMeta::default();
    for line in reader.lines() {
        let line = line?;
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
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
    }
    Ok(meta)
}

fn task_toml_title(project_dir: &Path) -> Option<String> {
    worktree::read_task_metadata(project_dir)
        .ok()
        .map(|m| m.task)
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
    use std::fs::create_dir_all;

    #[test]
    fn discovers_session_with_cwd_from_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("-home-test-proj");
        create_dir_all(&proj).unwrap();
        fs::write(
            proj.join("abc-123.jsonl"),
            "{\"type\":\"user\",\"cwd\":\"/home/test/proj\"}\n",
        )
        .unwrap();

        let sessions = discover_local(tmp.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id.0, "abc-123");
        assert_eq!(sessions[0].project_dir, PathBuf::from("/home/test/proj"));
    }

    #[test]
    fn falls_back_to_decoded_dir_name_when_no_cwd_in_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("-home-test-proj");
        create_dir_all(&proj).unwrap();
        fs::write(proj.join("xyz.jsonl"), "{\"type\":\"system\"}\n").unwrap();

        let sessions = discover_local(tmp.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].project_dir, PathBuf::from("/home/test/proj"));
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
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("-x");
        create_dir_all(&proj).unwrap();
        fs::write(
            proj.join("abc.jsonl"),
            "{\"type\":\"user\",\"cwd\":\"/x\"}\n\
             {\"type\":\"ai-title\",\"aiTitle\":\"Wire up the parser\"}\n",
        )
        .unwrap();

        let sessions = discover_local(tmp.path()).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("Wire up the parser"));
    }

    #[test]
    fn ai_title_uses_latest_entry_when_multiple() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("-x");
        create_dir_all(&proj).unwrap();
        fs::write(
            proj.join("abc.jsonl"),
            "{\"type\":\"user\",\"cwd\":\"/x\"}\n\
             {\"type\":\"ai-title\",\"aiTitle\":\"early guess\"}\n\
             {\"type\":\"ai-title\",\"aiTitle\":\"refined title\"}\n",
        )
        .unwrap();

        let sessions = discover_local(tmp.path()).unwrap();
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
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("-x");
        create_dir_all(&proj).unwrap();
        fs::write(
            proj.join("abc.jsonl"),
            "{\"type\":\"user\",\"cwd\":\"/x\"}\n",
        )
        .unwrap();

        let sessions = discover_local(tmp.path()).unwrap();
        assert!(sessions[0].title.is_none());
    }

    #[test]
    fn ignores_non_jsonl_files() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("-x");
        create_dir_all(&proj).unwrap();
        fs::write(proj.join("memory"), "not a session").unwrap();
        fs::write(proj.join("real.jsonl"), "{\"cwd\":\"/x\"}\n").unwrap();

        let sessions = discover_local(tmp.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id.0, "real");
    }
}
