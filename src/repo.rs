use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::config::Config;
use crate::host::{Host, LocalHost};
use crate::session::HostId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    /// Which host owns the repo. `HostId::local()` for the local
    /// filesystem; the `[hosts.<name>]` config key for remote repos.
    /// Lets the new-session flow route `git worktree add` through the
    /// right host without an `is_local()` branch outside the trait.
    pub host: HostId,
    pub path: PathBuf,
    pub name: String,
}

impl Repo {
    fn new(host: HostId, path: PathBuf) -> Self {
        let name = path.file_name().map_or_else(
            || path.to_string_lossy().into_owned(),
            |n| n.to_string_lossy().into_owned(),
        );
        Self { host, path, name }
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
    /// Build the registry from config, scanning the *local* host
    /// synchronously. Remote hosts are not scanned here because their
    /// `Host` impls aren't available until each `[hosts.<name>]`
    /// reaches `Connected` — at which point [`Self::reconcile_host`]
    /// merges that host's slice in. Same shape as the M2 session
    /// catalog: local appears on first frame, remote streams in.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let local = LocalHost::new();
        let repos = scan_host_workspaces(&local, &config.workspace_folders);
        Self {
            repos: dedup_sorted(repos),
            last_scanned: SystemTime::now(),
        }
    }

    /// Re-scan the local host from the (possibly updated) Config and
    /// merge the result with the existing remote slices. Remote
    /// entries — added asynchronously by [`Self::reconcile_host`] when
    /// each `[hosts.<name>]` reaches `Connected` — survive the refresh
    /// because `Connected` only fires once per agent-mux process; if
    /// we dropped them here, the picker would lose them permanently on
    /// the first TTL-driven re-scan and the user would need to
    /// restart agent-mux to see remote repos again.
    pub fn refresh(&mut self, config: &Config) {
        let local = LocalHost::new();
        let local_repos = scan_host_workspaces(&local, &config.workspace_folders);
        self.reconcile_host(&HostId::local(), local_repos);
        self.last_scanned = SystemTime::now();
    }

    /// Re-scan only if the cached snapshot is older than `ttl`. Returns
    /// `true` if a refresh actually ran. Called by the Dashboard before
    /// opening the new-session picker.
    pub fn refresh_if_stale(&mut self, config: &Config, ttl: Duration) -> bool {
        if self.last_scanned.elapsed().is_ok_and(|e| e < ttl) {
            return false;
        }
        self.refresh(config);
        true
    }

