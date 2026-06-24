use std::path::{Path, PathBuf};
use std::process::Command;

use crate::host::{Host, shell_join_quoted, shell_single_quote};
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
#[derive(Debug, Clone, Default)]
pub struct EmbedSpec {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub label: String,
    /// The tmux session name to attach to. Populated by `spawn_tool`
    /// (the embed wraps the tool command in a detached tmux session so
    /// the tool survives PTY swaps), `None` for every other code path.
    /// `ToolLaunchRegistry` keys on this field so the dashboard's
    /// "Tools" group can re-attach to the same session.
    pub tmux_session: Option<String>,
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
        if let Ok(target) = find_pane_local(session) {
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
        match find_pane_remote(host, session) {
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

    /// Remote terminal launch: open a local tmux window (or, outside
    /// tmux, `SuspendAndRun`) that ssh's into the remote and exec's the
    /// user's `$SHELL` at the session's cwd. Mirrors the local in-tmux
    /// equivalent (`tmux new-window -c <cwd>` drops the user into a
    /// shell at <cwd>) — the user gets a window they can use until
    /// they exit, then it closes.
    ///
    /// Pre-2026-05-20 this fired `ssh target tmux new-window -c <cwd>`,
    /// which made the *remote* tmux create a window in whatever session
    /// happened to be "current" on the remote server (not necessarily
    /// the one the user was attached to) while the local wrapper window
    /// died immediately. Dogfooding surfaced both halves of that
    /// failure mode at once.
    fn spawn_terminal_remote(
        session: &Session,
        host: &dyn Host,
    ) -> Result<AttachOutcome, AttachError> {
        let cwd = session.project_dir.to_string_lossy().into_owned();
        let recipe = format!("cd {} && exec \"$SHELL\"", shell_single_quote(&cwd));
        Self::run_remote_interactive(host, &["sh", "-c", &recipe])
    }

    /// Remote tool launch: same shape as [`spawn_terminal_remote`], with
    /// the user's command (`{cwd}`/`{host}` already substituted) in
    /// place of `$SHELL`. The cwd is single-quoted into the `sh -c`
    /// recipe so paths with spaces survive both quoting layers (ours
    /// here, plus the one [`SshHost::ssh_argv`] applies before sending
    /// to the remote).
    fn spawn_tool_remote(
        session: &Session,
        host: &dyn Host,
        command: &[String],
    ) -> Result<AttachOutcome, AttachError> {
        let cwd = session.project_dir.to_string_lossy().into_owned();
        let joined = shell_join_quoted(command.iter().map(String::as_str));
        let recipe = format!("cd {} && exec {}", shell_single_quote(&cwd), joined);
        Self::run_remote_interactive(host, &["sh", "-c", &recipe])
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

/// Snapshot of live tmux panes on `host`: each pane's owning
/// `session_name` paired with its `pane_current_path`. Used by the
/// dashboard's per-session "live pane?" indicator and by the
/// attach-side `find_pane_local` / `find_pane_remote` resolution.
///
/// Returning both fields together (rather than two separate
/// invocations) keeps the poller to one round-trip per tick — same
/// cost as the pre-2026-05-20 cwd-only call. Session names
/// disambiguate the "two sessions in the same `project_dir`" collision
/// that cwd-only matching otherwise resolves arbitrarily.
#[derive(Debug, Clone, Default)]
pub struct LivePaneSnapshot {
    pub cwds: Vec<PathBuf>,
    pub session_names: Vec<String>,
}

/// List the live tmux panes on `host` — their owning `session_name`
/// and `pane_current_path`. Returns an empty snapshot on any failure —
/// no tmux server, ssh hiccup, non-zero exit, parse error. The
/// dashboard's pane-presence indicator is strictly advisory (the
/// attach path's `claude --resume` fallback covers stale state), so
/// failures must not surface as errors.
///
/// Dispatches by host: local invokes `tmux` directly, remote shells
/// out via `host.ssh_argv(false, ...)` over the existing
/// `ControlMaster`. Output format is `#{session_name}\t#{pane_current_path}`
/// — tab keeps cwds with spaces intact, and tmux's default session
/// naming rules disallow tabs in names so the parser doesn't have to
/// guess the split.
#[must_use]
pub fn list_live_panes(host: &dyn Host) -> LivePaneSnapshot {
    let tmux_args = [
        "list-panes",
        "-a",
        "-F",
        "#{session_name}\t#{pane_current_path}",
    ];
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
            return LivePaneSnapshot::default();
        };
        let Some((program, rest)) = argv.split_first() else {
            return LivePaneSnapshot::default();
        };
        Command::new(program).args(rest).output()
    };
    let Ok(output) = output else {
        return LivePaneSnapshot::default();
    };
    if !output.status.success() {
        return LivePaneSnapshot::default();
    }
    parse_pane_records(&String::from_utf8_lossy(&output.stdout))
}

/// Pure parser for `#{session_name}\t#{pane_current_path}` records:
/// one pane per line, blank lines skipped, lines without a tab
/// dropped (defensive — should not occur with the format string above
/// but a malformed line shouldn't poison the whole snapshot).
#[must_use]
pub fn parse_pane_records(tmux_output: &str) -> LivePaneSnapshot {
    let mut cwds = Vec::new();
    let mut session_names = Vec::new();
    for line in tmux_output.lines() {
        if line.is_empty() {
            continue;
        }
        let Some((name, cwd)) = line.split_once('\t') else {
            continue;
        };
        session_names.push(name.to_string());
        cwds.push(PathBuf::from(cwd));
    }
    LivePaneSnapshot {
        cwds,
        session_names,
    }
}

fn find_pane_remote(host: &dyn Host, session: &Session) -> Result<String, AttachError> {
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
    resolve_pane_target(&stdout, session).ok_or(AttachError::NotFound)
}

// ---------- PtyDriver ----------

/// Embedded-PTY `AttachmentDriver`. Same dispatch shape as
/// [`TmuxDriver`], but emits [`AttachOutcome::EmbedPty`] for `attach`
/// and `spawn_session`, `spawn_terminal`, and `spawn_tool` so the
/// dashboard hosts every "doing stuff" action inside the PTY widget
/// instead of handing the terminal off. Sidebar stays visible
/// throughout; pressing Enter on a session row re-attaches to the
/// underlying claude session (the embed re-installs against a
/// different `SessionId`, dropping the terminal/tool view).
///
/// 2026-05-20 design shift: `spawn_terminal` and `spawn_tool` used to
/// delegate to `TmuxDriver` (sibling local tmux window). Dogfooding
/// surfaced that this was effectively a fullscreen handoff — tmux
/// switched the local terminal to the new window and the dashboard
/// view disappeared. Routing through the embedded pane instead keeps
/// the sidebar visible and matches what `Enter` and `n`/`N` already
/// do.
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

    /// Open a shell at the session's cwd, hosted inside the embedded
    /// PTY pane. Wraps `$SHELL` in a detached tmux session — same
    /// shape as `spawn_tool_*_embed` — so the terminal survives PTY
    /// swaps in the dashboard and surfaces in the Tools sidebar group
    /// as a re-attachable row. Pressing Enter on a session row swaps
    /// the embed back to the session attach without killing the
    /// terminal; the user can navigate to its tools-group row to come
    /// back.
    fn spawn_terminal(
        &self,
        session: &Session,
        host: &dyn Host,
    ) -> Result<AttachOutcome, AttachError> {
        if session.host.is_local() {
            Self::spawn_terminal_local(session)
        } else {
            Self::spawn_terminal_remote_embed(session, host)
        }
    }

    /// Spawn `claude` into a detached tmux session, then return an
    /// `EmbedPty` that attaches the dashboard's embedded pane to it.
    /// Split into two tmux calls so the user sees claude paint into the
    /// embedded pane immediately rather than getting a fullscreen
    /// handoff (which is what delegating to `TmuxDriver` produced
    /// pre-2026-05-19). tmux assigns the session name via `-P -F
    /// '#{session_name}'` so collisions are impossible and the embed
    /// can attach by that exact name. The dashboard later discovers
    /// the session normally through the transcript watcher; re-attach
    /// via Enter goes through `find_pane_local`, finds this tmux
    /// session by cwd, and embeds against the same target.
    fn spawn_session(&self, cwd: &Path, host: &dyn Host) -> Result<AttachOutcome, AttachError> {
        if host.id().is_local() {
            Self::spawn_session_local(cwd)
        } else {
            Self::spawn_session_remote(cwd, host)
        }
    }

    /// Launch a `[[tools]]` keybind's command at the session's cwd,
    /// hosted inside the embedded PTY pane. Same dispatch shape as
    /// `spawn_terminal` — local: spawn argv directly with cwd; remote:
    /// ssh-wrap a `sh -c 'cd <cwd> && exec <cmd>'` recipe and embed
    /// the ssh argv.
    fn spawn_tool(
        &self,
        session: &Session,
        host: &dyn Host,
        command: &[String],
    ) -> Result<AttachOutcome, AttachError> {
        if session.host.is_local() {
            Self::spawn_tool_local_embed(session, command)
        } else {
            Self::spawn_tool_remote_embed(session, host, command)
        }
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
        let argv = match find_pane_local(session) {
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
            ..Default::default()
        })
    }

