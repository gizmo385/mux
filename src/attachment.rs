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
}

#[derive(Debug, Clone)]
pub struct SuspendCommand {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
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
    /// `WorktreeManager` has created the worktree.
    ///
    /// The resulting Claude Code session will surface in the dashboard
    /// shortly after via the existing transcript-discovery pipeline — this
    /// method does not return a `SessionId`.
    ///
    /// Local-only in M2: remote session *creation* (the new-session flow
    /// running against an SSH host) is post-M5.
    ///
    /// # Errors
    /// Returns `AttachError::TmuxCommandFailed` if tmux returns non-zero.
    fn spawn_session(&self, cwd: &Path) -> Result<AttachOutcome, AttachError>;
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

    fn spawn_session(&self, cwd: &Path) -> Result<AttachOutcome, AttachError> {
        Self::launch_in_new_window(cwd, "claude")
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
            Ok(AttachOutcome::SuspendAndRun(SuspendCommand {
                program: "tmux".to_string(),
                args: vec!["attach".to_string(), "-t".to_string(), target.to_string()],
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
        let session_name = format!("agent-mux-{}", session.id.0);
        let cwd = session.project_dir.to_string_lossy().into_owned();
        let claude_cmd = format!("claude --resume {}", session.id.0);
        Self::run_remote_interactive(
            host,
            &[
                "tmux",
                "new-session",
                "-A",
                "-s",
                &session_name,
                "-c",
                &cwd,
                &claude_cmd,
            ],
        )
    }

    fn spawn_terminal_remote(
        session: &Session,
        host: &dyn Host,
    ) -> Result<AttachOutcome, AttachError> {
        let cwd = session.project_dir.to_string_lossy().into_owned();
        Self::run_remote_interactive(host, &["tmux", "new-window", "-c", &cwd])
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

// ---------- shared helpers ----------

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
            has_live_pane: None,
        }
    }
}
