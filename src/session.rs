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
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
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
    /// If `project_dir` is a git worktree, the path of the worktree's
    /// parent repo (resolved from the `gitdir: …/.git/worktrees/…`
    /// pointer file). `None` for sessions in a regular checkout or in
    /// a non-git directory. The dashboard groups sessions by
    /// `parent_repo.unwrap_or(project_dir)` so worktree-backed sessions
    /// land under one header alongside the parent repo's own sessions
    /// instead of each worktree getting its own project group.
    pub parent_repo: Option<PathBuf>,
    /// Whether the session's host has a live tmux pane whose
    /// `pane_current_path` matches `project_dir`. `Some(true)` means
    /// Enter is a fast switch into an existing pane; `Some(false)`
    /// means it'll fall through to `claude --resume` (spinning up a
    /// fresh tmux + claude — slower). `None` means the pane poller
    /// hasn't yet reported for this host (initial state).
    ///
    /// Deliberately *not* serialized into the disk cache: tmux state
    /// is ephemeral and can change without a corresponding session
    /// event, so it must always be re-derived at runtime from the
    /// pane poller.
    pub has_live_pane: Option<bool>,
    /// Timestamp of the most recent Claude Code `Notification` hook
    /// event ingested for this session, or `None` if no hook event
    /// has arrived (the heuristic path is fully authoritative).
    ///
    /// When `Some(t)`, the hook is authoritative: heuristic-derived
    /// attention updates whose source mtime is `<= t` are dropped (the
    /// hook event represents a more precise signal that hasn't yet
    /// been superseded by transcript progress). A heuristic update
    /// whose mtime is strictly `> t` clears `hook_pinned` and applies
    /// normally — the transcript has progressed past the hook event,
    /// so the heuristic is once again trustworthy.
    ///
    /// Not serialized into the disk cache: the hook signal is
    /// ephemeral and the cache only seeds first-frame display.
    pub hook_pinned: Option<SystemTime>,
    /// Whether the session's current `NeedsInput` is a Claude Code
    /// *blocking prompt* — a permission request or an elicitation
    /// dialog where the agent is actively waiting on the user's answer
    /// — as opposed to simply having finished its turn ("done"). Set by
    /// the `Notification`-hook ingest (only `permission_prompt` /
    /// `elicitation_dialog` flip it true; `idle_prompt` and the
    /// transcript heuristic leave it false), and cleared the moment the
    /// heuristic re-applies (transcript progressed past the prompt). The
    /// sidebar renders a distinct glyph when this is set *and* the
    /// session reads `NeedsInput`, so the user can tell "answer me" from
    /// "done" at a glance.
    ///
    /// Only meaningful while `attention == NeedsInput`; the display
    /// gates on that, so a stale `true` left over after the state moved
    /// on never mis-renders. Ephemeral like `hook_pinned`: not
    /// serialized into the disk cache (the live hook re-establishes it).
    pub blocking_prompt: bool,
}
