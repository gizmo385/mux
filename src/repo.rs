use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::config::Config;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub path: PathBuf,
    pub name: String,
}

impl Repo {
    fn new(path: PathBuf) -> Self {
        let name = path.file_name().map_or_else(
            || path.to_string_lossy().into_owned(),
            |n| n.to_string_lossy().into_owned(),
        );
        Self { path, name }
    }
}

#[derive(Debug, Clone)]
pub struct RepoRegistry {
    repos: Vec<Repo>,
    last_scanned: SystemTime,
}

impl Default for RepoRegistry {
    fn default() -> Self {
        Self {
            repos: Vec::new(),
            last_scanned: SystemTime::now(),
        }
    }
}

impl RepoRegistry {
    /// Scan each workspace folder one level deep and collect every direct
    /// child whose `.git` is a directory (i.e. an actual repo, not a
    /// worktree pointer file). Result is sorted and de-duplicated by path,
    /// so multiple workspace folders pointing at overlapping directories
    /// don't surface the same repo twice.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let mut repos: Vec<Repo> = config
            .workspace_folders
            .iter()
            .flat_map(|f| scan_one_folder(f))
            .collect();
        repos.sort_by(|a, b| a.path.cmp(&b.path));
        repos.dedup_by(|a, b| a.path == b.path);
        Self {
            repos,
            last_scanned: SystemTime::now(),
        }
    }

    /// Re-scan from the (possibly updated) Config. Replaces the cached list.
    pub fn refresh(&mut self, config: &Config) {
        *self = Self::from_config(config);
    }

    /// Re-scan only if the cached snapshot is older than `ttl`. Returns
    /// `true` if a refresh actually ran. Called by the Dashboard before
    /// opening the new-session picker: a depth-1 directory walk is cheap
    /// enough to run synchronously, so we don't bother with the "render
    /// from cache, refresh async" dance `ARCHITECTURE.md` allowed for —
    /// that escape hatch is reserved for scans that grow expensive.
    pub fn refresh_if_stale(&mut self, config: &Config, ttl: Duration) -> bool {
        if self.last_scanned.elapsed().is_ok_and(|e| e < ttl) {
            return false;
        }
        self.refresh(config);
        true
    }

    #[must_use]
    pub fn repos(&self) -> &[Repo] {
        &self.repos
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.repos.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.repos.len()
    }
}

fn scan_one_folder(folder: &Path) -> Vec<Repo> {
    // Missing or unreadable workspace folder is a soft failure: it disappears
    // from the registry rather than failing the whole scan. The picker shows
    // whichever folders did resolve.
    let Ok(entries) = fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && is_git_repo(&path) {
            out.push(Repo::new(path));
        }
    }
    out
}

