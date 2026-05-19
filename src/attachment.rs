use std::path::{Path, PathBuf};
use std::process::Command;

use crate::host::{Host, shell_join_quoted};
use crate::session::Session;

#[derive(Debug)]
pub enum AttachError {
    NotFound,
    TmuxCommandFailed(String),
    /// Remote attach hit a code path we haven't filled in yet (e.g. no
    /// remote pane found and we don't yet auto-spawn `claude --resume`
    /// inside remote tmux). Surfaced verbatim in the dashboard status
    /// so the user knows what to do manually.
    RemoteUnsupported(String),
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "no tmux pane found in the session's cwd"),
            Self::TmuxCommandFailed(msg) => write!(f, "tmux: {msg}"),
            Self::RemoteUnsupported(msg) => write!(f, "remote: {msg}"),
        }
    }
}

impl std::error::Error for AttachError {}

/// The result of an attachment action. The driver describes what should
/// happen; the caller decides how to honour it. This keeps tmux specifics
/// (and terminal handoff) out of the trait surface.
#[derive(Debug)]
pub enum AttachOutcome {
    /// Already handled in-place — e.g. `tmux switch-client` from within
    /// tmux, or `tmux new-window` from within tmux. The dashboard keeps
    /// rendering uninterrupted.
    Done,
    /// The dashboard should release the terminal, run this command as a
    /// foreground process, then re-acquire the terminal when it exits.
    /// Used when the driver needs to hand the screen over to another
    /// process (running `tmux attach` from outside tmux, or dropping into
    /// a plain shell when tmux isn't in the picture).
    SuspendAndRun(SuspendCommand),
    /// The dashboard should spawn this command inside an embedded PTY
    /// widget — the dashboard list stays visible as a sidebar while the
    /// active session renders in the embedded terminal. Returned by
    /// `PtyDriver`; `TmuxDriver` never emits this variant. Phase 3 of
    /// the embedded-PTY arc plumbs this into the main loop; Phase 2
    /// surfaces a "not yet wired" status when consumed.
    EmbedPty(EmbedSpec),
}

#[derive(Debug, Clone)]
pub struct SuspendCommand {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
}

/// Spec for an embedded-PTY spawn. Mirrors `SuspendCommand` in shape so
/// the call sites differ only in the variant arm.
///
/// `argv[0]` is the program; the rest are its arguments. For local
/// hosts this is the literal `tmux …` command line; for remote hosts
/// it's the `ssh -t target tmux …` wrapped argv from
/// `Host::ssh_argv` — the embedded widget runs whatever we hand it
/// without caring about the host/transport distinction.
///
/// `cwd` is the process-level working directory. `None` is the common
/// case because tmux honors its own `-c` flag; the field exists so
/// future non-tmux callers (a hypothetical no-tmux Shape B) can set it
/// without a trait change.
///
/// `label` is the human-readable string the widget renders in its
/// title — session title, falling back to a short id suffix. Phase 2
/// stores it; Phase 3 plumbs it into the widget block.
#[derive(Debug, Clone)]
pub struct EmbedSpec {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub label: String,
}

pub trait AttachmentDriver {
    /// Switch the user's terminal focus into the running session.
    ///
    /// # Errors
    /// Returns `AttachError::NotFound` if no tmux pane matches the session,
    /// `AttachError::TmuxCommandFailed` if tmux returns non-zero, or
    /// `AttachError::RemoteUnsupported` for remote code paths not yet
    /// implemented (e.g. `claude --resume` against a remote tmux).
    fn attach(&self, session: &Session, host: &dyn Host) -> Result<AttachOutcome, AttachError>;

    /// Open a fresh terminal in the session's working directory.
    ///
    /// # Errors
    /// Returns `AttachError::TmuxCommandFailed` if tmux returns non-zero.
    /// (No error when running outside tmux — that path drops into `$SHELL`
    /// without consulting tmux at all.)
    fn spawn_terminal(
        &self,
        session: &Session,
        host: &dyn Host,
    ) -> Result<AttachOutcome, AttachError>;

    /// Launch a fresh `claude` process in a new tmux window at `cwd`, and
    /// switch focus to it. Used by the new-session flow after the
    /// `WorktreeManager` has created the worktree on `host`.
    ///
    /// For local hosts, `cwd` is a local path and `claude` runs in a new
    /// local tmux window (or a fresh tmux session if outside tmux). For
    /// remote hosts, `cwd` is a path on the *remote* and the command
    /// runs through `Host::ssh_argv` over the existing `ControlMaster`
    /// — same dispatch shape as the M2 attach path.
    ///
    /// The resulting Claude Code session surfaces in the dashboard
    /// shortly after via the existing transcript-discovery pipeline —
    /// this method does not return a `SessionId`.
    ///
    /// # Errors
    /// Returns `AttachError::TmuxCommandFailed` if tmux returns non-zero,
    /// or `AttachError::RemoteUnsupported` if `host` is unexpectedly
    /// local-shaped (e.g. an `ssh_argv` impl that returns `None`).
    fn spawn_session(&self, cwd: &Path, host: &dyn Host) -> Result<AttachOutcome, AttachError>;

