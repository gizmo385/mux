use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Reserved host name standing for the local machine. May not be used as a
/// `[hosts.<name>]` key — the local host is implicit, not configured.
pub const LOCAL_HOST_NAME: &str = "local";

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(toml::de::Error),
    ReservedHostName(String),
    EmptySshTarget(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Parse(e) => write!(f, "parse: {e}"),
            Self::ReservedHostName(name) => {
                write!(f, "host name {name:?} is reserved")
            }
            Self::EmptySshTarget(name) => {
                write!(f, "host {name:?} has an empty `ssh` field")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct HostConfig {
    pub ssh: String,
    #[serde(default = "default_transcript_root")]
    pub transcript_root: PathBuf,
}

fn default_transcript_root() -> PathBuf {
    PathBuf::from("~/.claude/projects")
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub workspace_folders: Vec<PathBuf>,
    #[serde(default)]
    pub hosts: BTreeMap<String, HostConfig>,
}

impl Config {
    /// Load from `~/.config/agent-mux/config.toml` (or
    /// `$XDG_CONFIG_HOME/agent-mux/config.toml` if set). Missing file is not
    /// an error — returns a default `Config` with an empty workspace list.
    ///
    /// # Errors
    /// [`ConfigError::Io`] for unreadable files (other than not-found);
    /// [`ConfigError::Parse`] if the TOML is malformed;
    /// [`ConfigError::ReservedHostName`] if a `[hosts.<name>]` table uses a
    /// reserved name (see [`LOCAL_HOST_NAME`]);
    /// [`ConfigError::EmptySshTarget`] if a host's `ssh` field is empty.
    pub fn load() -> Result<Self, ConfigError> {
        match default_config_path() {
            Some(p) => Self::load_from(&p),
            None => Ok(Self::default()),
        }
    }

    /// Load from an explicit path. Same semantics as [`Config::load`].
    ///
    /// # Errors
    /// See [`Config::load`].
    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        let raw = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(ConfigError::Io(e)),
        };
        let mut cfg: Self = toml::from_str(&raw).map_err(ConfigError::Parse)?;
        cfg.workspace_folders = cfg.workspace_folders.iter().map(|p| expand(p)).collect();
        for (name, host) in &mut cfg.hosts {
            if name.eq_ignore_ascii_case(LOCAL_HOST_NAME) {
                return Err(ConfigError::ReservedHostName(name.clone()));
            }
            if host.ssh.trim().is_empty() {
                return Err(ConfigError::EmptySshTarget(name.clone()));
            }
            // No tilde expansion here: a remote host's `~/.claude/projects`
            // means the *remote* user's home, not ours. `SshHost` passes
            // the tilde through to the remote shell via `shell_quote_path`.
        }
        Ok(cfg)
    }
}

fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("agent-mux").join("config.toml"))
}

