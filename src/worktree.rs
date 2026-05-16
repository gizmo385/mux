use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const METADATA_PATH: &str = ".agent-mux/task.toml";

#[derive(Debug)]
pub enum WorktreeError {
    NotARepo(PathBuf),
    GitFailed(String),
    InvalidTaskName(String),
    PathExists(PathBuf),
    Io(std::io::Error),
    TomlSerialize(toml::ser::Error),
    TomlParse(toml::de::Error),
}

impl std::fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotARepo(p) => write!(f, "{} is not a git repository", p.display()),
            Self::GitFailed(msg) => write!(f, "git: {msg}"),
            Self::InvalidTaskName(name) => {
                write!(f, "task name {name:?} produces an empty slug")
            }
            Self::PathExists(p) => write!(f, "worktree path {} already exists", p.display()),
            Self::Io(e) => write!(f, "io: {e}"),
            Self::TomlSerialize(e) => write!(f, "toml serialize: {e}"),
            Self::TomlParse(e) => write!(f, "toml parse: {e}"),
        }
    }
}

impl std::error::Error for WorktreeError {}

impl From<std::io::Error> for WorktreeError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskMetadata {
    pub task: String,
    pub base_branch: String,
    /// Seconds since the Unix epoch at worktree creation.
    pub created_at: u64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct WorktreeManager;

impl WorktreeManager {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Create a worktree alongside the parent repo.
    ///
    /// The new worktree lives at `<repo-parent>/<repo-name>-<slug>` on a new
    /// branch named `<slug>` branched off `base_branch`. A `task.toml` file
    /// inside the worktree records the task metadata.
    ///
    /// # Errors
    /// Returns [`WorktreeError`] for missing repo, git failure, empty-slug
    /// task name, path collision, or I/O failure while writing metadata.
    pub fn create(
        &self,
        repo: &Path,
        base_branch: &str,
        task: &str,
    ) -> Result<PathBuf, WorktreeError> {
        let repo_root = repo_toplevel(repo)?;
        let slug = slugify(task).ok_or_else(|| WorktreeError::InvalidTaskName(task.to_string()))?;
        let path = worktree_path(&repo_root, &slug);
        if path.exists() {
            return Err(WorktreeError::PathExists(path));
        }
        run_git(
            &repo_root,
            &[
                "worktree",
                "add",
                "-b",
                &slug,
                &path.to_string_lossy(),
                base_branch,
            ],
        )?;
        write_task_metadata(
            &path,
            &TaskMetadata {
                task: task.to_string(),
                base_branch: base_branch.to_string(),
                created_at: now_unix_seconds(),
            },
        )?;
        Ok(path)
    }
}

/// Resolve the repo's default branch name.
///
/// Tries `git symbolic-ref --short refs/remotes/origin/HEAD` and strips the
/// `origin/` prefix; falls back to `main` then `master` if either exists as
/// a local ref. Returns `None` if none resolve — the caller should prompt.
#[must_use]
pub fn resolve_default_base_branch(repo: &Path) -> Option<String> {
    if let Some(name) = symbolic_ref_origin_head(repo) {
        return Some(name);
    }
    for fallback in ["main", "master"] {
        if branch_exists(repo, fallback) {
            return Some(fallback.to_string());
        }
    }
    None
}

/// Read task metadata from a worktree.
///
/// # Errors
/// Returns [`WorktreeError::Io`] if the file is missing or unreadable, and
/// [`WorktreeError::TomlParse`] if its contents don't deserialize.
pub fn read_task_metadata(worktree: &Path) -> Result<TaskMetadata, WorktreeError> {
    let raw = fs::read_to_string(worktree.join(METADATA_PATH))?;
    toml::from_str(&raw).map_err(WorktreeError::TomlParse)
}

fn write_task_metadata(worktree: &Path, meta: &TaskMetadata) -> Result<(), WorktreeError> {
    let dir = worktree.join(".agent-mux");
    fs::create_dir_all(&dir)?;
    let serialized = toml::to_string(meta).map_err(WorktreeError::TomlSerialize)?;
    fs::write(worktree.join(METADATA_PATH), serialized)?;
    Ok(())
}

fn slugify(task: &str) -> Option<String> {
    let mut out = String::with_capacity(task.len());
    let mut prev_dash = true;
    for c in task.chars() {
        if c.is_ascii_alphanumeric() {
            out.extend(c.to_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_end_matches('-');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn worktree_path(repo_root: &Path, slug: &str) -> PathBuf {
    let parent = repo_root.parent().unwrap_or(repo_root);
    let repo_name = repo_root
        .file_name()
        .map_or_else(|| "repo".to_string(), |n| n.to_string_lossy().into_owned());
    parent.join(format!("{repo_name}-{slug}"))
}

fn repo_toplevel(repo: &Path) -> Result<PathBuf, WorktreeError> {
    let stdout = run_git_stdout(repo, &["rev-parse", "--show-toplevel"])
        .map_err(|_| WorktreeError::NotARepo(repo.to_path_buf()))?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(WorktreeError::NotARepo(repo.to_path_buf()));
    }
    Ok(PathBuf::from(trimmed))
}

fn symbolic_ref_origin_head(repo: &Path) -> Option<String> {
    let stdout = run_git_stdout(
        repo,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .ok()?;
    stdout
        .trim()
        .strip_prefix("origin/")
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn branch_exists(repo: &Path, name: &str) -> bool {
    git_command(repo)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ])
        .status()
        .is_ok_and(|s| s.success())
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<(), WorktreeError> {
    let output = git_command(cwd)
        .args(args)
        .output()
        .map_err(|e| WorktreeError::GitFailed(e.to_string()))?;
    if !output.status.success() {
        return Err(WorktreeError::GitFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(())
}

fn run_git_stdout(cwd: &Path, args: &[&str]) -> Result<String, WorktreeError> {
    let output = git_command(cwd)
        .args(args)
        .output()
        .map_err(|e| WorktreeError::GitFailed(e.to_string()))?;
    if !output.status.success() {
        return Err(WorktreeError::GitFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

// Strip GIT_* env vars that would redirect git's view of the repo. Critical
// when this process (or its tests) is invoked inside a git hook, where
// GIT_DIR/GIT_WORK_TREE/etc. point at the surrounding repo and silently
// override `current_dir`.
fn git_command(cwd: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd);
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_COMMON_DIR",
        "GIT_PREFIX",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn init_repo(dir: &Path) {
        run_git(dir, &["init", "-q", "-b", "main"]).expect("git init");
        run_git(dir, &["config", "user.email", "test@example.com"]).expect("git config email");
        run_git(dir, &["config", "user.name", "test"]).expect("git config name");
        run_git(dir, &["config", "commit.gpgsign", "false"]).expect("git config gpgsign");
        fs::write(dir.join("README.md"), "seed").expect("seed file");
        run_git(dir, &["add", "."]).expect("git add");
        run_git(dir, &["commit", "-q", "-m", "seed"]).expect("git commit");
    }

    #[test]
    fn slugify_basic_cases() {
        assert_eq!(
            slugify("refactor parser").as_deref(),
            Some("refactor-parser")
        );
        assert_eq!(slugify("Fix bug #123!").as_deref(), Some("fix-bug-123"));
        assert_eq!(slugify("  spaced  ").as_deref(), Some("spaced"));
        assert_eq!(slugify("multi---dash").as_deref(), Some("multi-dash"));
    }

    #[test]
    fn slugify_rejects_empty_result() {
        assert!(slugify("").is_none());
        assert!(slugify("!!!").is_none());
        assert!(slugify("   ").is_none());
    }

    #[test]
    fn worktree_path_is_alongside_repo() {
        let path = worktree_path(Path::new("/work/agent-mux"), "fix-bug");
        assert_eq!(path, PathBuf::from("/work/agent-mux-fix-bug"));
    }

    #[test]
    fn create_writes_worktree_and_metadata() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = tmp.path().join("proj");
        fs::create_dir(&repo).expect("mkdir proj");
        init_repo(&repo);

        let path = WorktreeManager::new()
            .create(&repo, "main", "refactor parser")
            .expect("create worktree");

        assert!(path.is_dir(), "worktree dir should exist: {path:?}");
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("proj-refactor-parser")
        );

        let meta = read_task_metadata(&path).expect("read metadata");
        assert_eq!(meta.task, "refactor parser");
        assert_eq!(meta.base_branch, "main");
        assert!(meta.created_at > 0);
    }

    #[test]
    fn create_rejects_collisions() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = tmp.path().join("proj");
        fs::create_dir(&repo).expect("mkdir proj");
        init_repo(&repo);

        let manager = WorktreeManager::new();
        manager
            .create(&repo, "main", "task one")
            .expect("first create");
        let err = manager
            .create(&repo, "main", "task one")
            .expect_err("second create should fail");
        assert!(matches!(err, WorktreeError::PathExists(_)), "got: {err:?}");
    }

    #[test]
    fn create_rejects_empty_slug() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = tmp.path().join("proj");
        fs::create_dir(&repo).expect("mkdir proj");
        init_repo(&repo);

        let err = WorktreeManager::new()
            .create(&repo, "main", "!!!")
            .expect_err("should reject");
        assert!(
            matches!(err, WorktreeError::InvalidTaskName(_)),
            "got: {err:?}"
        );
    }

    #[test]
    fn resolve_default_base_branch_falls_back_to_main() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = tmp.path().join("proj");
        fs::create_dir(&repo).expect("mkdir proj");
        init_repo(&repo);

        let resolved = resolve_default_base_branch(&repo);
        assert_eq!(resolved.as_deref(), Some("main"));
    }

    #[test]
    fn resolve_default_base_branch_returns_none_for_empty_repo() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = tmp.path().join("empty");
        fs::create_dir(&repo).expect("mkdir empty");
        run_git(&repo, &["init", "-q", "-b", "trunk"]).expect("git init");
        // No commit, no main/master branch, no origin. Resolver should give up.
        assert_eq!(resolve_default_base_branch(&repo), None);
    }
}