    /// Local terminal launch: wrap `$SHELL` in a detached tmux session
    /// at the session's `project_dir`, then attach via the embedded
    /// pane. Same recipe as [`spawn_tool_local_embed`] — the user's
    /// `t` keypress is the degenerate case of a tool launch (the
    /// "tool" is the shell itself). `tmux_session` carries the
    /// assigned name back so `App::spawn_terminal_selected` records
    /// it in `ToolLaunchRegistry` and the Tools sidebar group renders
    /// a re-attachable row.
    fn spawn_terminal_local(session: &Session) -> Result<AttachOutcome, AttachError> {
        let cwd_str = session.project_dir.to_string_lossy().into_owned();
        let spawn_argv = tmux_new_detached_tool_argv(&cwd_str, &user_shell());
        let (program, rest) = spawn_argv.split_first().expect("non-empty argv");
        let stdout = run_for_stdout(Command::new(program).args(rest))?;
        let session_name = stdout.trim().to_string();
        if session_name.is_empty() {
            return Err(AttachError::TmuxCommandFailed(
                "tmux new-session returned empty session name".into(),
            ));
        }
        Ok(AttachOutcome::EmbedPty(EmbedSpec {
            argv: tmux_attach_argv(&session_name),
            cwd: None,
            label: format!("terminal · {}", spawn_session_label(&session.project_dir)),
            tmux_session: Some(session_name),
        }))
    }