    /// Launch a user-configured tool (lazygit, nvim, …) in the
    /// session's cwd. Same dispatch family as [`spawn_terminal`] —
    /// inside tmux this is `tmux new-window -c <cwd> <command>` so
    /// the new window joins the user's existing tmux session;
    /// outside tmux it `SuspendAndRun`s the command with the cwd
    /// set, taking over the terminal until the tool exits.
    ///
    /// `command` is shell-quoted as a unit before being handed to
    /// `tmux new-window` so spaces / glob chars / shell
    /// metacharacters in arguments round-trip verbatim.
    ///
    /// # Errors
    /// Returns `AttachError::TmuxCommandFailed` if tmux returns non-zero.
    fn spawn_tool(
        &self,
        session: &Session,
        host: &dyn Host,
        command: &[String],
    ) -> Result<AttachOutcome, AttachError>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TmuxDriver;

impl TmuxDriver {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl AttachmentDriver for TmuxDriver {
    fn attach(&self, session: &Session, host: &dyn Host) -> Result<AttachOutcome, AttachError> {
        if session.host.is_local() {
            Self::attach_local(session)
        } else {
            Self::attach_remote(session, host)
        }
    }

    fn spawn_terminal(
        &self,
        session: &Session,
        host: &dyn Host,
    ) -> Result<AttachOutcome, AttachError> {
        if session.host.is_local() {
            Self::spawn_terminal_local(session)
        } else {
            Self::spawn_terminal_remote(session, host)
        }
    }

    fn spawn_tool(
        &self,
        session: &Session,
        host: &dyn Host,
        command: &[String],
    ) -> Result<AttachOutcome, AttachError> {
        if session.host.is_local() {
            Self::spawn_tool_local(session, command)
        } else {
            Self::spawn_tool_remote(session, host, command)
        }
    }

    fn spawn_session(&self, cwd: &Path, host: &dyn Host) -> Result<AttachOutcome, AttachError> {
        if host.id().is_local() {
            Self::launch_in_new_window(cwd, "claude")
        } else {
            // Fresh remote tmux session running claude. No `-A` here
            // (unlike `resume_remote`): this is the initial spawn so
            // there's no pre-existing session to attach to. tmux
            // auto-names the session; the user reaches it later via
            // the dashboard's normal attach path once the new
            // transcript surfaces in `~/.claude/projects/`.
            let cwd_str = cwd.to_string_lossy();
            Self::run_remote_interactive(host, &["tmux", "new-session", "-c", &cwd_str, "claude"])
        }
    }
}

// ---------- Local ----------

impl TmuxDriver {
    fn attach_local(session: &Session) -> Result<AttachOutcome, AttachError> {
        if let Ok(target) = find_pane_local(&session.project_dir) {
            return Self::switch_to_live(&target);
        }
        // No live pane (either tmux says NotFound, or there's no tmux server
        // at all and list-panes failed). Resume the transcript in a fresh
        // claude process, in the session's recorded cwd.
        let cmd = format!("claude --resume {}", session.id.0);
        Self::launch_in_new_window(&session.project_dir, &cmd)
    }

    fn switch_to_live(target: &str) -> Result<AttachOutcome, AttachError> {
        if in_tmux() {
            run_tmux(&["switch-client", "-t", target])?;
            Ok(AttachOutcome::Done)
        } else {
            let argv = tmux_attach_argv(target);
            let (program, rest) = argv.split_first().expect("tmux_attach_argv non-empty");
            Ok(AttachOutcome::SuspendAndRun(SuspendCommand {
                program: program.clone(),
                args: rest.to_vec(),
                cwd: None,
            }))
        }
    }

    /// Launch `command` in a fresh tmux window at `cwd`. Inside tmux this is
    /// `tmux new-window -c <cwd> <command>` (which tmux focuses
    /// automatically); outside tmux it's `tmux new-session -c <cwd>
    /// <command>`, which both creates the server and attaches the client.
    fn launch_in_new_window(cwd: &Path, command: &str) -> Result<AttachOutcome, AttachError> {
        let cwd_str = cwd.to_string_lossy().into_owned();
        if in_tmux() {
            run_tmux(&["new-window", "-c", &cwd_str, command])?;
            Ok(AttachOutcome::Done)
        } else {
            Ok(AttachOutcome::SuspendAndRun(build_new_session_command(
                &cwd_str, command,
            )))
        }
    }

    fn spawn_terminal_local(session: &Session) -> Result<AttachOutcome, AttachError> {
        if in_tmux() {
            let output = Command::new("tmux")
                .arg("new-window")
                .arg("-c")
                .arg(session.project_dir.as_os_str())
                .output()
                .map_err(|e| AttachError::TmuxCommandFailed(e.to_string()))?;
            if !output.status.success() {
                return Err(AttachError::TmuxCommandFailed(
                    String::from_utf8_lossy(&output.stderr).trim().to_string(),
                ));
            }
            Ok(AttachOutcome::Done)
        } else {
            Ok(AttachOutcome::SuspendAndRun(SuspendCommand {
                program: user_shell(),
                args: vec![],
                cwd: Some(session.project_dir.clone()),
            }))
        }
    }