fn is_git_repo(dir: &Path) -> bool {
    dir.join(".git").is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_repo(parent: &Path, name: &str) -> PathBuf {
        let p = parent.join(name);
        fs::create_dir_all(p.join(".git")).expect("mkdir .git");
        p
    }

    fn make_worktree_pointer(parent: &Path, name: &str) -> PathBuf {
        let p = parent.join(name);
        fs::create_dir(&p).expect("mkdir worktree");
        fs::write(p.join(".git"), "gitdir: /somewhere/main/.git/worktrees/x")
            .expect("write .git file");
        p
    }

    fn make_plain_dir(parent: &Path, name: &str) -> PathBuf {
        let p = parent.join(name);
        fs::create_dir_all(&p).expect("mkdir plain");
        p
    }

    #[test]
    fn is_git_repo_recognises_directory() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = make_repo(tmp.path(), "proj");
        assert!(is_git_repo(&repo));
    }

    #[test]
    fn is_git_repo_rejects_worktree_pointer() {
        let tmp = TempDir::new().expect("tempdir");
        let wt = make_worktree_pointer(tmp.path(), "proj-task");
        assert!(!is_git_repo(&wt));
    }

    #[test]
    fn is_git_repo_rejects_plain_directory() {
        let tmp = TempDir::new().expect("tempdir");
        let dir = make_plain_dir(tmp.path(), "notes");
        assert!(!is_git_repo(&dir));
    }

    #[test]
    fn scan_finds_only_repos_among_children() {
        let tmp = TempDir::new().expect("tempdir");
        make_repo(tmp.path(), "a");
        make_repo(tmp.path(), "b");
        make_worktree_pointer(tmp.path(), "a-task");
        make_plain_dir(tmp.path(), "notes");

        let mut found: Vec<String> = scan_one_folder(tmp.path())
            .into_iter()
            .map(|r| r.name)
            .collect();
        found.sort();
        assert_eq!(found, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn scan_does_not_recurse() {
        let tmp = TempDir::new().expect("tempdir");
        let nested_parent = make_plain_dir(tmp.path(), "clients");
        make_repo(&nested_parent, "big-client-repo");

        // `clients` is a plain dir, so it's not in the result. The repo nested
        // beneath it is invisible to a depth-1 scan.
        let found = scan_one_folder(tmp.path());
        assert!(
            found.is_empty(),
            "depth-1 scan should not recurse: {found:?}"
        );
    }

    #[test]
    fn scan_returns_empty_for_missing_folder() {
        let tmp = TempDir::new().expect("tempdir");
        let missing = tmp.path().join("does-not-exist");
        assert!(scan_one_folder(&missing).is_empty());
    }

    #[test]
    fn from_config_aggregates_across_folders() {
        let tmp = TempDir::new().expect("tempdir");
        let work = make_plain_dir(tmp.path(), "work");
        let code = make_plain_dir(tmp.path(), "code");
        make_repo(&work, "alpha");
        make_repo(&code, "beta");

        let cfg = Config {
            workspace_folders: vec![work, code],
        };
        let reg = RepoRegistry::from_config(&cfg);
        let mut names: Vec<String> = reg.repos().iter().map(|r| r.name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn from_config_dedups_overlapping_folders() {
        let tmp = TempDir::new().expect("tempdir");
        let work = make_plain_dir(tmp.path(), "work");
        make_repo(&work, "shared");

        // Same folder listed twice — the repo should only appear once.
        let cfg = Config {
            workspace_folders: vec![work.clone(), work],
        };
        let reg = RepoRegistry::from_config(&cfg);
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn from_config_with_no_workspace_folders_is_empty() {
        let cfg = Config::default();
        let reg = RepoRegistry::from_config(&cfg);
        assert!(reg.is_empty());
    }

    #[test]
    fn refresh_picks_up_newly_added_repos() {
        let tmp = TempDir::new().expect("tempdir");
        let work = make_plain_dir(tmp.path(), "work");
        let cfg = Config {
            workspace_folders: vec![work.clone()],
        };
        let mut reg = RepoRegistry::from_config(&cfg);
        assert!(reg.is_empty());

        make_repo(&work, "fresh");
        reg.refresh(&cfg);
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.repos()[0].name, "fresh");
    }

    #[test]
    fn refresh_if_stale_skips_when_cache_is_fresh() {
        let tmp = TempDir::new().expect("tempdir");
        let work = make_plain_dir(tmp.path(), "work");
        let cfg = Config {
            workspace_folders: vec![work.clone()],
        };
        let mut reg = RepoRegistry::from_config(&cfg);
        assert!(reg.is_empty());

        make_repo(&work, "added-after-boot");
        // 1h TTL against a sub-millisecond-old cache: should not refresh.
        let did_refresh = reg.refresh_if_stale(&cfg, Duration::from_secs(3600));
        assert!(!did_refresh);
        assert!(reg.is_empty(), "stale-cache value still served");
    }

    #[test]
    fn refresh_if_stale_runs_when_cache_is_expired() {
        let tmp = TempDir::new().expect("tempdir");
        let work = make_plain_dir(tmp.path(), "work");
        let cfg = Config {
            workspace_folders: vec![work.clone()],
        };
        let mut reg = RepoRegistry::from_config(&cfg);

        make_repo(&work, "added-after-boot");
        // Zero TTL: any cache age counts as stale.
        let did_refresh = reg.refresh_if_stale(&cfg, Duration::ZERO);
        assert!(did_refresh);
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.repos()[0].name, "added-after-boot");
    }

    #[test]
    fn repo_name_derives_from_directory() {
        let r = Repo::new(PathBuf::from("/work/agent-mux"));
        assert_eq!(r.name, "agent-mux");
    }
}