    /// Replace this host's slice of the registry with `repos`. Called
    /// from the main thread after a background remote-workspace scan
    /// completes for a newly-connected host. Entries belonging to
    /// other hosts are untouched; the resulting list is re-sorted and
    /// de-duplicated against itself.
    pub fn reconcile_host(&mut self, host: &HostId, repos: Vec<Repo>) {
        self.repos.retain(|r| &r.host != host);
        self.repos.extend(repos);
        self.repos = dedup_sorted(std::mem::take(&mut self.repos));
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

/// Scan `folders` on `host` for direct children that contain a real
/// `.git/` directory (not a worktree pointer file). One trip across
/// the trait per folder: `Host::run("find", …)` to list candidates,
/// then `Host::is_dir_many` to batch the `.git` check. The same code
/// path serves local and remote — the trait dispatches.
///
/// Soft-fails per folder: a missing or unreadable workspace folder
/// disappears from the result rather than failing the whole scan.
/// Matches the M1 contract.
#[must_use]
pub fn scan_host_workspaces(host: &dyn Host, folders: &[PathBuf]) -> Vec<Repo> {
    let mut out = Vec::new();
    for folder in folders {
        let candidates = list_immediate_subdirs(host, folder);
        if candidates.is_empty() {
            continue;
        }
        let git_paths: Vec<PathBuf> = candidates.iter().map(|c| c.join(".git")).collect();
        let git_path_refs: Vec<&Path> = git_paths.iter().map(PathBuf::as_path).collect();
        let Ok(git_present) = host.is_dir_many(&git_path_refs) else {
            continue;
        };
        for (candidate, has_git) in candidates.into_iter().zip(git_present) {
            if has_git {
                out.push(Repo::new(host.id().clone(), candidate));
            }
        }
    }
    out
}

/// List immediate sub-directories of `folder` on `host` via
/// `find <folder> -mindepth 1 -maxdepth 1 -type d -print0`. Both
/// GNU find (Linux) and BSD find (macOS) accept these flags, so the
/// same invocation works whether `host` is local or remote. A
/// non-zero exit (missing folder, permission error) yields an empty
/// vec — mirrors `scan_one_folder`'s old soft-fail behaviour.
fn list_immediate_subdirs(host: &dyn Host, folder: &Path) -> Vec<PathBuf> {
    let folder_str = folder.to_string_lossy();
    let Ok(output) = host.run(
        None,
        "find",
        &[
            &folder_str,
            "-mindepth",
            "1",
            "-maxdepth",
            "1",
            "-type",
            "d",
            "-print0",
        ],
    ) else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    output
        .stdout
        .split(|&b| b == 0)
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| PathBuf::from(String::from_utf8_lossy(chunk).into_owned()))
        .collect()
}

fn dedup_sorted(mut repos: Vec<Repo>) -> Vec<Repo> {
    repos.sort_by(|a, b| {
        a.host
            .as_str()
            .cmp(b.host.as_str())
            .then_with(|| a.path.cmp(&b.path))
    });
    repos.dedup_by(|a, b| a.host == b.host && a.path == b.path);
    repos
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_repo_dir(parent: &Path, name: &str) -> PathBuf {
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
    fn scan_finds_only_repos_among_children_via_host_trait() {
        // The scanner routes through Host::run + is_dir_many so the
        // same code path serves local and remote. Pin the result for
        // a local host: real repos are kept, worktree pointers (.git
        // is a file, not a dir) are filtered, plain directories are
        // filtered. Repos carry the scanning host's id.
        let tmp = TempDir::new().expect("tempdir");
        make_repo_dir(tmp.path(), "a");
        make_repo_dir(tmp.path(), "b");
        make_worktree_pointer(tmp.path(), "a-task");
        make_plain_dir(tmp.path(), "notes");

        let host = LocalHost::new();
        let mut repos = scan_host_workspaces(&host, &[tmp.path().to_path_buf()]);
        repos.sort_by(|x, y| x.name.cmp(&y.name));
        let names: Vec<_> = repos.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        for r in &repos {
            assert!(r.host.is_local(), "repo carried wrong host: {:?}", r.host);
        }
    }

    #[test]
    fn scan_does_not_recurse() {
        let tmp = TempDir::new().expect("tempdir");
        let nested_parent = make_plain_dir(tmp.path(), "clients");
        make_repo_dir(&nested_parent, "big-client-repo");

        let host = LocalHost::new();
        let found = scan_host_workspaces(&host, &[tmp.path().to_path_buf()]);
        assert!(
            found.is_empty(),
            "depth-1 scan should not recurse: {found:?}"
        );
    }

    #[test]
    fn scan_returns_empty_for_missing_folder() {
        let tmp = TempDir::new().expect("tempdir");
        let missing = tmp.path().join("does-not-exist");
        let host = LocalHost::new();
        let found = scan_host_workspaces(&host, &[missing]);
        assert!(found.is_empty());
    }

    #[test]
    fn from_config_aggregates_across_local_workspace_folders() {
        let tmp = TempDir::new().expect("tempdir");
        let work = make_plain_dir(tmp.path(), "work");
        let code = make_plain_dir(tmp.path(), "code");
        make_repo_dir(&work, "alpha");
        make_repo_dir(&code, "beta");

        let cfg = Config {
            workspace_folders: vec![work, code],
            ..Default::default()
        };
        let reg = RepoRegistry::from_config(&cfg);
        let mut names: Vec<String> = reg.repos().iter().map(|r| r.name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
        assert!(reg.repos().iter().all(|r| r.host.is_local()));
    }

    #[test]
    fn from_config_dedups_overlapping_folders() {
        let tmp = TempDir::new().expect("tempdir");
        let work = make_plain_dir(tmp.path(), "work");
        make_repo_dir(&work, "shared");

        let cfg = Config {
            workspace_folders: vec![work.clone(), work],
            ..Default::default()
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
            ..Default::default()
        };
        let mut reg = RepoRegistry::from_config(&cfg);
        assert!(reg.is_empty());

        make_repo_dir(&work, "fresh");
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
            ..Default::default()
        };
        let mut reg = RepoRegistry::from_config(&cfg);
        assert!(reg.is_empty());

        make_repo_dir(&work, "added-after-boot");
        let did_refresh = reg.refresh_if_stale(&cfg, Duration::from_hours(1));
        assert!(!did_refresh);
        assert!(reg.is_empty(), "stale-cache value still served");
    }

    #[test]
    fn refresh_if_stale_runs_when_cache_is_expired() {
        let tmp = TempDir::new().expect("tempdir");
        let work = make_plain_dir(tmp.path(), "work");
        let cfg = Config {
            workspace_folders: vec![work.clone()],
            ..Default::default()
        };
        let mut reg = RepoRegistry::from_config(&cfg);

        make_repo_dir(&work, "added-after-boot");
        let did_refresh = reg.refresh_if_stale(&cfg, Duration::ZERO);
        assert!(did_refresh);
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.repos()[0].name, "added-after-boot");
    }

    #[test]
    fn refresh_preserves_remote_slices_added_via_reconcile_host() {
        // Pre-2026-05-19 regression: `refresh` used to overwrite the
        // whole registry via `from_config`, which dropped every remote
        // slice. Because `Connected` only fires once per agent-mux
        // process, the picker then lost remote repos permanently on
        // the first TTL-driven re-scan and the user had to restart
        // agent-mux to see them again. Pin the new behaviour so a
        // future "simplification" of `refresh` can't re-introduce the
        // bug.
        let tmp = TempDir::new().expect("tempdir");
        let work = make_plain_dir(tmp.path(), "work");
        make_repo_dir(&work, "local-repo");
        let cfg = Config {
            workspace_folders: vec![work.clone()],
            ..Default::default()
        };
        let mut reg = RepoRegistry::from_config(&cfg);

        let remote = HostId("gizmo".into());
        reg.reconcile_host(
            &remote,
            vec![Repo::new(remote.clone(), PathBuf::from("/srv/work/alpha"))],
        );
        assert_eq!(reg.len(), 2);

        // Refresh: rescans local (picks up newly-added local repo)
        // and keeps the remote slice intact.
        make_repo_dir(&work, "second-local");
        reg.refresh(&cfg);

        let mut by_host = reg
            .repos()
            .iter()
            .map(|r| (r.host.as_str(), r.name.as_str()))
            .collect::<Vec<_>>();
        by_host.sort_unstable();
        assert_eq!(
            by_host,
            vec![
                ("gizmo", "alpha"),
                (HostId::local().as_str(), "local-repo"),
                (HostId::local().as_str(), "second-local"),
            ],
            "refresh must preserve remote slices while re-scanning local"
        );
    }

    #[test]
    fn reconcile_host_adds_remote_repos_alongside_local() {
        // The post-Connected flow: local repos seeded from
        // `from_config`, then `reconcile_host` overlays a remote
        // host's slice. Both kinds coexist in the registry; the
        // picker decides how to render them.
        let tmp = TempDir::new().expect("tempdir");
        let work = make_plain_dir(tmp.path(), "work");
        make_repo_dir(&work, "local-repo");
        let cfg = Config {
            workspace_folders: vec![work],
            ..Default::default()
        };
        let mut reg = RepoRegistry::from_config(&cfg);
        assert_eq!(reg.len(), 1);

        let remote = HostId("gizmo".into());
        reg.reconcile_host(
            &remote,
            vec![
                Repo::new(remote.clone(), PathBuf::from("/srv/work/alpha")),
                Repo::new(remote.clone(), PathBuf::from("/srv/work/beta")),
            ],
        );
        assert_eq!(reg.len(), 3);
        let mut by_host = reg
            .repos()
            .iter()
            .map(|r| (r.host.as_str(), r.name.as_str()))
            .collect::<Vec<_>>();
        by_host.sort_unstable();
        assert_eq!(
            by_host,
            vec![
                ("gizmo", "alpha"),
                ("gizmo", "beta"),
                (HostId::local().as_str(), "local-repo")
            ],
        );
    }

    #[test]
    fn reconcile_host_replaces_only_that_hosts_slice() {
        // Calling reconcile_host twice for the same host replaces its
        // entries; entries for *other* hosts are untouched. Critical
        // for the polling/refresh path — a second connect must not
        // duplicate rows or drop other hosts' rows.
        let mut reg = RepoRegistry::default();
        let alpha = HostId("alpha".into());
        let beta = HostId("beta".into());
        reg.reconcile_host(&alpha, vec![Repo::new(alpha.clone(), PathBuf::from("/a1"))]);
        reg.reconcile_host(&beta, vec![Repo::new(beta.clone(), PathBuf::from("/b1"))]);
        reg.reconcile_host(
            &alpha,
            vec![
                Repo::new(alpha.clone(), PathBuf::from("/a2")),
                Repo::new(alpha.clone(), PathBuf::from("/a3")),
            ],
        );
        let paths: Vec<_> = reg
            .repos()
            .iter()
            .map(|r| (r.host.as_str(), r.path.display().to_string()))
            .collect();
        // alpha replaced; beta intact.
        assert!(paths.contains(&("alpha", "/a2".to_string())));
        assert!(paths.contains(&("alpha", "/a3".to_string())));
        assert!(paths.contains(&("beta", "/b1".to_string())));
        assert!(!paths.contains(&("alpha", "/a1".to_string())));
    }

    #[test]
    fn repo_name_derives_from_directory() {
        let r = Repo::new(HostId::local(), PathBuf::from("/work/agent-mux"));
        assert_eq!(r.name, "agent-mux");
    }
}