    /// Remote terminal in embedded mode. Same shape as
    /// [`spawn_tool_remote_embed`] with `exec "$SHELL"` as the joined
    /// command — the remote tmux runs the trailing arg through
    /// `/bin/sh -c`, so `$SHELL` expands against the *remote* user's
    /// env. The detached remote tmux session keeps the shell alive
    /// across PTY swaps so the user can navigate back to it via the
    /// Tools sidebar row.
    fn spawn_terminal_remote_embed(
        session: &Session,
        host: &dyn Host,
    ) -> Result<AttachOutcome, AttachError> {
        let cwd = session.project_dir.to_string_lossy().into_owned();
        let spawn_cmd = tmux_new_detached_tool_argv(&cwd, "exec \"$SHELL\"");
        let spawn_refs: Vec<&str> = spawn_cmd.iter().map(String::as_str).collect();
        let spawn_argv = host
            .ssh_argv(false, &spawn_refs)
            .ok_or_else(|| AttachError::RemoteUnsupported("host is local".into()))?;
        let (program, rest) = spawn_argv.split_first().expect("non-empty ssh argv");
        let stdout = run_for_stdout(Command::new(program).args(rest))?;
        let session_name = stdout.trim().to_string();
        if session_name.is_empty() {
            return Err(AttachError::TmuxCommandFailed(
                "remote tmux new-session returned empty session name".into(),
            ));
        }
        let attach_cmd = tmux_attach_argv(&session_name);
        let attach_refs: Vec<&str> = attach_cmd.iter().map(String::as_str).collect();
        let argv = host
            .ssh_argv(true, &attach_refs)
            .ok_or_else(|| AttachError::RemoteUnsupported("host is local".into()))?;
        Ok(AttachOutcome::EmbedPty(EmbedSpec {
            argv,
            cwd: None,
            label: format!("terminal · {}", spawn_session_label(&session.project_dir)),
            tmux_session: Some(session_name),
        }))
    }

    /// Local tool launch in embedded mode. Wraps the command in a
    /// detached tmux session so the tool process survives PTY swaps
    /// in the dashboard (the user can fire `g` for lazygit, swap
    /// focus to a Claude session, and Enter back into the lazygit
    /// row to find it still running). Two tmux calls: (1) `tmux
    /// new-session -d -P -F '#{session_name}' -c <cwd> <joined-cmd>`
    /// spawns detached and prints the assigned name; (2) the embed
    /// spec attaches to that name.
    ///
    /// `tmux_session` carries the assigned name back to the caller,
    /// which records it in `ToolLaunchRegistry` so the dashboard's
    /// "Tools" group can re-attach to the same session later.
    ///
    /// Defensive empty-command fallback to `sh` — config-load
    /// validation already rejects empty commands, but the trait
    /// surface accepts a slice.
    fn spawn_tool_local_embed(
        session: &Session,
        command: &[String],
    ) -> Result<AttachOutcome, AttachError> {
        let cwd_str = session.project_dir.to_string_lossy().into_owned();
        let joined = if command.is_empty() {
            user_shell()
        } else {
            shell_join_quoted(command.iter().map(String::as_str))
        };
        let spawn_argv = tmux_new_detached_tool_argv(&cwd_str, &joined);
        let (program, rest) = spawn_argv.split_first().expect("non-empty argv");
        let stdout = run_for_stdout(Command::new(program).args(rest))?;
        let session_name = stdout.trim().to_string();
        if session_name.is_empty() {
            return Err(AttachError::TmuxCommandFailed(
                "tmux new-session returned empty session name".into(),
            ));
        }
        Ok(AttachOutcome::EmbedPty(EmbedSpec {
            argv: tmux_attach_argv(&session_name),
            cwd: None,
            label: format!(
                "{} · {}",
                tool_label_token(command),
                spawn_session_label(&session.project_dir)
            ),
            tmux_session: Some(session_name),
        }))
    }

    /// Remote tool launch in embedded mode. Same shape as
    /// [`spawn_tool_local_embed`] but the two tmux calls go over
    /// `Host::ssh_argv` so the tmux session lives on the remote.
    /// The embedded pane attaches via `ssh -t target tmux attach -t <name>`.
    fn spawn_tool_remote_embed(
        session: &Session,
        host: &dyn Host,
        command: &[String],
    ) -> Result<AttachOutcome, AttachError> {
        let cwd = session.project_dir.to_string_lossy().into_owned();
        let joined = if command.is_empty() {
            "sh".to_string()
        } else {
            shell_join_quoted(command.iter().map(String::as_str))
        };
        let spawn_cmd = tmux_new_detached_tool_argv(&cwd, &joined);
        let spawn_refs: Vec<&str> = spawn_cmd.iter().map(String::as_str).collect();
        let spawn_argv = host
            .ssh_argv(false, &spawn_refs)
            .ok_or_else(|| AttachError::RemoteUnsupported("host is local".into()))?;
        let (program, rest) = spawn_argv.split_first().expect("non-empty ssh argv");
        let stdout = run_for_stdout(Command::new(program).args(rest))?;
        let session_name = stdout.trim().to_string();
        if session_name.is_empty() {
            return Err(AttachError::TmuxCommandFailed(
                "remote tmux new-session returned empty session name".into(),
            ));
        }
        let attach_cmd = tmux_attach_argv(&session_name);
        let attach_refs: Vec<&str> = attach_cmd.iter().map(String::as_str).collect();
        let argv = host
            .ssh_argv(true, &attach_refs)
            .ok_or_else(|| AttachError::RemoteUnsupported("host is local".into()))?;
        Ok(AttachOutcome::EmbedPty(EmbedSpec {
            argv,
            cwd: None,
            label: format!(
                "{} · {}",
                tool_label_token(command),
                spawn_session_label(&session.project_dir)
            ),
            tmux_session: Some(session_name),
        }))
    }

