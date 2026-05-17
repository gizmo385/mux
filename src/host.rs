use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::session::HostId;

/// One entry returned by [`Host::list_transcripts`]: an absolute transcript
/// path and its last-modified time. The mtime lets the polling watcher
/// (M2 next chunk) skip files that haven't changed since the last scan.
#[derive(Debug, Clone)]
pub struct TranscriptStat {
    pub path: PathBuf,
    pub mtime: SystemTime,
}

/// Hides the local-vs-SSH distinction for *read* operations on a host's
/// Claude Code transcripts and worktree metadata. Spawn/attach operations
/// live in the Attachment Driver, not here.
///
/// Trait-object–safe: all methods take `&self` and use only concrete types.
pub trait Host: Send + Sync {
    /// Stable identifier — matches the dashboard label and the
    /// `[hosts.<name>]` config key (or [`HostId::local`] for the local
    /// implicit host).
    fn id(&self) -> &HostId;

    /// Two-level walk under `root` (matches Claude Code's layout:
    /// `root/<project-hash>/<session-id>.jsonl`). Returns absolute paths
    /// and mtimes for every `.jsonl` discovered. A missing `root` is not
    /// an error — returns an empty `Vec` so callers don't have to special-
    /// case first-run.
    ///
    /// # Errors
    /// Propagates any I/O error other than `NotFound` on `root`.
    fn list_transcripts(&self, root: &Path) -> io::Result<Vec<TranscriptStat>>;

    /// Read the whole file as UTF-8. Intended for small files (transcript
    /// metadata extraction, `.agent-mux/task.toml`); use [`Host::read_tail`]
    /// for transcripts where only the tail is needed.
    ///
    /// # Errors
    /// Propagates any I/O or UTF-8 decoding error.
    fn read_to_string(&self, path: &Path) -> io::Result<String>;

    /// Read the last `n_bytes` of a file. Used by attention derivation
    /// against transcripts that can be megabytes long; reading the whole
    /// file every tick would be wasteful, and over SSH would be unusably
    /// slow. Local impl seeks; SSH impl will shell out to `tail -c <n>`.
    ///
    /// Returns a String via [`String::from_utf8_lossy`] so a tail that
    /// happens to start mid-codepoint still produces parseable output for
    /// the line-by-line JSON consumer (the bad line fails to parse and
    /// gets skipped, exactly as it would in the legacy `BufReader::lines`
    /// path).
    ///
    /// # Errors
    /// Propagates any I/O error from opening, stating, or seeking.
    fn read_tail(&self, path: &Path, n_bytes: u64) -> io::Result<String>;

    /// True iff `path` exists and is a directory. Used by discovery's
    /// stale-session filter (skip transcripts whose `cwd` no longer
    /// exists on disk). Errors are folded into `false` because the only
    /// useful question is "can I attach a session rooted here?" and a
    /// failed stat is indistinguishable from "no" for that purpose.
    fn is_dir(&self, path: &Path) -> bool;
}

/// `Host` implementation for the local machine. Pure `std::fs` calls; no
/// owned state beyond its identity.
#[derive(Debug, Clone)]
pub struct LocalHost {
    id: HostId,
}

impl LocalHost {
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: HostId::local(),
        }
    }
}

impl Default for LocalHost {
    fn default() -> Self {
        Self::new()
    }
}

impl Host for LocalHost {
    fn id(&self) -> &HostId {
        &self.id
    }

