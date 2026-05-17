use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

/// Stable identifier for the host a session runs on. For the implicit
/// local host this is the literal "local"; for SSH hosts (M2) this is
/// the `[hosts.<name>]` table key from the user's config.
///
/// Distinct from the [`crate::host::Host`] trait — that one is the
/// behavioural backend (read transcripts, list files, etc.); this one
/// is the per-session identifier that lets the catalog and dashboard
/// refer to a host without holding a trait object.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HostId(pub String);

impl HostId {
    /// Identifier for the implicit local host.
    #[must_use]
    pub fn local() -> Self {
        Self("local".to_string())
    }

    #[must_use]
    pub fn is_local(&self) -> bool {
        self.0 == "local"
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for HostId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Attention {
    NeedsInput,
    Working,
    Idle,
    #[default]
    Unknown,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: SessionId,
    pub host: HostId,
    pub project_dir: PathBuf,
    pub transcript_path: PathBuf,
    pub last_activity: SystemTime,
    pub attention: Attention,
    /// Human-readable label. Resolved in `discovery` from (in precedence)
    /// `.agent-mux/task.toml` → transcript `ai-title` entries → `None`.
    /// `None` means "no signal beyond the cwd" and the UI falls back to cwd
    /// alone.
    pub title: Option<String>,
}