fn expand(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if s == "~" {
        return dirs::home_dir().unwrap_or_else(|| p.to_path_buf());
    }
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    p.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn expand_keeps_absolute_path() {
        assert_eq!(
            expand(Path::new("/work/repos")),
            PathBuf::from("/work/repos")
        );
    }

    #[test]
    fn expand_keeps_relative_path() {
        assert_eq!(expand(Path::new("work/repos")), PathBuf::from("work/repos"));
    }

    #[test]
    fn expand_handles_tilde_prefix() {
        let home = dirs::home_dir().expect("test runner has $HOME");
        assert_eq!(expand(Path::new("~/work")), home.join("work"));
    }

    #[test]
    fn expand_handles_bare_tilde() {
        let home = dirs::home_dir().expect("test runner has $HOME");
        assert_eq!(expand(Path::new("~")), home);
    }

    #[test]
    fn load_from_missing_file_returns_default() {
        let tmp = TempDir::new().expect("tempdir");
        let cfg =
            Config::load_from(&tmp.path().join("does-not-exist.toml")).expect("missing file is ok");
        assert!(cfg.workspace_folders.is_empty());
    }

    #[test]
    fn load_from_empty_file_returns_default() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(&path, "").expect("write");
        let cfg = Config::load_from(&path).expect("empty toml is ok");
        assert!(cfg.workspace_folders.is_empty());
    }

    #[test]
    fn load_from_parses_workspace_folders() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(
            &path,
            r#"workspace_folders = ["/work/repos", "/home/dev/code"]"#,
        )
        .expect("write");
        let cfg = Config::load_from(&path).expect("parse");
        assert_eq!(
            cfg.workspace_folders,
            vec![
                PathBuf::from("/work/repos"),
                PathBuf::from("/home/dev/code")
            ]
        );
    }

    #[test]
    fn load_from_expands_tilde_in_workspace_folders() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(&path, r#"workspace_folders = ["~/work"]"#).expect("write");
        let cfg = Config::load_from(&path).expect("parse");
        let home = dirs::home_dir().expect("home");
        assert_eq!(cfg.workspace_folders, vec![home.join("work")]);
    }

    #[test]
    fn load_from_rejects_malformed_toml() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(&path, "not = valid = toml").expect("write");
        let err = Config::load_from(&path).expect_err("should reject");
        assert!(matches!(err, ConfigError::Parse(_)), "got: {err:?}");
    }

    #[test]
    fn load_from_with_no_hosts_returns_empty_map() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(&path, r#"workspace_folders = ["/work"]"#).expect("write");
        let cfg = Config::load_from(&path).expect("parse");
        assert!(cfg.hosts.is_empty());
    }

    #[test]
    fn load_from_parses_hosts_with_defaults() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(
            &path,
            r#"
[hosts.devbox]
ssh = "devbox"
"#,
        )
        .expect("write");
        let cfg = Config::load_from(&path).expect("parse");
        let host = cfg.hosts.get("devbox").expect("devbox host");
        assert_eq!(host.ssh, "devbox");
        // Default is `~/.claude/projects`, kept as-is — the tilde must
        // resolve against the *remote* user's home, not ours.
        assert_eq!(host.transcript_root, PathBuf::from("~/.claude/projects"));
    }

    #[test]
    fn load_from_parses_explicit_transcript_root() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(
            &path,
            r#"
[hosts.gpu]
ssh = "gizmo@gpu-1.internal"
transcript_root = "/srv/claude/projects"
"#,
        )
        .expect("write");
        let cfg = Config::load_from(&path).expect("parse");
        let host = cfg.hosts.get("gpu").expect("gpu host");
        assert_eq!(host.ssh, "gizmo@gpu-1.internal");
        assert_eq!(host.transcript_root, PathBuf::from("/srv/claude/projects"));
    }

    #[test]
    fn load_from_leaves_tilde_unexpanded_in_remote_transcript_root() {
        // Regression: tilde in a remote-host `transcript_root` must
        // resolve against the *remote* home, not the local user's. The
        // config layer keeps it as a literal `~/…`; SshHost passes it
        // through to the remote shell intact.
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(
            &path,
            r#"
[hosts.devbox]
ssh = "devbox"
transcript_root = "~/scratch/claude"
"#,
        )
        .expect("write");
        let cfg = Config::load_from(&path).expect("parse");
        let host = cfg.hosts.get("devbox").expect("devbox host");
        assert_eq!(host.transcript_root, PathBuf::from("~/scratch/claude"));
    }

    #[test]
    fn load_from_parses_multiple_hosts() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(
            &path,
            r#"
workspace_folders = ["~/work"]

[hosts.devbox]
ssh = "devbox"

[hosts.gpu]
ssh = "user@gpu-1.internal"
transcript_root = "/srv/claude/projects"
"#,
        )
        .expect("write");
        let cfg = Config::load_from(&path).expect("parse");
        assert_eq!(cfg.hosts.len(), 2);
        // BTreeMap orders deterministically.
        let names: Vec<_> = cfg.hosts.keys().cloned().collect();
        assert_eq!(names, vec!["devbox".to_string(), "gpu".to_string()]);
    }

    #[test]
    fn load_from_rejects_reserved_local_host_name() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(
            &path,
            r#"
[hosts.local]
ssh = "anything"
"#,
        )
        .expect("write");
        let err = Config::load_from(&path).expect_err("should reject");
        assert!(
            matches!(err, ConfigError::ReservedHostName(ref n) if n == "local"),
            "got: {err:?}"
        );
    }

    #[test]
    fn load_from_rejects_reserved_local_host_name_case_insensitive() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(
            &path,
            r#"
[hosts.Local]
ssh = "anything"
"#,
        )
        .expect("write");
        let err = Config::load_from(&path).expect_err("should reject");
        assert!(
            matches!(err, ConfigError::ReservedHostName(_)),
            "got: {err:?}"
        );
    }

    #[test]
    fn load_from_rejects_empty_ssh_target() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(
            &path,
            r#"
[hosts.devbox]
ssh = "   "
"#,
        )
        .expect("write");
        let err = Config::load_from(&path).expect_err("should reject");
        assert!(
            matches!(err, ConfigError::EmptySshTarget(ref n) if n == "devbox"),
            "got: {err:?}"
        );
    }
}
