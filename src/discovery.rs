use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::session::{Attention, Host, Session, SessionId};

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

fn build_session(transcript_path: &Path) -> io::Result<Option<Session>> {
    let id = match transcript_path.file_stem().and_then(|s| s.to_str()) {
        Some(s) => SessionId(s.to_string()),
        None => return Ok(None),
    };
    let metadata = fs::metadata(transcript_path)?;
    let last_activity = metadata.modified()?;
    let project_dir = read_cwd(transcript_path)?.unwrap_or_else(|| fallback_dir(transcript_path));
    Ok(Some(Session {
        id,
        host: Host::Local,
        project_dir,
        transcript_path: transcript_path.to_path_buf(),
        last_activity,
        attention: Attention::Unknown,
    }))
}

fn read_cwd(transcript_path: &Path) -> io::Result<Option<PathBuf>> {
    let file = File::open(transcript_path)?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line)
            && let Some(cwd) = value.get("cwd").and_then(serde_json::Value::as_str)
        {
            return Ok(Some(PathBuf::from(cwd)));
        }
    }
    Ok(None)
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