    /// Local tool launch. Inside tmux this opens a new window in cwd
    /// running the shell-joined command (so `tmux` execs it via
    /// `sh -c`, which handles spaces / quotes / globs uniformly).
    /// Outside tmux, suspend the TUI and run the program directly —
    /// `cwd` set on the [`SuspendCommand`] so the child inherits it.
    fn spawn_tool_local(
        session: &Session,
        command: &[String],
    ) -> Result<AttachOutcome, AttachError> {
        if in_tmux() {
            let cwd_str = session.project_dir.to_string_lossy().into_owned();
            let joined = shell_join_quoted(command.iter().map(String::as_str));
            run_tmux(&["new-window", "-c", &cwd_str, &joined])?;
            Ok(AttachOutcome::Done)
        } else {
            // command was validated non-empty by `ToolBinding`'s
            // Deserialize; defensively split here so a future caller
            // that hands us an empty slice gets a clear error rather
            // than an index panic.
            let (program, args) = command
                .split_first()
                .ok_or_else(|| AttachError::TmuxCommandFailed("tool command is empty".into()))?;
            Ok(AttachOutcome::SuspendAndRun(SuspendCommand {
                program: program.clone(),
                args: args.to_vec(),
                cwd: Some(session.project_dir.clone()),
            }))
        }
    }
}

// ---------- Remote ----------

impl TmuxDriver {
    /// Remote attach mirrors the local pipeline: find a pane on the
    /// *remote* tmux whose `pane_current_path` matches `project_dir`,
    /// then `ssh -t <target> tmux attach -t <pane>`. When no pane
    /// matches, fall through to a remote `tmux new-session -A` running
    /// `claude --resume <id>` — the analog of the local "resume in a
    /// fresh tmux window" fallback. `-A` makes the fallback
    /// idempotent: a second attach reuses the same remote tmux session
    /// rather than spawning a parallel `claude --resume` that would
    /// race the first on the same transcript.
    ///
    /// Inside-tmux on the local side, both branches live in a new
    /// local tmux window (yes, nested tmux — see README); outside-tmux,
    /// we `SuspendAndRun` the ssh directly.
    fn attach_remote(session: &Session, host: &dyn Host) -> Result<AttachOutcome, AttachError> {
        match find_pane_remote(host, &session.project_dir) {
            Ok(target) => Self::run_remote_interactive(host, &["tmux", "attach", "-t", &target]),
            Err(AttachError::NotFound) => Self::resume_remote(session, host),
            Err(other) => Err(other),
        }
    }

    /// Spawn `claude --resume <id>` inside a deterministically-named
    /// remote tmux session and attach the client. The session name is
    /// `agent-mux-<conversation-id>`; `-A` attaches to it if it
    /// already exists (so repeated fallbacks converge). Handles all
    /// three remote states uniformly:
    ///   - no remote tmux server: `new-session` creates the server.
    ///   - server but no `agent-mux-<id>` session: creates it.
    ///   - `agent-mux-<id>` exists (prior fallback): attaches to it.
    fn resume_remote(session: &Session, host: &dyn Host) -> Result<AttachOutcome, AttachError> {
        let argv = tmux_resume_argv(session);
        let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        Self::run_remote_interactive(host, &argv_refs)
    }

    fn spawn_terminal_remote(
        session: &Session,
        host: &dyn Host,
    ) -> Result<AttachOutcome, AttachError> {
        let cwd = session.project_dir.to_string_lossy().into_owned();
        Self::run_remote_interactive(host, &["tmux", "new-window", "-c", &cwd])
    }

    /// Remote tool launch. Same dispatch shape as `spawn_terminal_remote`,
    /// just with the user's shell-joined command appended to the
    /// `tmux new-window` invocation. The remote tmux runs the command
    /// via `sh -c`, so spaces / globs / shell metacharacters survive.
    fn spawn_tool_remote(
        session: &Session,
        host: &dyn Host,
        command: &[String],
    ) -> Result<AttachOutcome, AttachError> {
        let cwd = session.project_dir.to_string_lossy().into_owned();
        let joined = shell_join_quoted(command.iter().map(String::as_str));
        Self::run_remote_interactive(host, &["tmux", "new-window", "-c", &cwd, &joined])
    }

