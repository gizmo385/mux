//! The `AgentCli` seam — a sibling to [`crate::host::Host`] and
//! [`crate::attachment::AttachmentDriver`] that absorbs every
//! agent-CLI-specific concern behind one trait.
//!
//! agent-mux is a control plane over tmux plus on-disk transcripts. "The
//! transcript" and "the `claude` binary" are *per-agent* concepts: the
//! shape of the transcript tree on disk, how the JSONL is parsed for
//! attention / title / edited files, how a session's id is derived from
//! its transcript path, and how a session is spawned and resumed from the
//! CLI all vary by agent. Concentrating those behind [`AgentCli`] is what
//! lets a fourth agent CLI land as one new module under `src/agents/`
//! rather than a codebase sweep — exactly the property that let
//! `PtyDriver` slot in behind `AttachmentDriver` and remote hosts behind
//! `Host`.
//!
//! [`AgentKind`] (a `Copy` discriminator) — not `&dyn AgentCli` — is what
//! travels through channels, [`crate::session::Session`], and the disk
//! cache; behaviour is looked up at the point of use via [`agent`]. The
//! set of agents is closed per release (a parser is code), so an enum
//! costs nothing in extensibility and buys exhaustive `match` checking
//! wherever the kinds diverge.

use std::path::{Path, PathBuf};

use crate::session::{Attention, SessionId};

/// Copy-able discriminator carried on [`crate::session::Session`], watcher
/// events, and disk-cache entries. The set is closed per release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentKind {
    Claude,
    Codex,
    Pi,
}

impl AgentKind {
    /// Config-key + disk-cache string for this agent (`"claude"` etc.).
    /// Kept in sync with [`AgentCli::label`]; lives here as an inherent
    /// method so callers with only an [`AgentKind`] (the cache) don't have
    /// to reach through the registry.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Pi => "pi",
        }
    }

    /// Parse a [`label`](Self::label) back into an [`AgentKind`]. Returns
    /// `None` for an unrecognised label; the disk-cache load path maps
    /// that to the default (Claude) so an old or hand-edited snapshot
    /// still loads.
    #[must_use]
    pub fn from_label(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "pi" => Some(Self::Pi),
            _ => None,
        }
    }
}

/// Shape of an agent's on-disk transcript tree, executed by
/// [`crate::host::Host::list_transcripts`] (local `read_dir`, remote
/// `find`). Keeps the SSH mechanics in `Host` and the layout knowledge in
/// the agent. `mindepth`/`maxdepth` count directory levels below the root
/// (root children are depth 1), matching GNU `find`'s `-mindepth` /
/// `-maxdepth`; `name_glob` is a shell-style wildcard (only `*` is used).
///
/// Claude's spec is `{ mindepth: 2, maxdepth: 2, name_glob: "*.jsonl" }`,
/// which reproduces the pre-trait hardcoded `find <root> -mindepth 2
/// -maxdepth 2 -type f -name '*.jsonl'` byte-for-byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListingSpec {
    pub mindepth: usize,
    pub maxdepth: usize,
    pub name_glob: &'static str,
}

/// Head-of-transcript parse result: the working directory, a title (if
/// the agent surfaces one), and the first human-authored user message
/// (title fallback). All optional — a fresh transcript may carry none of
/// them yet.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TranscriptMeta {
    /// Working directory recorded in the transcript, if present. `None`
    /// when no line carried one; callers fall back to
    /// [`AgentCli::fallback_dir`].
    pub cwd: Option<PathBuf>,
    /// Agent-surfaced title (Claude's `ai-title` entry), if any.
    pub title: Option<String>,
    /// Normalised + truncated first non-empty human user message, used as
    /// a title fallback when neither a task name nor an agent title
    /// exists.
    pub first_user_message: Option<String>,
}

/// Tail-of-transcript (or whole-buffer) derivation: attention state, the
/// `from_tool_use` discriminator the catalog uses to protect a live hook
/// "blocked" pin, and the files edited within the scanned window. Bundled
/// so a single buffer walk yields all three without a second read — which
/// on a remote host would be a second SSH round-trip per poll tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDerivation {
    pub attention: Attention,
    /// `true` iff the last classified entry was an assistant tool-use (the
    /// transcript signature of an in-flight tool *or* a blocked permission
    /// prompt — indistinguishable from the transcript alone, which is why
    /// the hook exists). See
    /// [`crate::catalog::SessionCatalog::apply_heuristic_attention`].
    pub from_tool_use: bool,
    /// Absolute paths of files edited within the scanned buffer,
    /// most-recent-first, deduplicated, capped at
    /// [`crate::session::EDITED_FILES_CAP`].
    pub edited_files: Vec<PathBuf>,
}

