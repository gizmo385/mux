use std::path::{Path, PathBuf};
use std::process::Command;

use crate::session::Session;

#[derive(Debug)]
pub enum AttachError {
    NotFound,
    TmuxCommandFailed(String),
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "no tmux pane found in the session's cwd"),
            Self::TmuxCommandFailed(msg) => write!(f, "tmux: {msg}"),
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
    /// or `AttachError::TmuxCommandFailed` if tmux itself returns non-zero.
    fn attach(&self, session: &Session) -> Result<AttachOutcome, AttachError>;

    /// Open a fresh terminal in the session's working directory.
    ///
    /// # Errors
    /// Returns `AttachError::TmuxCommandFailed` if tmux returns non-zero.
    /// (No error when running outside tmux — that path drops into `$SHELL`
    /// without consulting tmux at all.)
    fn spawn_terminal(&self, session: &Session) -> Result<AttachOutcome, AttachError>;

    /// Launch a fresh `claude` process in a new tmux window at `cwd`, and
    /// switch focus to it. Used by the new-session flow after the
    /// `WorktreeManager` has created the worktree.
    ///
    /// The resulting Claude Code session will surface in the dashboard
    /// shortly after via the existing transcript-discovery pipeline — this
    /// method does not return a `SessionId`.
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
    fn attach(&self, session: &Session) -> Result<AttachOutcome, AttachError> {
        if let Ok(target) = find_pane(&session.project_dir) {
            return Self::switch_to_live(&target);
        }
        // No live pane (either tmux says NotFound, or there's no tmux server
        // at all and list-panes failed). Resume the transcript in a fresh
        // claude process, in the session's recorded cwd.
        let cmd = format!("claude --resume {}", session.id.0);
        Self::launch_in_new_window(&session.project_dir, &cmd)
    }

    fn spawn_terminal(&self, session: &Session) -> Result<AttachOutcome, AttachError> {
        Self::spawn_terminal_impl(session)
    }

    fn spawn_session(&self, cwd: &Path) -> Result<AttachOutcome, AttachError> {
        Self::launch_in_new_window(cwd, "claude")
    }
}

impl TmuxDriver {
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

    fn spawn_terminal_impl(session: &Session) -> Result<AttachOutcome, AttachError> {
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

fn find_pane(project_dir: &Path) -> Result<String, AttachError> {
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
}