    fn list_transcripts(&self, root: &Path) -> io::Result<Vec<TranscriptStat>> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(root) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e),
        };
        for project_dir in entries {
            let project_dir = project_dir?;
            if !project_dir.file_type()?.is_dir() {
                continue;
            }
            for jsonl in fs::read_dir(project_dir.path())? {
                let jsonl = jsonl?;
                let path = jsonl.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let meta = fs::metadata(&path)?;
                let mtime = meta.modified()?;
                out.push(TranscriptStat { path, mtime });
            }
        }
        Ok(out)
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        fs::read_to_string(path)
    }

    fn read_tail(&self, path: &Path, n_bytes: u64) -> io::Result<String> {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        let start = len.saturating_sub(n_bytes);
        file.seek(SeekFrom::Start(start))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir parent");
        }
        fs::write(path, content).expect("write file");
    }

    #[test]
    fn id_returns_local() {
        let host = LocalHost::new();
        assert!(host.id().is_local());
    }

    #[test]
    fn list_transcripts_returns_empty_for_missing_root() {
        let tmp = TempDir::new().expect("tempdir");
        let host = LocalHost::new();
        let stats = host
            .list_transcripts(&tmp.path().join("nope"))
            .expect("missing root is ok");
        assert!(stats.is_empty());
    }

    #[test]
    fn list_transcripts_finds_jsonl_two_levels_deep() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path().join("projects");
        write_file(&root.join("proj-a").join("s1.jsonl"), "{}\n");
        write_file(&root.join("proj-a").join("s2.jsonl"), "{}\n");
        write_file(&root.join("proj-b").join("s3.jsonl"), "{}\n");

        let host = LocalHost::new();
        let mut stats = host.list_transcripts(&root).expect("list");
        stats.sort_by(|a, b| a.path.cmp(&b.path));
        let names: Vec<_> = stats
            .iter()
            .map(|s| s.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["s1.jsonl", "s2.jsonl", "s3.jsonl"]);
        assert!(stats.iter().all(|s| s.mtime <= SystemTime::now()));
    }

    #[test]
    fn list_transcripts_skips_non_jsonl_and_top_level_files() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path().join("projects");
        write_file(&root.join("proj-a").join("s1.jsonl"), "{}\n");
        write_file(&root.join("proj-a").join("notes.txt"), "ignore");
        write_file(&root.join("README.md"), "ignore"); // top-level file: not a project dir

        let host = LocalHost::new();
        let stats = host.list_transcripts(&root).expect("list");
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].path.file_name().unwrap(), "s1.jsonl");
    }

    #[test]
    fn read_to_string_returns_file_contents() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("file.txt");
        fs::write(&path, "hello\n").expect("write");
        let host = LocalHost::new();
        assert_eq!(host.read_to_string(&path).expect("read"), "hello\n");
    }

    #[test]
    fn read_to_string_propagates_missing_file_error() {
        let tmp = TempDir::new().expect("tempdir");
        let host = LocalHost::new();
        let err = host
            .read_to_string(&tmp.path().join("nope"))
            .expect_err("should error");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn read_tail_returns_whole_file_when_smaller_than_n_bytes() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("file.txt");
        fs::write(&path, "abc\n").expect("write");
        let host = LocalHost::new();
        assert_eq!(host.read_tail(&path, 1024).expect("read"), "abc\n");
    }

    #[test]
    fn read_tail_returns_last_n_bytes_when_file_is_larger() {
        use std::fmt::Write as _;
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("file.txt");
        // 90 bytes total, ask for last 10.
        let mut content = String::new();
        for i in 0..10 {
            writeln!(content, "line-{i:03}").expect("write to string");
        }
        assert_eq!(content.len(), 90);
        fs::write(&path, &content).expect("write");
        let host = LocalHost::new();
        let tail = host.read_tail(&path, 10).expect("read");
        assert_eq!(tail.len(), 10);
        assert!(tail.ends_with("line-009\n"));
    }

    #[test]
    fn is_dir_recognises_directories_and_rejects_files() {
        let tmp = TempDir::new().expect("tempdir");
        let host = LocalHost::new();
        assert!(host.is_dir(tmp.path()));

        let file = tmp.path().join("file.txt");
        fs::write(&file, "x").expect("write");
        assert!(!host.is_dir(&file));

        assert!(!host.is_dir(&tmp.path().join("nope")));
    }
}
