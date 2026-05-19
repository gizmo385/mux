use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::host::Host;

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

    /// Create a worktree in a hidden sibling of the parent repo on `host`.
    ///
    /// The new worktree lives at `<repo-parent>/.agent-mux-worktrees/<repo-name>-<slug>`
    /// on a new branch named `<slug>` branched off `base_branch`. The
    /// dot-prefixed parent keeps the workspace folder uncluttered under
    /// default `ls`; `git worktree add` creates intermediate directories
    /// on its own, so no explicit `mkdir -p` is needed. A `task.toml` file
    /// inside the worktree records the task metadata. All git invocations
    /// and the metadata write route through the [`Host`] trait, so the
    /// same code path works for local and SSH-reachable hosts — the trait
    /// dispatches.
    ///
    /// # Errors
    /// Returns [`WorktreeError`] for missing repo, git failure, empty-slug
    /// task name, path collision, or I/O failure while writing metadata.
    pub fn create(
        &self,
        host: &dyn Host,
        repo: &Path,
        base_branch: &str,
        task: &str,
    ) -> Result<PathBuf, WorktreeError> {
        let repo_root = repo_toplevel(host, repo)?;
        let slug = slugify(task).ok_or_else(|| WorktreeError::InvalidTaskName(task.to_string()))?;
        let path = worktree_path(&repo_root, &slug);
        // Use the host's is_dir rather than `path.exists()` so the
        // remote case works — the local FS doesn't know about a
        // worktree the remote has on disk.
        if host.is_dir(&path) {
            return Err(WorktreeError::PathExists(path));
        }
        run_git_via_host(
            host,
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
            host,
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

/// Resolve the repo's default branch name on `host`.
///
/// Tries `git symbolic-ref --short refs/remotes/origin/HEAD` and strips the
/// `origin/` prefix; falls back to `main` then `master` if either exists as
/// a local ref. Returns `None` if none resolve — the caller should prompt.
#[must_use]
pub fn resolve_default_base_branch(host: &dyn Host, repo: &Path) -> Option<String> {
    if let Some(name) = symbolic_ref_origin_head(host, repo) {
        return Some(name);
    }
    for fallback in ["main", "master"] {
        if branch_exists(host, repo, fallback) {
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
    parse_task_metadata(&raw)
}

/// Parse already-loaded task metadata TOML. Split from [`read_task_metadata`]
/// so callers that go through the [`crate::host::Host`] abstraction can do
/// their own (possibly remote) read and then parse here.
///
/// # Errors
/// Returns [`WorktreeError::TomlParse`] if the input doesn't deserialize.
pub fn parse_task_metadata(raw: &str) -> Result<TaskMetadata, WorktreeError> {
    toml::from_str(raw).map_err(WorktreeError::TomlParse)
}

/// Canonical relative path of a worktree's task metadata file, exposed so
/// the [`crate::host::Host`]-backed read in `discovery` doesn't duplicate
/// the convention.
#[must_use]
pub fn task_metadata_path(worktree: &Path) -> PathBuf {
    worktree.join(METADATA_PATH)
}

/// Relative path of the `.git` file/dir inside a working directory.
/// In a regular checkout this is a directory; in a worktree it's a
/// pointer file containing `gitdir: <parent-repo>/.git/worktrees/<id>`.
/// Exposed so `discovery` can request bulk reads through `Host::read_many`
/// without duplicating the path convention.
#[must_use]
pub fn git_pointer_path(working_dir: &Path) -> PathBuf {
    working_dir.join(".git")
}

/// Parse the content of a worktree's `.git` pointer file and return
/// the parent repo's working-tree path.
///
/// Git's worktree pointer format is one line: `gitdir: <abs-path>` where
/// `<abs-path>` ends in `/.git/worktrees/<name>`. Stripping that suffix
/// yields the parent repo's `.git` directory; one more pop yields the
/// parent repo's working tree, which is what the dashboard groups on.
///
/// Returns `None` for any content that doesn't match the worktree
/// pointer shape (including `.git` being a directory and therefore not
/// readable as a file — that case turns into a `NotFound`/`IsADirectory`
/// at the caller, never reaches this function).
#[must_use]
pub fn parse_parent_repo(git_pointer_contents: &str) -> Option<PathBuf> {
    let first = git_pointer_contents.lines().next()?.trim();
    let rest = first.strip_prefix("gitdir:")?.trim();
    let (parent, _) = rest.split_once("/.git/worktrees/")?;
    if parent.is_empty() {
        return None;
    }
    Some(PathBuf::from(parent))
}

fn write_task_metadata(
    host: &dyn Host,
    worktree: &Path,
    meta: &TaskMetadata,
) -> Result<(), WorktreeError> {
    // `mkdir -p` is portable across local and SSH; building the path
    // remotely avoids needing a dedicated `Host::create_dir_all`
    // primitive for one caller. The dotfile name is fixed so this
    // can't expand to a parent we didn't intend.
    let dir = worktree.join(".agent-mux");
    let dir_str = dir.to_string_lossy();
    let mkdir = host
        .run(None, "mkdir", &["-p", &dir_str])
        .map_err(|e| WorktreeError::GitFailed(format!("mkdir {dir_str}: {e}")))?;
    if !mkdir.status.success() {
        return Err(WorktreeError::GitFailed(format!(
            "mkdir {dir_str} failed: {}",
            String::from_utf8_lossy(&mkdir.stderr).trim()
        )));
    }
    let serialized = toml::to_string(meta).map_err(WorktreeError::TomlSerialize)?;
    host.write_file(&worktree.join(METADATA_PATH), &serialized)?;
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
    parent
        .join(".agent-mux-worktrees")
        .join(format!("{repo_name}-{slug}"))
}

fn repo_toplevel(host: &dyn Host, repo: &Path) -> Result<PathBuf, WorktreeError> {
    let stdout = run_git_stdout(host, repo, &["rev-parse", "--show-toplevel"])
        .map_err(|_| WorktreeError::NotARepo(repo.to_path_buf()))?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(WorktreeError::NotARepo(repo.to_path_buf()));
    }
    Ok(PathBuf::from(trimmed))
}

fn symbolic_ref_origin_head(host: &dyn Host, repo: &Path) -> Option<String> {
    let stdout = run_git_stdout(
        host,
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

fn branch_exists(host: &dyn Host, repo: &Path, name: &str) -> bool {
    let refspec = format!("refs/heads/{name}");
    host.run(
        Some(repo),
        "git",
        &["show-ref", "--verify", "--quiet", &refspec],
    )
    .is_ok_and(|out| out.status.success())
}

/// Run `git <args>` on `host` in the directory `cwd`, expecting it to
/// exit zero. Maps non-zero exit + stderr into [`WorktreeError::GitFailed`].
fn run_git_via_host(host: &dyn Host, cwd: &Path, args: &[&str]) -> Result<(), WorktreeError> {
    let output = host
        .run(Some(cwd), "git", args)
        .map_err(|e| WorktreeError::GitFailed(e.to_string()))?;
    if !output.status.success() {
        return Err(WorktreeError::GitFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(())
}

/// Same as [`run_git_via_host`] but returns captured stdout. Used for
/// the read-only `git symbolic-ref` / `rev-parse` calls.
fn run_git_stdout(host: &dyn Host, cwd: &Path, args: &[&str]) -> Result<String, WorktreeError> {
    let output = host
        .run(Some(cwd), "git", args)
        .map_err(|e| WorktreeError::GitFailed(e.to_string()))?;
    if !output.status.success() {
        return Err(WorktreeError::GitFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::LocalHost;
    use std::process::Command;
    use tempfile::TempDir;

    /// Local-only test helper: shell git directly without going through
    /// the `Host` trait so the repo-init scaffolding stays small. The
    /// production worktree code routes everything through `Host::run`;
    /// this helper is just for the surrounding test fixtures (init a
    /// bare repo, seed a commit). Pinned to local `Command::new` so
    /// tests don't accidentally exercise the trait dispatch we mean
    /// to test below it.
    fn run_git_directly(dir: &Path, args: &[&str]) {
        let mut cmd = Command::new("git");
        cmd.current_dir(dir).args(args);
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
        let status = cmd.status().expect("spawn git");
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    }

    fn init_repo(dir: &Path) {
        run_git_directly(dir, &["init", "-q", "-b", "main"]);
        run_git_directly(dir, &["config", "user.email", "test@example.com"]);
        run_git_directly(dir, &["config", "user.name", "test"]);
        run_git_directly(dir, &["config", "commit.gpgsign", "false"]);
        fs::write(dir.join("README.md"), "seed").expect("seed file");
        run_git_directly(dir, &["add", "."]);
        run_git_directly(dir, &["commit", "-q", "-m", "seed"]);
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
    fn parse_parent_repo_extracts_path_from_pointer_file() {
        // Canonical Git worktree pointer shape — `.git` is a regular
        // file whose first line is `gitdir: <abs>/.git/worktrees/<name>`.
        // The parent repo's *working tree* is everything to the left of
        // `/.git/worktrees/…`. Pin the exact shape because the dashboard
        // groups on this — a regression that returns the parent's `.git`
        // dir instead of the worktree would mis-group every session.
        let parent =
            parse_parent_repo("gitdir: /Users/dev/work/myproj/.git/worktrees/myproj-fix-bug\n");
        assert_eq!(parent.as_deref(), Some(Path::new("/Users/dev/work/myproj")));
    }

    #[test]
    fn parse_parent_repo_tolerates_no_trailing_newline_and_extra_whitespace() {
        // Real-world pointer files we've observed (e.g. `cat
        // ~/workspace/<wt>/.git`) have a trailing newline; defensive
        // input may not. Both must parse the same.
        let no_nl = parse_parent_repo("gitdir: /a/b/.git/worktrees/x");
        assert_eq!(no_nl.as_deref(), Some(Path::new("/a/b")));
        let extra_space = parse_parent_repo("gitdir:   /a/b/.git/worktrees/x  \n");
        assert_eq!(extra_space.as_deref(), Some(Path::new("/a/b")));
    }

    #[test]
    fn parse_parent_repo_returns_none_for_non_worktree_content() {
        // Anything that isn't a worktree pointer must not synthesize a
        // parent. Covers: arbitrary text (a stray file someone named
        // `.git`), a config file with a `gitdir` line that points at
        // something other than a worktree subdir, and an empty body.
        assert!(parse_parent_repo("hello world").is_none());
        assert!(parse_parent_repo("").is_none());
        assert!(parse_parent_repo("gitdir: /some/path").is_none());
        assert!(parse_parent_repo("gitdir: /some/.git/objects").is_none());
    }

    #[test]
    fn parse_parent_repo_ignores_lines_after_first() {
        // Git pointer files are single-line, but be lenient: only the
        // first line is the pointer. Don't fail on a trailing comment
        // or an editor-added line.
        let with_trailer = parse_parent_repo("gitdir: /a/b/.git/worktrees/x\nsome editor footer\n");
        assert_eq!(with_trailer.as_deref(), Some(Path::new("/a/b")));
    }

    #[test]
    fn worktree_path_lands_in_hidden_workspace_sibling() {
        // Worktrees live under `<repo-parent>/.agent-mux-worktrees/` so
        // they don't clutter the workspace folder under default `ls`.
        // The directory name keeps the `<repo>-<slug>` shape so two
        // worktrees off different repos with the same task name don't
        // collide.
        let path = worktree_path(Path::new("/work/agent-mux"), "fix-bug");
        assert_eq!(
            path,
            PathBuf::from("/work/.agent-mux-worktrees/agent-mux-fix-bug")
        );
    }

    #[test]
    fn create_writes_worktree_and_metadata() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = tmp.path().join("proj");
        fs::create_dir(&repo).expect("mkdir proj");
        init_repo(&repo);

        let host = LocalHost::new();
        let path = WorktreeManager::new()
            .create(&host, &repo, "main", "refactor parser")
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

        let host = LocalHost::new();
        let manager = WorktreeManager::new();
        manager
            .create(&host, &repo, "main", "task one")
            .expect("first create");
        let err = manager
            .create(&host, &repo, "main", "task one")
            .expect_err("second create should fail");
        assert!(matches!(err, WorktreeError::PathExists(_)), "got: {err:?}");
    }

    #[test]
    fn create_rejects_empty_slug() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = tmp.path().join("proj");
        fs::create_dir(&repo).expect("mkdir proj");
        init_repo(&repo);

        let host = LocalHost::new();
        let err = WorktreeManager::new()
            .create(&host, &repo, "main", "!!!")
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

        let host = LocalHost::new();
        let resolved = resolve_default_base_branch(&host, &repo);
        assert_eq!(resolved.as_deref(), Some("main"));
    }

    #[test]
    fn resolve_default_base_branch_reads_origin_head() {
        let tmp = TempDir::new().expect("tempdir");

        // Bare remote with a distinctive default branch name so a positive
        // result can't be explained by the main/master fallback.
        let remote = tmp.path().join("remote.git");
        fs::create_dir(&remote).expect("mkdir remote");
        run_git_directly(&remote, &["init", "-q", "--bare", "-b", "release"]);

        // Seed repo to publish one commit so the bare remote has a real HEAD.
        let seed = tmp.path().join("seed");
        fs::create_dir(&seed).expect("mkdir seed");
        run_git_directly(&seed, &["init", "-q", "-b", "release"]);
        run_git_directly(&seed, &["config", "user.email", "test@example.com"]);
        run_git_directly(&seed, &["config", "user.name", "test"]);
        run_git_directly(&seed, &["config", "commit.gpgsign", "false"]);
        fs::write(seed.join("README.md"), "seed").expect("seed file");
        run_git_directly(&seed, &["add", "."]);
        run_git_directly(&seed, &["commit", "-q", "-m", "seed"]);
        run_git_directly(
            &seed,
            &["remote", "add", "origin", &remote.to_string_lossy()],
        );
        run_git_directly(&seed, &["push", "-q", "origin", "release"]);

        // git clone sets refs/remotes/origin/HEAD from the remote's HEAD.
        let clone = tmp.path().join("clone");
        run_git_directly(
            tmp.path(),
            &[
                "clone",
                "-q",
                &remote.to_string_lossy(),
                &clone.to_string_lossy(),
            ],
        );

        let host = LocalHost::new();
        let resolved = resolve_default_base_branch(&host, &clone);
        assert_eq!(resolved.as_deref(), Some("release"));
    }

    #[test]
    fn resolve_default_base_branch_returns_none_for_empty_repo() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = tmp.path().join("empty");
        fs::create_dir(&repo).expect("mkdir empty");
        run_git_directly(&repo, &["init", "-q", "-b", "trunk"]);
        // No commit, no main/master branch, no origin. Resolver should give up.
        let host = LocalHost::new();
        assert_eq!(resolve_default_base_branch(&host, &repo), None);
    }
}