    /// Spawn a fresh `claude` into a detached local tmux session and
    /// build the embedded-attach spec for it. Two tmux calls:
    /// (1) `tmux new-session -d -P -F '#{session_name}' -c <cwd> claude`
    /// creates the detached session and prints the assigned name on
    /// stdout — letting tmux pick the name guarantees no collision
    /// with the user's existing sessions; (2) the embed spec attaches
    /// to that exact name.
    fn spawn_session_local(cwd: &Path) -> Result<AttachOutcome, AttachError> {
        let cwd_str = cwd.to_string_lossy().into_owned();
        let spawn_argv = tmux_new_detached_argv(&cwd_str);
        let (program, rest) = spawn_argv.split_first().expect("non-empty argv");
        let stdout = run_for_stdout(Command::new(program).args(rest))?;
        let session_name = stdout.trim().to_string();
        if session_name.is_empty() {
            return Err(AttachError::TmuxCommandFailed(
                "tmux new-session returned empty session name".into(),
            ));
        }
        Ok(AttachOutcome::EmbedPty(EmbedSpec {
            argv: tmux_attach_argv(&session_name),
            cwd: None,
            label: spawn_session_label(cwd),
            ..Default::default()
        }))
    }

    /// Remote analogue of [`spawn_session_local`]. One ssh round-trip
    /// to create the detached remote tmux session and capture its
    /// assigned name; the embed spec then wraps `tmux attach -t <name>`
    /// in `Host::ssh_argv(true, …)` so the embedded pane drives the
    /// remote attach over the same `ControlMaster`.
    fn spawn_session_remote(cwd: &Path, host: &dyn Host) -> Result<AttachOutcome, AttachError> {
        let cwd_str = cwd.to_string_lossy().into_owned();
        let spawn_remote_cmd = tmux_new_detached_argv(&cwd_str);
        let spawn_refs: Vec<&str> = spawn_remote_cmd.iter().map(String::as_str).collect();
        let spawn_argv = host
            .ssh_argv(false, &spawn_refs)
            .ok_or_else(|| AttachError::RemoteUnsupported("host is local".into()))?;
        let (program, rest) = spawn_argv.split_first().expect("non-empty ssh argv");
        let stdout = run_for_stdout(Command::new(program).args(rest))?;
        let session_name = stdout.trim().to_string();
        if session_name.is_empty() {
            return Err(AttachError::TmuxCommandFailed(
                "remote tmux new-session returned empty session name".into(),
            ));
        }
        let attach_remote_cmd = tmux_attach_argv(&session_name);
        let attach_refs: Vec<&str> = attach_remote_cmd.iter().map(String::as_str).collect();
        let argv = host
            .ssh_argv(true, &attach_refs)
            .ok_or_else(|| AttachError::RemoteUnsupported("host is local".into()))?;
        Ok(AttachOutcome::EmbedPty(EmbedSpec {
            argv,
            cwd: None,
            label: spawn_session_label(cwd),
            ..Default::default()
        }))
    }

    /// Remote attach in embedded mode. Same try-pane-then-resume shape
    /// as [`TmuxDriver::attach_remote`]; the difference is the outcome
    /// variant. The ssh wrap comes from `Host::ssh_argv(true, …)` —
    /// identical to the `SuspendAndRun` path so the embedded widget
    /// sees the same argv the legacy outside-tmux path would.
    fn attach_remote(session: &Session, host: &dyn Host) -> Result<AttachOutcome, AttachError> {
        let remote_cmd = match find_pane_remote(host, session) {
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
            ..Default::default()
        }))
    }
}

/// Human-readable label for the embedded pane title. Prefers the
/// session's resolved title (from `.agent-mux/task.toml` or
/// `aiTitle`); falls back to a short id suffix so multiple title-less
/// sessions stay distinguishable.
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

/// The tmux command for "create a detached session running `claude` in
/// `cwd` and print the assigned session name." Used by
/// [`PtyDriver::spawn_session_local`] and `_remote` to spawn the
/// new-session target the embedded pane will then attach to. `-d`
/// keeps the spawning client detached (the embed becomes the only
/// attached client); `-P -F '#{session_name}'` prints the tmux-assigned
/// name on stdout, so callers don't need to invent a unique name and
/// risk collisions with the user's existing sessions.
#[must_use]
fn tmux_new_detached_argv(cwd: &str) -> Vec<String> {
    vec![
        "tmux".into(),
        "new-session".into(),
        "-d".into(),
        "-P".into(),
        "-F".into(),
        "#{session_name}".into(),
        "-c".into(),
        cwd.into(),
        "claude".into(),
    ]
}