/// How a new session is created. Two strategies exist in the wild.
pub enum SpawnPlan {
    /// The agent accepts a caller-chosen id (`claude --session-id`,
    /// `pi --session-id`): the minted uuid is simultaneously the tmux
    /// session-name suffix, the transcript stem/suffix, and the
    /// [`SessionId`] — today's identity contract, unchanged. `argv` is the
    /// agent-command tail (e.g. `["claude", "--session-id", "<uuid>"]`)
    /// that the Attachment Driver wraps in its `tmux new-session` shell.
    PinnedId { argv: Vec<String> },
    /// The agent refuses id pinning (codex): spawn with a provisional
    /// tmux name, then adopt the id from the transcript that appears. Not
    /// reachable until WP5 wires the adoption protocol; carried here so
    /// the seam is complete.
    DiscoverAfterSpawn { argv: Vec<String> },
}

/// Everything agent-CLI-specific, behind one trait. Unit-struct impls live
/// in `src/agents/<agent>.rs`, registered by [`agent`]. The surface is
/// deliberately narrow (Host-trait discipline): only the operations
/// discovery, the watcher, the host layer, and the attachment driver
/// actually need.
pub trait AgentCli: Send + Sync {
    /// This agent's [`AgentKind`] discriminator.
    fn kind(&self) -> AgentKind;

    /// Config-key + UI string (`"claude"`). Matches
    /// [`AgentKind::label`].
    fn label(&self) -> &'static str;

    /// Default binary name, overridable via the `[agents.<label>] binary`
    /// config key ([`crate::config::Config::agent_binary`]). The
    /// spawn/attach paths use this for the bare-launch case.
    fn default_binary(&self) -> &'static str;

    /// Default transcript root (`~/.claude/projects` for Claude). `None`
    /// when the platform has no resolvable home directory.
    fn default_transcript_root(&self) -> Option<PathBuf>;

    /// Shape of the on-disk transcript tree (see [`ListingSpec`]).
    fn listing(&self) -> ListingSpec;

    /// Is `path` a top-level transcript for this agent's tree rooted at
    /// `root`? Filters out sidechain/subagent transcripts nested deeper.
    fn is_transcript(&self, path: &Path, root: &Path) -> bool;

    /// Derive a [`SessionId`] from a transcript path (the file stem for
    /// Claude). `None` when the path yields no usable id.
    fn session_id_from_path(&self, path: &Path) -> Option<SessionId>;

    /// The `project_dir` to use when a transcript carries no `cwd` line —
    /// decoded from the transcript path (Claude's bucket-name decode).
    fn fallback_dir(&self, transcript_path: &Path) -> PathBuf;

    /// Head-of-file parse: cwd, title, first user message.
    fn parse_meta(&self, content: &str) -> TranscriptMeta;

    /// Tail-of-file (or whole-buffer) parse: attention + edited files.
    /// `cwd` is the session's working directory, used by agents whose
    /// edited-file paths can be relative (pi); unused by Claude.
    fn derive(&self, content: &str, cwd: &Path) -> AgentDerivation;

    /// How a new session is created for `cwd`, pinned to `minted_id` when
    /// the agent supports id pinning. See [`SpawnPlan`].
    fn spawn(&self, cwd: &Path, minted_id: &SessionId) -> SpawnPlan;

    /// Command string that resumes an existing session by id, used by the
    /// tmux resume fallback (`claude --resume <id>`).
    fn resume_command(&self, id: &SessionId) -> String;
}

static CLAUDE: crate::agents::claude::ClaudeAgent = crate::agents::claude::ClaudeAgent;
static CODEX: crate::agents::codex::CodexAgent = crate::agents::codex::CodexAgent;
static PI: crate::agents::pi::PiAgent = crate::agents::pi::PiAgent;

/// Static registry: resolve an [`AgentKind`] to its behaviour. The lookup
/// is O(1) and allocation-free (`&'static` unit structs), so call sites
/// carry the cheap `Copy` [`AgentKind`] and look behaviour up at the point
/// of use.
#[must_use]
pub fn agent(kind: AgentKind) -> &'static dyn AgentCli {
    match kind {
        AgentKind::Claude => &CLAUDE,
        AgentKind::Codex => &CODEX,
        AgentKind::Pi => &PI,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_round_trips_through_from_label() {
        for kind in [AgentKind::Claude, AgentKind::Codex, AgentKind::Pi] {
            assert_eq!(AgentKind::from_label(kind.label()), Some(kind));
        }
    }

    #[test]
    fn from_label_rejects_unknown() {
        assert_eq!(AgentKind::from_label("aider"), None);
    }

    #[test]
    fn registry_returns_matching_kind() {
        // Only Claude is exercised at runtime in WP1, but the registry
        // must resolve every variant so later work packages can construct
        // them.
        assert_eq!(agent(AgentKind::Claude).kind(), AgentKind::Claude);
        assert_eq!(agent(AgentKind::Claude).label(), "claude");
    }
}