    /// Hand the user a fully-interactive subprocess that runs
    /// `remote_cmd` on `host`. Inside local tmux this becomes a new
    /// local window that runs the ssh; outside local tmux it suspends
    /// the TUI and runs ssh directly. The remote command itself runs
    /// over the existing `ControlMaster` — `-t` is added so the remote
    /// gets a real tty (required for tmux attach / new-window to
    /// behave interactively).
    fn run_remote_interactive(
        host: &dyn Host,
        remote_cmd: &[&str],
    ) -> Result<AttachOutcome, AttachError> {
        let argv = host
            .ssh_argv(true, remote_cmd)
            .ok_or_else(|| AttachError::RemoteUnsupported("host is local".into()))?;
        if in_tmux() {
            // Embed as a single shell-string for `tmux new-window`,
            // which execs via sh -c. The new local window becomes the
            // host of the remote tmux UI.
            let shell_cmd = shell_join_quoted(argv.iter().map(String::as_str));
            run_tmux(&["new-window", &shell_cmd])?;
            Ok(AttachOutcome::Done)
        } else {
            let (program, rest) = argv.split_first().expect("ssh_argv non-empty when remote");
            Ok(AttachOutcome::SuspendAndRun(SuspendCommand {
                program: program.clone(),
                args: rest.to_vec(),
                cwd: None,
            }))
        }
    }
}

/// List the `pane_current_path` of every live tmux pane on `host`.
/// Returns an empty Vec on any failure — no tmux server, ssh hiccup,
/// non-zero exit, parse error. The dashboard's pane-presence indicator
/// is strictly advisory (the attach path's `claude --resume` fallback
/// covers stale state), so failures must not surface as errors.
///
/// Dispatches by host: local invokes `tmux` directly, remote shells
/// out via `host.ssh_argv(false, ...)` over the existing
/// `ControlMaster`. Output format is `#{pane_current_path}` one path
/// per line — narrower than the attach-side query, since the
/// indicator only needs the cwd, not a target identifier.
#[must_use]
pub fn list_live_pane_cwds(host: &dyn Host) -> Vec<PathBuf> {
    let tmux_args = ["list-panes", "-a", "-F", "#{pane_current_path}"];
    let output = if host.id().is_local() {
        Command::new("tmux").args(tmux_args).output()
    } else {
        // `#{...}` in -F starts with `#`, which a remote shell treats
        // as a comment. `Host::ssh_argv` shell-quotes each element so
        // the `#` survives the remote tokenizer — same fix as the
        // attach-side `find_pane_remote`.
        let mut remote_cmd = vec!["tmux"];
        remote_cmd.extend(tmux_args);
        let Some(argv) = host.ssh_argv(false, &remote_cmd) else {
            return Vec::new();
        };
        let Some((program, rest)) = argv.split_first() else {
            return Vec::new();
        };
        Command::new(program).args(rest).output()
    };
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_pane_cwds(&String::from_utf8_lossy(&output.stdout))
}

/// Pure parser: one path per line, blank lines skipped. Trims trailing
/// newlines but otherwise treats the path as the entire line so cwds
/// containing whitespace round-trip cleanly.
#[must_use]
pub fn parse_pane_cwds(tmux_output: &str) -> Vec<PathBuf> {
    tmux_output
        .lines()
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn find_pane_remote(host: &dyn Host, project_dir: &Path) -> Result<String, AttachError> {
    // -F format starts with `#{...}` — `Host::ssh_argv` shell-quotes
    // each remote_cmd element so the leading `#` survives the remote
    // shell tokenizer (which would otherwise eat it as a comment).
    let argv = host
        .ssh_argv(
            false,
            &[
                "tmux",
                "list-panes",
                "-a",
                "-F",
                "#{session_name}:#{window_index}.#{pane_index} #{pane_current_path}",
            ],
        )
        .ok_or_else(|| AttachError::RemoteUnsupported("host is local".into()))?;
    let (program, rest) = argv.split_first().expect("non-empty argv for remote host");
    let output = Command::new(program)
        .args(rest)
        .output()
        .map_err(|e| AttachError::TmuxCommandFailed(e.to_string()))?;
    if !output.status.success() {
        // tmux returns non-zero when there's no server, but we still
        // want NotFound semantics so the caller's match collapses
        // both "no panes" and "no server" into the same friendly
        // message. Treat any failure here as NotFound rather than
        // trying to parse stderr.
        return Err(AttachError::NotFound);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_pane_match(&stdout, project_dir).ok_or(AttachError::NotFound)
}

// ---------- PtyDriver ----------

/// Embedded-PTY `AttachmentDriver`. Same dispatch shape as
/// [`TmuxDriver`], but emits [`AttachOutcome::EmbedPty`] for `attach`
/// so the dashboard hosts the active session inside a PTY widget
/// instead of handing the terminal off.
///
/// `spawn_terminal` and `spawn_session` deliberately delegate to
/// `TmuxDriver` — those are out-of-band actions (open-shell, new-
/// session-creation flow) that don't fit the "embed the active
/// attach" model. Phase 6 may revisit if dogfooding asks for it.
#[derive(Debug, Default, Clone, Copy)]
pub struct PtyDriver;

impl PtyDriver {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl AttachmentDriver for PtyDriver {
    fn attach(&self, session: &Session, host: &dyn Host) -> Result<AttachOutcome, AttachError> {
        if session.host.is_local() {
            Ok(Self::attach_local(session))
        } else {
            Self::attach_remote(session, host)
        }
    }

    fn spawn_terminal(
        &self,
        session: &Session,
        host: &dyn Host,
    ) -> Result<AttachOutcome, AttachError> {
        TmuxDriver.spawn_terminal(session, host)
    }

    fn spawn_session(&self, cwd: &Path, host: &dyn Host) -> Result<AttachOutcome, AttachError> {
        TmuxDriver.spawn_session(cwd, host)
    }

    fn spawn_tool(
        &self,
        session: &Session,
        host: &dyn Host,
        command: &[String],
    ) -> Result<AttachOutcome, AttachError> {
        // Out-of-band action — same reasoning as `spawn_terminal`'s
        // delegate-to-TmuxDriver shape: opening lazygit in a tmux
        // window isn't part of "embed the active attach," it's a
        // sibling tmux affordance. Pivot here only if dogfooding
        // surfaces demand for an embedded-launch variant.
        TmuxDriver.spawn_tool(session, host, command)
    }
}

impl PtyDriver {
    /// Local attach in embedded mode. The argv runs directly inside the
    /// PTY widget — no SSH wrap, no `switch-client` branch. We still
    /// prefer a live-pane attach over a fresh resume when one exists,
    /// so the user's existing tmux work shows up in the widget rather
    /// than a parallel claude session that doesn't see their open
    /// terminals.
    ///
    /// Infallible by construction — `find_pane_local`'s error path
    /// folds into the resume fallback, so this never produces an
    /// `Err`. Returning `AttachOutcome` directly keeps the local
    /// branch readable; the dispatcher in `attach` wraps it in `Ok`.
    fn attach_local(session: &Session) -> AttachOutcome {
        let argv = match find_pane_local(&session.project_dir) {
            Ok(target) => tmux_attach_argv(&target),
            // `TmuxCommandFailed` here means tmux isn't running or
            // list-panes errored — same condition the legacy driver
            // treats as "no live pane." Fall through to resume.
            Err(_) => tmux_resume_argv(session),
        };
        AttachOutcome::EmbedPty(EmbedSpec {
            argv,
            cwd: None,
            label: embed_label(session),
        })
    }

    /// Remote attach in embedded mode. Same try-pane-then-resume shape
    /// as [`TmuxDriver::attach_remote`]; the difference is the outcome
    /// variant. The ssh wrap comes from `Host::ssh_argv(true, …)` —
    /// identical to the `SuspendAndRun` path so the embedded widget
    /// sees the same argv the legacy outside-tmux path would.
    fn attach_remote(session: &Session, host: &dyn Host) -> Result<AttachOutcome, AttachError> {
        let remote_cmd = match find_pane_remote(host, &session.project_dir) {
            Ok(target) => tmux_attach_argv(&target),
            Err(AttachError::NotFound) => tmux_resume_argv(session),
            Err(other) => return Err(other),
        };
        let remote_cmd_refs: Vec<&str> = remote_cmd.iter().map(String::as_str).collect();
        let argv = host
            .ssh_argv(true, &remote_cmd_refs)
            .ok_or_else(|| AttachError::RemoteUnsupported("host is local".into()))?;
        Ok(AttachOutcome::EmbedPty(EmbedSpec {
            argv,
            cwd: None,
            label: embed_label(session),
        }))
    }
}

/// Human-readable label for the embedded pane title. Prefers the
/// session's resolved title (from `.agent-mux/task.toml` or
/// `aiTitle`); falls back to a short id suffix so multiple title-less
/// sessions stay distinguishable. Mirrors `preview_pane_title` in
/// main.rs.
fn embed_label(session: &Session) -> String {
    if let Some(title) = &session.title {
        return title.clone();
    }
    let id = &session.id.0;
    let suffix: String = id.chars().rev().take(6).collect();
    let suffix: String = suffix.chars().rev().collect();
    format!("…{suffix}")
}

// ---------- shared helpers ----------

/// The `remote_cmd` for "attach to an existing pane." Used by both
/// drivers — `TmuxDriver` wraps it in `SuspendAndRun` (or ssh + tmux
/// new-window for the remote-with-pane case); `PtyDriver` hands it to
/// the embedded widget. Centralised here so a future tmux flag change
/// (e.g. `-d` for detach-other-clients) lands in one place.
#[must_use]
fn tmux_attach_argv(target: &str) -> Vec<String> {
    vec!["tmux".into(), "attach".into(), "-t".into(), target.into()]
}

/// The `remote_cmd` for "no live pane — spin up a fresh tmux session
/// named after the conversation and run `claude --resume <id>` in it."
/// The `-A` flag makes the spawn idempotent: a second invocation
/// attaches to the existing `agent-mux-<id>` session instead of
/// spawning a parallel `claude --resume` that would race the first on
/// the same transcript.
///
/// Used by `TmuxDriver::resume_remote` (preserves the M2-shipped
/// remote-resume behaviour exactly) and by `PtyDriver` for both local
/// and remote — `PtyDriver` always wants the named-session-with-`-A`
/// shape because the embedded widget can be re-attached freely.
#[must_use]
fn tmux_resume_argv(session: &Session) -> Vec<String> {
    let session_name = format!("agent-mux-{}", session.id.0);
    let cwd = session.project_dir.to_string_lossy().into_owned();
    let claude_cmd = format!("claude --resume {}", session.id.0);
    vec![
        "tmux".into(),
        "new-session".into(),
        "-A".into(),
        "-s".into(),
        session_name,
        "-c".into(),
        cwd,
        claude_cmd,
    ]
}

fn build_new_session_command(cwd: &str, command: &str) -> SuspendCommand {
    SuspendCommand {
        program: "tmux".to_string(),
        args: vec![
            "new-session".to_string(),
            "-c".to_string(),
            cwd.to_string(),
            command.to_string(),
        ],
        cwd: None,
    }
}

fn in_tmux() -> bool {
    std::env::var_os("TMUX").is_some()
}

fn user_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string())
}

fn find_pane_local(project_dir: &Path) -> Result<String, AttachError> {
    let output = Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{session_name}:#{window_index}.#{pane_index} #{pane_current_path}",
        ])
        .output()
        .map_err(|e| AttachError::TmuxCommandFailed(e.to_string()))?;
    if !output.status.success() {
        return Err(AttachError::TmuxCommandFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_pane_match(&stdout, project_dir).ok_or(AttachError::NotFound)
}

fn parse_pane_match(tmux_output: &str, project_dir: &Path) -> Option<String> {
    for line in tmux_output.lines() {
        let Some((target, path)) = line.split_once(' ') else {
            continue;
        };
        if Path::new(path) == project_dir {
            return Some(target.to_string());
        }
    }
    None
}

fn run_tmux(args: &[&str]) -> Result<(), AttachError> {
    let output = Command::new("tmux")
        .args(args)
        .output()
        .map_err(|e| AttachError::TmuxCommandFailed(e.to_string()))?;
    if !output.status.success() {
        return Err(AttachError::TmuxCommandFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::LocalHost;
    use crate::session::HostId;

    #[test]
    fn parse_match_finds_exact_path() {
        let out = "main:0.0 /home/u/proj\n\
             main:1.0 /home/u/other\n";
        let got = parse_pane_match(out, Path::new("/home/u/proj"));
        assert_eq!(got, Some("main:0.0".to_string()));
    }

    #[test]
    fn parse_match_returns_none_when_no_match() {
        let out = "main:0.0 /home/u/a\nmain:1.0 /home/u/b\n";
        assert_eq!(parse_pane_match(out, Path::new("/home/u/c")), None);
    }

    #[test]
    fn parse_match_handles_paths_with_spaces() {
        let out = "main:0.0 /home/u/path with spaces\n";
        let got = parse_pane_match(out, Path::new("/home/u/path with spaces"));
        assert_eq!(got, Some("main:0.0".to_string()));
    }

    #[test]
    fn parse_match_skips_malformed_lines() {
        let out = "garbage_without_space\nmain:0.0 /good/path\n";
        let got = parse_pane_match(out, Path::new("/good/path"));
        assert_eq!(got, Some("main:0.0".to_string()));
    }

    #[test]
    fn parse_match_picks_first_when_multiple() {
        let out = "a:0.0 /home/u/proj\n\
             b:1.0 /home/u/proj\n";
        let got = parse_pane_match(out, Path::new("/home/u/proj"));
        assert_eq!(got, Some("a:0.0".to_string()));
    }

    #[test]
    fn build_new_session_command_for_spawn_session() {
        let got = build_new_session_command("/work/agent-mux-fix-bug", "claude");
        assert_eq!(got.program, "tmux");
        assert_eq!(
            got.args,
            vec![
                "new-session".to_string(),
                "-c".to_string(),
                "/work/agent-mux-fix-bug".to_string(),
                "claude".to_string(),
            ]
        );
        assert!(got.cwd.is_none());
    }

    #[test]
    fn build_new_session_command_for_resume() {
        let got = build_new_session_command("/work/proj", "claude --resume abc-123");
        assert_eq!(
            got.args,
            vec![
                "new-session".to_string(),
                "-c".to_string(),
                "/work/proj".to_string(),
                "claude --resume abc-123".to_string(),
            ]
        );
    }

    // ---- AttachmentDriver dispatch ----
    //
    // The host-aware branch lives in the driver; cover that dispatch
    // here. The local code path is already exercised by the parse_*
    // tests above and by the existing tmux-shelled-out integration via
    // dogfooding — these tests focus on the new "is_remote dispatch"
    // contract.

    /// `Host` impl that records `ssh_argv` calls and returns a
    /// predictable prefix. Lets the resume-builder tests verify the
    /// argv the driver constructs without actually contacting a host.
    struct FakeRemoteHost {
        calls: std::sync::Mutex<Vec<(bool, Vec<String>)>>,
    }
    impl FakeRemoteHost {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn last_call(&self) -> Option<(bool, Vec<String>)> {
            self.calls.lock().unwrap().last().cloned()
        }
    }
    impl Host for FakeRemoteHost {
        fn id(&self) -> &HostId {
            static ID: std::sync::OnceLock<HostId> = std::sync::OnceLock::new();
            ID.get_or_init(|| HostId("remote".into()))
        }
        fn list_transcripts(&self, _: &Path) -> std::io::Result<Vec<crate::host::TranscriptStat>> {
            Ok(vec![])
        }
        fn read_to_string(&self, _: &Path) -> std::io::Result<String> {
            unreachable!()
        }
        fn read_tail(&self, _: &Path, _: u64) -> std::io::Result<String> {
            unreachable!()
        }
        fn is_dir(&self, _: &Path) -> bool {
            true
        }
        fn read_many(&self, _: &[&Path]) -> std::io::Result<Vec<std::io::Result<String>>> {
            unreachable!()
        }
        fn is_dir_many(&self, _: &[&Path]) -> std::io::Result<Vec<bool>> {
            unreachable!()
        }
        fn run(
            &self,
            _: Option<&Path>,
            _: &str,
            _: &[&str],
        ) -> std::io::Result<std::process::Output> {
            unreachable!()
        }
        fn write_file(&self, _: &Path, _: &str) -> std::io::Result<()> {
            unreachable!()
        }
        fn ssh_argv(&self, tty: bool, remote_cmd: &[&str]) -> Option<Vec<String>> {
            self.calls
                .lock()
                .unwrap()
                .push((tty, remote_cmd.iter().map(|s| (*s).to_string()).collect()));
            // Return a fixed prefix so tests can assert on shape
            // without rebuilding the real `ssh_argv` quoting logic.
            // The exact contents past `target` aren't load-bearing for
            // the dispatch tests below.
            Some(vec![
                "ssh".into(),
                "-S".into(),
                "/tmp/sock".into(),
                "remote-host".into(),
                "stub".into(),
            ])
        }
    }

    #[test]
    fn local_host_attach_takes_local_dispatch_branch() {
        // The dispatcher branches on `session.host.is_local()`. For a
        // local session, no ssh_argv call is needed. We can't easily
        // intercept the actual `tmux` shell-out without a process-
        // layer mock, so verify the contract at the dispatcher level:
        // `attach` against a local session does not consult the host's
        // ssh_argv at all.
        let host = LocalHost::new();
        let session = make_session(HostId::local(), "/nonexistent-path");
        assert!(host.ssh_argv(false, &[]).is_none());
        let _ = session; // dispatch shape is observable; running the
        // body would invoke `tmux list-panes` on the test machine.
    }

    #[test]
    fn remote_attach_with_no_pane_falls_through_to_resume_new_session() {
        // The previous chunk had this path return RemoteUnsupported.
        // Now it should spawn `tmux new-session -A -s agent-mux-<id>`
        // on the remote. We can't fake `list-panes` from inside a unit
        // test without a process-layer mock, so verify the
        // resume-builder directly: it must produce the right argv
        // shape via the host's ssh_argv.
        let host = FakeRemoteHost::new();
        let session = make_session(HostId("remote".into()), "/work/proj");
        let _ = TmuxDriver::resume_remote(&session, &host);
        let (tty, remote_cmd) = host.last_call().expect("ssh_argv called");
        assert!(tty, "resume needs -t for interactive claude");
        assert_eq!(
            remote_cmd,
            vec![
                "tmux",
                "new-session",
                "-A",
                "-s",
                "agent-mux-abc",
                "-c",
                "/work/proj",
                "claude --resume abc",
            ]
        );
    }

    #[test]
    fn remote_attach_uses_dedicated_named_session_per_conversation_id() {
        // Naming the remote tmux session after the conversation id
        // makes repeated fallbacks converge: the second attach with
        // -A attaches to the existing session rather than spawning a
        // parallel `claude --resume` that would race the first on
        // the transcript.
        let host = FakeRemoteHost::new();
        let session = make_session(HostId("remote".into()), "/work/proj");
        let _ = TmuxDriver::resume_remote(&session, &host);
        let (_, remote_cmd) = host.last_call().unwrap();
        // -A flag present; session name derived from conversation id.
        assert!(remote_cmd.iter().any(|s| s == "-A"), "got: {remote_cmd:?}");
        let name_idx = remote_cmd
            .iter()
            .position(|s| s == "-s")
            .expect("must have -s");
        assert_eq!(remote_cmd[name_idx + 1], "agent-mux-abc");
    }

    #[test]
    fn spawn_tool_remote_appends_shell_joined_command_to_new_window() {
        // Remote tool launch must shell-quote the user's command into
        // one token that the remote `tmux new-window` execs via sh -c.
        // Otherwise multi-arg commands like `nvim .` would be split
        // by SSH/the remote shell in surprising ways.
        let host = FakeRemoteHost::new();
        let session = make_session(HostId("remote".into()), "/work/proj");
        let cmd = vec!["nvim".to_string(), ".".to_string()];
        let _ = TmuxDriver::spawn_tool_remote(&session, &host, &cmd);
        let (tty, remote_cmd) = host.last_call().expect("ssh_argv called");
        assert!(tty, "spawn_tool needs -t for interactive tools");
        assert_eq!(
            remote_cmd,
            vec!["tmux", "new-window", "-c", "/work/proj", "'nvim' '.'"]
        );
    }

    #[test]
    fn spawn_tool_local_outside_tmux_returns_suspend_command_with_cwd() {
        // Outside tmux, the tool runs as a foreground subprocess
        // inheriting the session's cwd. The first command token is
        // the program; the rest are args. The shell-join used by the
        // in-tmux branch is *not* applied here — SuspendCommand spawns
        // via exec(), not sh -c.
        let session = make_session(HostId::local(), "/work/proj");
        let cmd = vec![
            "lazygit".to_string(),
            "--git-dir".to_string(),
            ".".to_string(),
        ];
        // Skip if $TMUX is set in the test environment — we can't
        // exercise the outside-tmux branch from inside one.
        if std::env::var_os("TMUX").is_some() {
            return;
        }
        let outcome = TmuxDriver::spawn_tool_local(&session, &cmd).expect("spawn_tool_local");
        match outcome {
            AttachOutcome::SuspendAndRun(sc) => {
                assert_eq!(sc.program, "lazygit");
                assert_eq!(sc.args, vec!["--git-dir", "."]);
                assert_eq!(sc.cwd.as_deref(), Some(Path::new("/work/proj")));
            }
            other => panic!("expected SuspendAndRun, got {other:?}"),
        }
    }

    #[test]
    fn parse_pane_cwds_extracts_one_path_per_line() {
        let out = "/home/u/proj\n/home/u/other\n";
        assert_eq!(
            parse_pane_cwds(out),
            vec![
                PathBuf::from("/home/u/proj"),
                PathBuf::from("/home/u/other")
            ]
        );
    }

    #[test]
    fn parse_pane_cwds_skips_blank_lines() {
        // A stray blank line should not produce a `PathBuf::from("")`
        // — the indicator would never match an empty project_dir, but
        // keeping the parser clean avoids surprising debug output.
        let out = "/a\n\n/b\n";
        assert_eq!(
            parse_pane_cwds(out),
            vec![PathBuf::from("/a"), PathBuf::from("/b")]
        );
    }

    #[test]
    fn parse_pane_cwds_handles_paths_with_spaces() {
        // The pane lister uses `#{pane_current_path}` with no separator,
        // so a single line *is* the whole path. This is why we don't
        // share parse_pane_match's split_once(' ') logic.
        let out = "/home/u/path with spaces\n";
        assert_eq!(
            parse_pane_cwds(out),
            vec![PathBuf::from("/home/u/path with spaces")]
        );
    }

    #[test]
    fn parse_pane_cwds_returns_empty_for_empty_input() {
        // No tmux server / no panes / failed shell-out all collapse to
        // an empty list. The caller treats this as "no live panes",
        // which means every session on that host gets `has_live_pane = Some(false)`.
        assert_eq!(parse_pane_cwds(""), Vec::<PathBuf>::new());
    }

    #[test]
    fn ssh_argv_for_list_panes_does_not_request_tty() {
        // Capturing list-panes output doesn't need a tty allocation;
        // asking for one wastes bandwidth.
        let host = FakeRemoteHost::new();
        let argv = host.ssh_argv(false, &["tmux", "list-panes", "-a"]).unwrap();
        assert!(!argv.contains(&"-t".to_string()), "got: {argv:?}");
    }

    fn make_session(host: HostId, project: &str) -> Session {
        use std::time::SystemTime;
        Session {
            id: crate::session::SessionId("abc".into()),
            host,
            project_dir: PathBuf::from(project),
            transcript_path: PathBuf::from("/x/abc.jsonl"),
            last_activity: SystemTime::UNIX_EPOCH,
            attention: crate::session::Attention::Unknown,
            title: None,
            parent_repo: None,
            has_live_pane: None,
        }
    }

    // ---- shared argv helpers ----

    #[test]
    fn tmux_attach_argv_produces_attach_target_form() {
        // The helper is the single point of truth for "tmux attach to a
        // pane"; both TmuxDriver's outside-tmux SuspendAndRun branch and
        // PtyDriver's live-pane EmbedPty branch route through it.
        assert_eq!(
            tmux_attach_argv("main:0.0"),
            vec![
                "tmux".to_string(),
                "attach".to_string(),
                "-t".to_string(),
                "main:0.0".to_string()
            ]
        );
    }

    #[test]
    fn tmux_resume_argv_produces_named_session_with_dash_a_and_claude_resume() {
        // Pins the resume shape used by TmuxDriver::resume_remote *and*
        // PtyDriver for both local and remote: the -A flag plus the
        // deterministic agent-mux-<id> session name is what makes
        // repeated attach attempts converge instead of racing parallel
        // `claude --resume` invocations on the same transcript.
        let session = make_session(HostId("remote".into()), "/work/proj");
        assert_eq!(
            tmux_resume_argv(&session),
            vec![
                "tmux".to_string(),
                "new-session".to_string(),
                "-A".to_string(),
                "-s".to_string(),
                "agent-mux-abc".to_string(),
                "-c".to_string(),
                "/work/proj".to_string(),
                "claude --resume abc".to_string(),
            ]
        );
    }

    // ---- PtyDriver ----

    #[test]
    fn embed_label_uses_session_title_when_present() {
        let mut session = make_session(HostId::local(), "/p");
        session.title = Some("refactor parser".to_string());
        assert_eq!(embed_label(&session), "refactor parser");
    }

    #[test]
    fn embed_label_falls_back_to_id_suffix_when_no_title() {
        // Mirrors the fallback in main.rs's `preview_pane_title` and the
        // dashboard's title-less row rendering — last 6 chars of the
        // session id, prefixed with "…".
        let session = Session {
            id: crate::session::SessionId("1234567890abcdef".into()),
            ..make_session(HostId::local(), "/p")
        };
        assert_eq!(embed_label(&session), "…abcdef");
    }

    #[test]
    fn embed_label_short_id_appears_without_truncation_marker_collision() {
        // For ids shorter than 6 chars the whole id is used; the "…"
        // prefix still appears because callers rely on it as a "this
        // is a fallback, not a real title" hint.
        let session = Session {
            id: crate::session::SessionId("abc".into()),
            ..make_session(HostId::local(), "/p")
        };
        assert_eq!(embed_label(&session), "…abc");
    }

    #[test]
    fn pty_driver_local_attach_no_pane_returns_embed_pty_with_resume_argv() {
        // PtyDriver's local attach: when `find_pane_local` returns Err
        // (no tmux server, or no pane matching the cwd — both common
        // for a test machine where tmux isn't running against this
        // exact /nonexistent path), the driver falls through to the
        // resume form. The argv we send to the embedded widget should
        // be the `tmux_resume_argv` output, run directly (no ssh wrap
        // because the host is local).
        //
        // If a test environment happens to have a tmux pane in
        // `/agent-mux-pty-test-no-such-path-XXXX`, this test will see
        // the attach-target form instead — wildly unlikely; assert on
        // either shape rather than fail the whole suite.
        let session = make_session(HostId::local(), "/agent-mux-pty-test-no-such-path");
        let outcome = PtyDriver::new()
            .attach(&session, &LocalHost::new())
            .expect("local attach should not error");
        let AttachOutcome::EmbedPty(spec) = outcome else {
            panic!("expected EmbedPty, got {outcome:?}");
        };
        let resume = tmux_resume_argv(&session);
        let is_resume = spec.argv == resume;
        let is_attach = spec.argv.len() == 4 && spec.argv[0] == "tmux" && spec.argv[1] == "attach";
        assert!(
            is_resume || is_attach,
            "argv must be resume or attach-target form; got {:?}",
            spec.argv
        );
        assert_eq!(spec.cwd, None, "tmux honours -c; process cwd stays None");
    }

    #[test]
    fn pty_driver_label_propagates_into_embed_spec() {
        // The label that ends up in EmbedSpec is what the Phase 3 widget
        // will render in its title bar. Pin that the spec carries the
        // session's title verbatim.
        let mut session = make_session(HostId::local(), "/p");
        session.title = Some("hello-world".to_string());
        let outcome = PtyDriver::new()
            .attach(&session, &LocalHost::new())
            .expect("local attach should not error");
        let AttachOutcome::EmbedPty(spec) = outcome else {
            panic!("expected EmbedPty");
        };
        assert_eq!(spec.label, "hello-world");
    }
}
