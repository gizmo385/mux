use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::session::{HostId, Session, SessionId};

/// Single-state confirmation modal driving the `d: delete` action.
///
/// Two-axis decision: the user confirms (Enter) or cancels (Esc), and
/// independently toggles whether the eventual `git worktree remove` call
/// should pass `--force`. The default is `force = false` so a stray
/// keystroke can't blow away uncommitted work; the user opts in
/// explicitly with `f` after reading the modal copy.
pub struct DeleteWorktreeModal {
    session_id: SessionId,
    host_id: HostId,
    parent_repo: PathBuf,
    worktree_path: PathBuf,
    /// Display label assembled by the caller — typically the session's
    /// title, falling back to a short id suffix. Used only for render;
    /// not part of the submit payload.
    label: String,
    force: bool,
}

pub enum KeyOutcome {
    Handled,
    Cancel,
    Submit {
        session_id: SessionId,
        host_id: HostId,
        parent_repo: PathBuf,
        worktree_path: PathBuf,
        force: bool,
    },
}

impl DeleteWorktreeModal {
    /// Construct a modal from the selected session. Returns `None` if
    /// the session has no `parent_repo` — sessions started outside a
    /// worktree (a plain checkout, or an arbitrary `claude` invocation
    /// against any directory) aren't deletable through this path, and
    /// the caller surfaces a status message instead.
    #[must_use]
    pub fn for_session(session: &Session, label: String) -> Option<Self> {
        let parent_repo = session.parent_repo.clone()?;
        Some(Self {
            session_id: session.id.clone(),
            host_id: session.host.clone(),
            parent_repo,
            worktree_path: session.project_dir.clone(),
            label,
            force: false,
        })
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> KeyOutcome {
        match key.code {
            KeyCode::Esc => KeyOutcome::Cancel,
            KeyCode::Enter => KeyOutcome::Submit {
                session_id: self.session_id.clone(),
                host_id: self.host_id.clone(),
                parent_repo: self.parent_repo.clone(),
                worktree_path: self.worktree_path.clone(),
                force: self.force,
            },
            // `f` (and `F`) toggles the force flag. The render path
            // reflects the current state so the user always sees what
            // their next Enter will do — no hidden mode.
            KeyCode::Char('f' | 'F') => {
                self.force = !self.force;
                KeyOutcome::Handled
            }
            _ => KeyOutcome::Handled,
        }
    }

    pub fn draw(&self, frame: &mut Frame<'_>) {
        let area = centered_rect(70, 40, frame.area());
        frame.render_widget(Clear, area);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(" delete worktree ");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let layout = Layout::vertical([
            Constraint::Length(1), // label
            Constraint::Length(1), // path
            Constraint::Length(1), // host
            Constraint::Length(1), // blank
            Constraint::Length(2), // force toggle + warning
            Constraint::Min(0),    // spacer
            Constraint::Length(1), // help line
        ])
        .split(inner);

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw("task: "),
                Span::styled(&self.label, Style::new().add_modifier(Modifier::BOLD)),
            ])),
            layout[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw("path: "),
                Span::styled(
                    self.worktree_path.to_string_lossy().into_owned(),
                    Style::new().add_modifier(Modifier::DIM),
                ),
            ]))
            .wrap(Wrap { trim: false }),
            layout[1],
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw("host: "),
                Span::styled(
                    self.host_id.as_str().to_string(),
                    Style::new().add_modifier(Modifier::DIM),
                ),
            ])),
            layout[2],
        );

        let (marker, force_text, force_style) = if self.force {
            (
                "[x]",
                "force: skip the dirty-worktree safety check",
                Style::new().add_modifier(Modifier::BOLD),
            )
        } else {
            (
                "[ ]",
                "force: off (git refuses if the worktree has uncommitted changes)",
                Style::new().add_modifier(Modifier::DIM),
            )
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(marker, force_style),
                    Span::raw(" "),
                    Span::styled(force_text, force_style),
                ]),
                Line::from(Span::styled(
                    "branch is left alone — clean up with `git branch -d` later if you want",
                    Style::new().add_modifier(Modifier::DIM),
                )),
            ])
            .wrap(Wrap { trim: false }),
            layout[4],
        );

        frame.render_widget(
            Paragraph::new(Span::styled(
                "⏎ confirm · f force toggle · esc cancel",
                Style::new().add_modifier(Modifier::DIM),
            )),
            layout[6],
        );
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(r);
    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Attention, HostId, Session, SessionId};
    use std::path::PathBuf;
    use std::time::SystemTime;

    fn worktree_session() -> Session {
        Session {
            id: SessionId("abc123".into()),
            host: HostId::local(),
            project_dir: PathBuf::from("/work/.agent-mux-worktrees/proj-feature"),
            transcript_path: PathBuf::from("/transcripts/abc.jsonl"),
            last_activity: SystemTime::UNIX_EPOCH,
            attention: Attention::Idle,
            title: Some("refactor parser".into()),
            parent_repo: Some(PathBuf::from("/work/proj")),
            has_live_pane: None,
            hook_pinned: None,
            blocking_prompt: false,
            attention_entered_at: None,
            started_at: None,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    #[test]
    fn for_session_returns_none_without_parent_repo() {
        // A session whose `project_dir` isn't a git worktree (plain
        // checkout, arbitrary directory) can't be deleted through this
        // path. The dashboard surfaces a status message instead of
        // opening a modal it can never submit usefully.
        let mut s = worktree_session();
        s.parent_repo = None;
        assert!(DeleteWorktreeModal::for_session(&s, "irrelevant".into()).is_none());
    }

    #[test]
    fn for_session_constructs_with_force_off_by_default() {
        let s = worktree_session();
        let modal =
            DeleteWorktreeModal::for_session(&s, "refactor parser".into()).expect("constructs");
        assert!(!modal.force, "force should default to off — opt-in only");
    }

    #[test]
    fn esc_cancels() {
        let s = worktree_session();
        let mut modal = DeleteWorktreeModal::for_session(&s, "x".into()).unwrap();
        assert!(matches!(
            modal.handle_key(key(KeyCode::Esc)),
            KeyOutcome::Cancel
        ));
    }

    #[test]
    fn enter_submits_with_current_force_state() {
        // Submitting with force off carries `force: false` — the worktree
        // manager will refuse on uncommitted changes and the dashboard
        // surfaces the error so the user re-runs with force on.
        let s = worktree_session();
        let mut modal = DeleteWorktreeModal::for_session(&s, "x".into()).unwrap();
        match modal.handle_key(key(KeyCode::Enter)) {
            KeyOutcome::Submit {
                session_id,
                force,
                parent_repo,
                worktree_path,
                ..
            } => {
                assert_eq!(session_id.0, "abc123");
                assert!(!force);
                assert_eq!(parent_repo, PathBuf::from("/work/proj"));
                assert_eq!(
                    worktree_path,
                    PathBuf::from("/work/.agent-mux-worktrees/proj-feature")
                );
            }
            other => panic!("expected Submit, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn f_toggles_force_and_subsequent_submit_carries_it() {
        let s = worktree_session();
        let mut modal = DeleteWorktreeModal::for_session(&s, "x".into()).unwrap();
        assert!(matches!(
            modal.handle_key(key(KeyCode::Char('f'))),
            KeyOutcome::Handled
        ));
        match modal.handle_key(key(KeyCode::Enter)) {
            KeyOutcome::Submit { force, .. } => assert!(force),
            _ => panic!("expected Submit with force=true"),
        }
    }

    #[test]
    fn f_toggle_is_idempotent_off_again() {
        // Two presses of `f` returns to the safe default — important
        // because the user's escape hatch from an accidental force-on
        // is to press `f` again before Enter, not Esc + reopen.
        let s = worktree_session();
        let mut modal = DeleteWorktreeModal::for_session(&s, "x".into()).unwrap();
        modal.handle_key(key(KeyCode::Char('f')));
        modal.handle_key(key(KeyCode::Char('f')));
        match modal.handle_key(key(KeyCode::Enter)) {
            KeyOutcome::Submit { force, .. } => assert!(!force),
            _ => panic!("expected Submit"),
        }
    }
}