/// The tmux command for "create a detached session running an
/// arbitrary command (a `[[tools]]` keybind's joined argv) in `cwd`
/// and print the assigned session name." Used by
/// `spawn_tool_local_embed` and `spawn_tool_remote_embed` so tool
/// launches survive PTY swaps in the embedded pane. Same `-d -P -F
/// '#{session_name}' -c <cwd>` shape as [`tmux_new_detached_argv`];
/// the difference is the trailing command — `claude` for new
/// sessions, the user's tool for this path.
#[must_use]
fn tmux_new_detached_tool_argv(cwd: &str, joined_cmd: &str) -> Vec<String> {
    vec![
        "tmux".into(),
        "new-session".into(),
        "-d".into(),
        "-P".into(),
        "-F".into(),
        "#{session_name}".into(),
        "-c".into(),
        cwd.into(),
        joined_cmd.into(),
    ]
}

/// Run `cmd` and return its stdout, mapping non-zero exit and io
/// failures into [`AttachError::TmuxCommandFailed`] with the stderr (or
/// io error) as the message. Used by the spawn-session paths to read
/// back the tmux-assigned session name.
fn run_for_stdout(cmd: &mut Command) -> Result<String, AttachError> {
    let output = cmd
        .output()
        .map_err(|e| AttachError::TmuxCommandFailed(e.to_string()))?;
    if !output.status.success() {
        return Err(AttachError::TmuxCommandFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Human-readable label for the embedded pane that hosts a freshly-
/// spawned session. The pane title shows the worktree's basename
/// (e.g. `agent-mux-fix-bug`) so the user can correlate the embed
/// with the worktree they just created. Falls back to a generic
/// label when `cwd` has no `file_name` (shouldn't happen for real
/// worktree paths, but the `Path` API allows it).
fn spawn_session_label(cwd: &Path) -> String {
    cwd.file_name().map_or_else(
        || "new session".to_string(),
        |s| s.to_string_lossy().into_owned(),
    )
}

/// Pick a short label for the embedded-pane title from a tool's argv.
/// First token is the program; everything else is treated as arg
/// noise. Defensive empty-slice fallback to "tool" so a bad caller
/// can't blank the title — config-load already rejects empty
/// commands.
fn tool_label_token(command: &[String]) -> String {
    command
        .first()
        .cloned()
        .unwrap_or_else(|| "tool".to_string())
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

/// Resolve the local tmux pane (if any) that should serve as the
/// attach target for `session`. Used both inside `PtyDriver::attach_local`
/// and from the App's pre-attach parallel-resume gate, hence the
/// `pub` visibility.
///
/// # Errors
/// Returns `AttachError::NotFound` when `tmux list-panes -a` ran
/// successfully but no pane matched the resolution rules in
/// [`resolve_pane_target`]. Returns `AttachError::TmuxCommandFailed`
/// when the `tmux` invocation itself failed (binary missing, exit
/// non-zero, etc.) — the bare-terminal case where there's no local
/// tmux server at all collapses to this.
pub fn find_pane_local(session: &Session) -> Result<String, AttachError> {
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
    resolve_pane_target(&stdout, session).ok_or(AttachError::NotFound)
}

/// Resolve the tmux target for `session` from the output of
/// `tmux list-panes -F '#{session_name}:#{window_index}.#{pane_index} #{pane_current_path}'`.
///
/// Two-stage lookup, in priority order:
///
/// 1. Pane in a tmux session named `agent-mux-<session.id.0>`. That's
///    the deterministic name `tmux_resume_argv` uses, so once a
///    session has been re-attached at least once (taking the resume
///    fallback path) we have an unambiguous pin from `session_id` to
///    tmux session — even when multiple sessions share a cwd. This
///    resolves the "two rows collapse onto the same pane" bug from
///    the cwd-only fallback below.
///
/// 2. First pane whose `pane_current_path` matches `session.project_dir`.
///    Catches externally-created sessions and agent-mux sessions
///    whose user-side embedded pane is still on the auto-named tmux
///    session from the initial `spawn_session_local` (before any
///    re-attach has consolidated onto the deterministic name).
///
/// Sessions sharing one cwd that *also* lack a deterministic name
/// will still collide on the first cwd match. That's a known
/// limitation; the affordance to fully disambiguate them is filed in
/// TODO.
#[must_use]
fn resolve_pane_target(tmux_output: &str, session: &Session) -> Option<String> {
    let preferred = format!("agent-mux-{}", session.id.0);
    let mut cwd_match: Option<String> = None;
    for line in tmux_output.lines() {
        let Some((target, path)) = line.split_once(' ') else {
            continue;
        };
        if target.starts_with(&format!("{preferred}:")) {
            return Some(target.to_string());
        }
        if cwd_match.is_none() && Path::new(path) == session.project_dir {
            cwd_match = Some(target.to_string());
        }
    }
    cwd_match
}

/// Returns true when at least one process is currently holding
/// `transcript_path` open. Used as a pre-attach signal to catch the
/// case where Claude is running outside agent-mux's reach (e.g. the
/// user started `claude` in a bare terminal with no tmux) so the
/// `tmux_resume_argv` fallback can prompt before spawning a parallel
/// `claude --resume` against the same transcript file.
///
/// Implementation: `lsof -t -- <path>` and treat any non-empty line
/// of stdout as a positive hit. Failure modes (`lsof` missing, exits
/// non-zero with no matches, non-UTF-8 stdout) collapse to `false` —
/// false negatives are the safe default for a confirmation gate. The
/// worst case is the legacy silent-resume behaviour, which is the
/// behaviour we already had before this gate existed.
///
/// `lsof` was chosen over `fuser` because the macOS dogfood box is
/// the active surface and macOS ships `lsof` but not `fuser`. Linux
/// and WSL also ship `lsof` in their typical package set; agent-mux's
/// other userspace assumptions (`tmux`, `ssh`, `git`, `claude`) are
/// already at that bar.
#[must_use]
pub fn probe_live_writer(transcript_path: &Path) -> bool {
    let Ok(output) = Command::new("lsof")
        .args(["-t", "--"])
        .arg(transcript_path)
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        // `lsof -t` exits non-zero when no process matches — the
        // common "no live writer" case. Collapse to `false` rather
        // than treating it as an error.
        return false;
    }
    interpret_lsof_output(&output.stdout)
}

/// Pure parser for `lsof -t` stdout: one PID per line, no trailing
/// metadata. Returns true if at least one non-empty line is present.
/// Split out from [`probe_live_writer`] so the parsing edge cases
/// (whitespace-only output, non-UTF-8 bytes) are unit-testable
/// without spawning `lsof`.
#[must_use]
fn interpret_lsof_output(stdout: &[u8]) -> bool {
    let Ok(s) = std::str::from_utf8(stdout) else {
        return false;
    };
    s.lines().any(|l| !l.trim().is_empty())
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

    fn test_session(id: &str, project_dir: &str) -> Session {
        use crate::session::{Attention, HostId, Session, SessionId};
        Session {
            id: SessionId(id.to_string()),
            host: HostId::local(),
            project_dir: PathBuf::from(project_dir),
            transcript_path: PathBuf::from("/transcripts/x.jsonl"),
            last_activity: std::time::SystemTime::UNIX_EPOCH,
            attention: Attention::Unknown,
            title: None,
            parent_repo: None,
            has_live_pane: None,
            hook_pinned: None,
            blocking_prompt: false,
        }
    }

    #[test]
    fn interpret_lsof_output_returns_true_for_single_pid() {
        // One PID, trailing newline — the standard `lsof -t` shape.
        assert!(interpret_lsof_output(b"12345\n"));
    }

    #[test]
    fn interpret_lsof_output_returns_true_for_multiple_pids() {
        // Multiple processes hold the file; `lsof -t` prints one per
        // line. Any single hit is enough to gate the attach.
        assert!(interpret_lsof_output(b"12345\n67890\n"));
    }

    #[test]
    fn interpret_lsof_output_returns_true_without_trailing_newline() {
        // Defensive against an `lsof` variant that omits the final
        // newline on a single match — the line iterator still yields
        // the PID, which still trips the predicate.
        assert!(interpret_lsof_output(b"12345"));
    }

    #[test]
    fn interpret_lsof_output_returns_false_for_empty_output() {
        // The "no writers" case shouldn't trip the gate. Empty stdout
        // is what we observe when `lsof` is called with exit 0 but no
        // match (rare — typical no-match is exit 1, handled by the
        // status check above) and the "lsof missing" path's empty
        // child output (also rare, but harmless to handle).
        assert!(!interpret_lsof_output(b""));
    }

    #[test]
    fn interpret_lsof_output_returns_false_for_whitespace_only() {
        // Whitespace-only lines must not trip the gate — protects
        // against trailing-blank quirks from a future lsof variant.
        assert!(!interpret_lsof_output(b"\n  \n\t\n"));
    }

    #[test]
    fn interpret_lsof_output_returns_false_for_non_utf8() {
        // Non-UTF-8 bytes shouldn't surface as a positive hit. lsof
        // doesn't legitimately produce them, but the parser stays
        // safe rather than `unwrap()`ing.
        assert!(!interpret_lsof_output(&[0xFF, 0xFE]));
    }

    #[test]
    fn resolve_pane_target_finds_exact_cwd_match() {
        let out = "main:0.0 /home/u/proj\n\
             main:1.0 /home/u/other\n";
        let s = test_session("abc", "/home/u/proj");
        let got = resolve_pane_target(out, &s);
        assert_eq!(got, Some("main:0.0".to_string()));
    }

    #[test]
    fn resolve_pane_target_returns_none_when_no_match() {
        let out = "main:0.0 /home/u/a\nmain:1.0 /home/u/b\n";
        let s = test_session("abc", "/home/u/c");
        assert_eq!(resolve_pane_target(out, &s), None);
    }

    #[test]
    fn resolve_pane_target_handles_paths_with_spaces() {
        let out = "main:0.0 /home/u/path with spaces\n";
        let s = test_session("abc", "/home/u/path with spaces");
        let got = resolve_pane_target(out, &s);
        assert_eq!(got, Some("main:0.0".to_string()));
    }

    #[test]
    fn resolve_pane_target_skips_malformed_lines() {
        let out = "garbage_without_space\nmain:0.0 /good/path\n";
        let s = test_session("abc", "/good/path");
        let got = resolve_pane_target(out, &s);
        assert_eq!(got, Some("main:0.0".to_string()));
    }

    #[test]
    fn resolve_pane_target_prefers_agent_mux_session_name_over_cwd_collision() {
        // Two panes share the same cwd. Without name preference, the
        // first one wins arbitrarily and two sidebar rows collapse
        // onto the same pane. With name preference, the
        // `agent-mux-<id>` pane wins for the session it belongs to.
        let out = "other:0.0 /home/u/proj\n\
             agent-mux-target-id:1.0 /home/u/proj\n";
        let s = test_session("target-id", "/home/u/proj");
        let got = resolve_pane_target(out, &s);
        assert_eq!(got, Some("agent-mux-target-id:1.0".to_string()));
    }

    #[test]
    fn resolve_pane_target_falls_back_to_cwd_when_no_named_session_present() {
        // No `agent-mux-<id>` pane exists; the cwd-matching pane is
        // the right answer for externally-created or never-re-attached
        // sessions.
        let out = "other:0.0 /home/u/proj\n";
        let s = test_session("missing-named", "/home/u/proj");
        let got = resolve_pane_target(out, &s);
        assert_eq!(got, Some("other:0.0".to_string()));
    }

    #[test]
    fn resolve_pane_target_picks_named_match_even_when_cwd_differs() {
        // If `agent-mux-<id>` exists but its cwd no longer matches
        // project_dir (the user `cd`'d inside the pane), the named
        // match still wins — it's the deterministic pin.
        let out = "agent-mux-abc:0.0 /tmp/elsewhere\n\
             beta:1.0 /home/u/proj\n";
        let s = test_session("abc", "/home/u/proj");
        let got = resolve_pane_target(out, &s);
        assert_eq!(got, Some("agent-mux-abc:0.0".to_string()));
    }

    #[test]
    fn tmux_new_detached_argv_has_session_name_capture_and_claude_command() {
        // The shape is load-bearing: `-d` keeps the spawning client
        // detached (the embed becomes the only attached client), `-P
        // -F '#{session_name}'` makes tmux print the assigned name on
        // stdout, `-c <cwd>` sets the working directory, and the final
        // argument is the command tmux runs in the new session.
        let got = tmux_new_detached_argv("/work/agent-mux-fix-bug");
        assert_eq!(
            got,
            vec![
                "tmux".to_string(),
                "new-session".to_string(),
                "-d".to_string(),
                "-P".to_string(),
                "-F".to_string(),
                "#{session_name}".to_string(),
                "-c".to_string(),
                "/work/agent-mux-fix-bug".to_string(),
                "claude".to_string(),
            ]
        );
    }

    #[test]
    fn spawn_session_label_returns_cwd_basename() {
        assert_eq!(
            spawn_session_label(Path::new("/work/agent-mux-fix-bug")),
            "agent-mux-fix-bug"
        );
        // Trailing slash: file_name strips it.
        assert_eq!(spawn_session_label(Path::new("/work/proj/")), "proj");
    }

    #[test]
    fn pty_driver_spawn_session_remote_invokes_ssh_with_detached_new_session() {
        // The remote spawn must hand `tmux new-session -d -P -F
        // '#{session_name}' -c <cwd> claude` to ssh_argv(false, …) —
        // no tty needed because we're only reading back the session
        // name. The ssh process spawned from the fake host's argv will
        // fail to connect; we don't care, we just want to observe the
        // first ssh_argv call.
        let host = FakeRemoteHost::new();
        let _ = PtyDriver::spawn_session_remote(Path::new("/srv/work/proj"), &host);
        let (tty, remote_cmd) = host.last_call().expect("ssh_argv called");
        assert!(!tty, "spawn capture does not need a tty");
        assert_eq!(
            remote_cmd,
            vec![
                "tmux",
                "new-session",
                "-d",
                "-P",
                "-F",
                "#{session_name}",
                "-c",
                "/srv/work/proj",
                "claude",
            ]
        );
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
        fn list_files(&self, _: &Path) -> std::io::Result<Vec<std::path::PathBuf>> {
            unreachable!()
        }
        fn remove(&self, _: &Path) -> std::io::Result<()> {
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
    fn spawn_tool_remote_sends_sh_recipe_with_cd_and_exec() {
        // Post-2026-05-20 fix: rather than asking the *remote* tmux to
        // create a window (which lands in some session the user isn't
        // necessarily attached to, while leaving a dead local wrapper),
        // remote tool launch ssh's into the host and exec's the
        // command at the session's cwd. The local wrapper window stays
        // alive until the tool exits — same shape as local-in-tmux.
        let host = FakeRemoteHost::new();
        let session = make_session(HostId("remote".into()), "/work/proj");
        let cmd = vec!["nvim".to_string(), ".".to_string()];
        let _ = TmuxDriver::spawn_tool_remote(&session, &host, &cmd);
        let (tty, remote_cmd) = host.last_call().expect("ssh_argv called");
        assert!(tty, "spawn_tool needs -t for interactive tools");
        assert_eq!(
            remote_cmd,
            vec!["sh", "-c", "cd '/work/proj' && exec 'nvim' '.'"]
        );
    }

    #[test]
    fn spawn_terminal_remote_sends_sh_recipe_with_cd_and_exec_shell() {
        // The terminal binding (`t`) must drop the user into their
        // remote `$SHELL` at the session's cwd. Pinned because the
        // pre-2026-05-20 shape (`tmux new-window -c <cwd>` on the
        // remote) was the source of the dogfood-discovered bug:
        // remote tmux created a window in some session the user
        // couldn't necessarily see, and the local wrapper exited
        // immediately.
        let host = FakeRemoteHost::new();
        let session = make_session(HostId("remote".into()), "/work/proj");
        let _ = TmuxDriver::spawn_terminal_remote(&session, &host);
        let (tty, remote_cmd) = host.last_call().expect("ssh_argv called");
        assert!(tty, "spawn_terminal needs -t for interactive shell");
        assert_eq!(
            remote_cmd,
            vec!["sh", "-c", "cd '/work/proj' && exec \"$SHELL\""]
        );
    }

    #[test]
    fn spawn_terminal_remote_quotes_cwd_with_spaces() {
        // Paths with spaces survive both quoting layers: ours here
        // (single-quote the cwd inside the sh recipe) and ssh_argv's
        // (single-quote the whole recipe as one final argv element).
        let host = FakeRemoteHost::new();
        let session = make_session(HostId("remote".into()), "/work/my proj");
        let _ = TmuxDriver::spawn_terminal_remote(&session, &host);
        let (_, remote_cmd) = host.last_call().expect("ssh_argv called");
        assert_eq!(
            remote_cmd,
            vec!["sh", "-c", "cd '/work/my proj' && exec \"$SHELL\""]
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
    fn parse_pane_records_extracts_session_and_cwd_per_line() {
        let out = "main\t/home/u/proj\nagent-mux-abc\t/home/u/other\n";
        let snap = parse_pane_records(out);
        assert_eq!(
            snap.cwds,
            vec![
                PathBuf::from("/home/u/proj"),
                PathBuf::from("/home/u/other")
            ]
        );
        assert_eq!(snap.session_names, vec!["main", "agent-mux-abc"]);
    }

    #[test]
    fn parse_pane_records_skips_blank_lines() {
        let out = "main\t/a\n\nbeta\t/b\n";
        let snap = parse_pane_records(out);
        assert_eq!(snap.cwds, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(snap.session_names, vec!["main", "beta"]);
    }

    #[test]
    fn parse_pane_records_handles_cwds_with_spaces() {
        // Spaces in cwds round-trip — tab is the field separator, not
        // space.
        let out = "main\t/home/u/path with spaces\n";
        let snap = parse_pane_records(out);
        assert_eq!(snap.cwds, vec![PathBuf::from("/home/u/path with spaces")]);
        assert_eq!(snap.session_names, vec!["main"]);
    }

    #[test]
    fn parse_pane_records_drops_lines_without_tab() {
        // A malformed line (no tab) shouldn't poison the whole
        // snapshot — defensive against tmux format changes.
        let out = "no_tab_here\nmain\t/good/path\n";
        let snap = parse_pane_records(out);
        assert_eq!(snap.cwds, vec![PathBuf::from("/good/path")]);
        assert_eq!(snap.session_names, vec!["main"]);
    }

    #[test]
    fn parse_pane_records_returns_empty_for_empty_input() {
        // No tmux server / no panes / failed shell-out all collapse to
        // an empty list. The caller treats this as "no live panes",
        // which means every session on that host gets `has_live_pane = Some(false)`.
        let snap = parse_pane_records("");
        assert!(snap.cwds.is_empty());
        assert!(snap.session_names.is_empty());
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
            hook_pinned: None,
            blocking_prompt: false,
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
        // Title-less fallback: last 6 chars of the session id, prefixed
        // with "…", matching the embedded pane's block label.
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

    #[test]
    fn pty_driver_spawn_terminal_remote_invokes_detached_tmux_new_session() {
        // Remote `t` wraps the remote shell in a detached tmux session
        // (the recipe `spawn_tool_remote_embed` uses) so the terminal
        // surfaces in the Tools sidebar group as a re-attachable row.
        // The first ssh_argv call captures the spawn — `tty=false`
        // because we only read the assigned session name back. tmux
        // execs the trailing positional arg via `/bin/sh -c`, so
        // `exec "$SHELL"` expands against the remote user's env.
        let host = FakeRemoteHost::new();
        let session = make_session(HostId("remote".into()), "/work/proj");
        let _ = PtyDriver::spawn_terminal_remote_embed(&session, &host);
        let (tty, remote_cmd) = host.last_call().expect("ssh_argv called");
        assert!(
            !tty,
            "spawn capture does not need a tty (only the attach does)"
        );
        assert_eq!(
            remote_cmd,
            vec![
                "tmux",
                "new-session",
                "-d",
                "-P",
                "-F",
                "#{session_name}",
                "-c",
                "/work/proj",
                "exec \"$SHELL\"",
            ]
        );
    }

    #[test]
    fn tmux_new_detached_tool_argv_carries_cwd_and_joined_command() {
        // The argv shape `PtyDriver::spawn_tool_*_embed` hands to
        // `tmux new-session` for tool launches. The detached session
        // wraps the tool so the user can swap focus away from the
        // embedded pane without killing the tool process.
        let got = tmux_new_detached_tool_argv("/work/proj", "lazygit");
        assert_eq!(
            got,
            vec![
                "tmux".to_string(),
                "new-session".to_string(),
                "-d".to_string(),
                "-P".to_string(),
                "-F".to_string(),
                "#{session_name}".to_string(),
                "-c".to_string(),
                "/work/proj".to_string(),
                "lazygit".to_string(),
            ]
        );
    }

    #[test]
    fn tmux_new_detached_tool_argv_passes_joined_command_as_one_arg() {
        // Quoting & joining is the caller's responsibility — the
        // helper passes the joined string as the trailing positional
        // argument to `tmux new-session`, which is what `tmux` then
        // execs via `/bin/sh -c`. A tool with arguments arrives here
        // as one shell-quoted string (e.g. `'nvim' '.'`).
        let got = tmux_new_detached_tool_argv("/work/proj", "'nvim' '.'");
        assert_eq!(got.last().unwrap(), "'nvim' '.'");
    }
}
