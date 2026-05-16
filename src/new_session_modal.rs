use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::repo::Repo;
use crate::worktree;

pub enum NewSessionModal {
    PickingRepo {
        repos: Vec<Repo>,
        state: ListState,
    },
    Filling {
        repo: Repo,
        task: String,
        branch: String,
        focus: FillFocus,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillFocus {
    Task,
    Branch,
}

pub enum KeyOutcome {
    /// Modal handled the key. Caller does nothing.
    Handled,
    /// User pressed Esc. Caller drops the modal.
    Cancel,
    /// User submitted a valid form. Caller dispatches the action.
    Submit {
        repo: Repo,
        task: String,
        base_branch: String,
    },
}

impl NewSessionModal {
    /// Open a new modal at the repo-picker stage. Caller must pass a
    /// non-empty list of repos.
    #[must_use]
    pub fn new(repos: Vec<Repo>) -> Self {
        debug_assert!(!repos.is_empty(), "modal opened with no repos");
        let mut state = ListState::default();
        state.select(Some(0));
        Self::PickingRepo { repos, state }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> KeyOutcome {
        if key.code == KeyCode::Esc {
            return KeyOutcome::Cancel;
        }
        // PickingRepo may need to transition self into Filling on Enter, so
        // it returns the new state separately rather than mutating self
        // through the destructure (which would double-borrow).
        if let Self::PickingRepo { repos, state } = self {
            let (outcome, next) = handle_picking(repos, state, key);
            if let Some(next) = next {
                *self = next;
            }
            return outcome;
        }
        if let Self::Filling {
            repo,
            task,
            branch,
            focus,
        } = self
        {
            return handle_filling(repo, task, branch, focus, key);
        }
        KeyOutcome::Handled
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>) {
        let area = centered_rect(60, 60, frame.area());
        frame.render_widget(Clear, area);
        match self {
            Self::PickingRepo { repos, state } => draw_picking(frame, area, repos, state),
            Self::Filling {
                repo,
                task,
                branch,
                focus,
            } => draw_filling(frame, area, repo, task, branch, *focus),
        }
    }
}

fn handle_picking(
    repos: &mut [Repo],
    state: &mut ListState,
    key: KeyEvent,
) -> (KeyOutcome, Option<NewSessionModal>) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            select_prev(state, repos.len());
            (KeyOutcome::Handled, None)
        }
        KeyCode::Down | KeyCode::Char('j') => {
            select_next(state, repos.len());
            (KeyOutcome::Handled, None)
        }
        KeyCode::Enter => {
            let Some(idx) = state.selected() else {
                return (KeyOutcome::Handled, None);
            };
            let Some(repo) = repos.get(idx).cloned() else {
                return (KeyOutcome::Handled, None);
            };
            let branch = worktree::resolve_default_base_branch(&repo.path).unwrap_or_default();
            let next = NewSessionModal::Filling {
                repo,
                task: String::new(),
                branch,
                focus: FillFocus::Task,
            };
            (KeyOutcome::Handled, Some(next))
        }
        _ => (KeyOutcome::Handled, None),
    }
}

fn handle_filling(
    repo: &Repo,
    task: &mut String,
    branch: &mut String,
    focus: &mut FillFocus,
    key: KeyEvent,
) -> KeyOutcome {
    match key.code {
        KeyCode::Tab | KeyCode::BackTab => {
            *focus = match *focus {
                FillFocus::Task => FillFocus::Branch,
                FillFocus::Branch => FillFocus::Task,
            };
            KeyOutcome::Handled
        }
        KeyCode::Enter => {
            if task.trim().is_empty() {
                *focus = FillFocus::Task;
                return KeyOutcome::Handled;
            }
            if branch.trim().is_empty() {
                *focus = FillFocus::Branch;
                return KeyOutcome::Handled;
            }
            KeyOutcome::Submit {
                repo: repo.clone(),
                task: task.clone(),
                base_branch: branch.clone(),
            }
        }
        KeyCode::Backspace => {
            match focus {
                FillFocus::Task => {
                    task.pop();
                }
                FillFocus::Branch => {
                    branch.pop();
                }
            }
            KeyOutcome::Handled
        }
        KeyCode::Char(c) => {
            match focus {
                FillFocus::Task => task.push(c),
                FillFocus::Branch => branch.push(c),
            }
            KeyOutcome::Handled
        }
        _ => KeyOutcome::Handled,
    }
}

fn select_next(state: &mut ListState, len: usize) {
    if len == 0 {
        return;
    }
    let i = state.selected().unwrap_or(0);
    state.select(Some((i + 1) % len));
}

fn select_prev(state: &mut ListState, len: usize) {
    if len == 0 {
        return;
    }
    let i = state.selected().unwrap_or(0);
    let prev = if i == 0 { len - 1 } else { i - 1 };
    state.select(Some(prev));
}

fn centered_rect(width_pct: u16, height_pct: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - height_pct) / 2),
        Constraint::Percentage(height_pct),
        Constraint::Percentage((100 - height_pct) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - width_pct) / 2),
        Constraint::Percentage(width_pct),
        Constraint::Percentage((100 - width_pct) / 2),
    ])
    .split(vertical[1])[1]
}

fn draw_picking(frame: &mut Frame<'_>, area: Rect, repos: &[Repo], state: &mut ListState) {
    let layout = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);
    let items: Vec<ListItem<'_>> = repos
        .iter()
        .map(|r| ListItem::new(format!("{}  {}", r.name, r.path.display())))
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" new session: pick a repo "),
        )
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▌ ");
    frame.render_stateful_widget(list, layout[0], state);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ↑/↓ or j/k: move · ⏎: select · Esc: cancel ",
            Style::new().add_modifier(Modifier::DIM),
        ))),
        layout[1],
    );
}

fn draw_filling(
    frame: &mut Frame<'_>,
    area: Rect,
    repo: &Repo,
    task: &str,
    branch: &str,
    focus: FillFocus,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" new session in {} ", repo.name));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let layout = Layout::vertical([
        Constraint::Length(1), // task label
        Constraint::Length(1), // task input
        Constraint::Length(1), // blank
        Constraint::Length(1), // branch label
        Constraint::Length(1), // branch input
        Constraint::Min(0),    // spacer
        Constraint::Length(1), // hint
    ])
    .split(inner);

    let dim = Style::new().add_modifier(Modifier::DIM);
    let focused = Style::new().add_modifier(Modifier::REVERSED);

    frame.render_widget(Paragraph::new(Span::styled("task:", dim)), layout[0]);
    let task_style = if focus == FillFocus::Task {
        focused
    } else {
        Style::default()
    };
    frame.render_widget(
        Paragraph::new(input_line(task, focus == FillFocus::Task)).style(task_style),
        layout[1],
    );

    frame.render_widget(Paragraph::new(Span::styled("base branch:", dim)), layout[3]);
    let branch_style = if focus == FillFocus::Branch {
        focused
    } else {
        Style::default()
    };
    frame.render_widget(
        Paragraph::new(input_line(branch, focus == FillFocus::Branch)).style(branch_style),
        layout[4],
    );

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Tab: switch field · ⏎: submit · Esc: cancel",
            dim,
        ))),
        layout[6],
    );
}

impl std::fmt::Debug for KeyOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Handled => write!(f, "Handled"),
            Self::Cancel => write!(f, "Cancel"),
            Self::Submit {
                repo,
                task,
                base_branch,
            } => f
                .debug_struct("Submit")
                .field("repo", &repo.name)
                .field("task", task)
                .field("base_branch", base_branch)
                .finish(),
        }
    }
}

fn input_line(value: &str, focused: bool) -> Line<'static> {
    // A trailing cursor block on the focused field gives the user a visible
    // edit point even though ratatui doesn't draw a real cursor for us.
    let mut s = value.to_string();
    if focused {
        s.push('▏');
    }
    if value.is_empty() && !focused {
        Line::from(Span::raw(" "))
    } else {
        Line::from(Span::raw(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use std::path::PathBuf;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn repo(name: &str) -> Repo {
        Repo {
            path: PathBuf::from(format!("/tmp/{name}")),
            name: name.to_string(),
        }
    }

    #[test]
    fn new_starts_in_picker_with_first_selected() {
        let modal = NewSessionModal::new(vec![repo("a"), repo("b")]);
        match modal {
            NewSessionModal::PickingRepo { state, .. } => {
                assert_eq!(state.selected(), Some(0));
            }
            NewSessionModal::Filling { .. } => panic!("expected PickingRepo"),
        }
    }

    #[test]
    fn esc_returns_cancel_from_any_state() {
        let mut modal = NewSessionModal::new(vec![repo("a")]);
        assert!(matches!(
            modal.handle_key(key(KeyCode::Esc)),
            KeyOutcome::Cancel
        ));

        modal = NewSessionModal::Filling {
            repo: repo("a"),
            task: "x".to_string(),
            branch: "main".to_string(),
            focus: FillFocus::Task,
        };
        assert!(matches!(
            modal.handle_key(key(KeyCode::Esc)),
            KeyOutcome::Cancel
        ));
    }

    #[test]
    fn picker_navigation_wraps() {
        let mut modal = NewSessionModal::new(vec![repo("a"), repo("b"), repo("c")]);
        modal.handle_key(key(KeyCode::Up));
        match &modal {
            NewSessionModal::PickingRepo { state, .. } => {
                assert_eq!(state.selected(), Some(2));
            }
            NewSessionModal::Filling { .. } => panic!("expected PickingRepo"),
        }
        modal.handle_key(key(KeyCode::Down));
        match &modal {
            NewSessionModal::PickingRepo { state, .. } => {
                assert_eq!(state.selected(), Some(0));
            }
            NewSessionModal::Filling { .. } => panic!("expected PickingRepo"),
        }
    }

    #[test]
    fn picker_enter_transitions_to_filling_with_selected_repo() {
        let mut modal = NewSessionModal::new(vec![repo("a"), repo("b")]);
        modal.handle_key(key(KeyCode::Down));
        let outcome = modal.handle_key(key(KeyCode::Enter));
        assert!(matches!(outcome, KeyOutcome::Handled));
        match &modal {
            NewSessionModal::Filling { repo, focus, .. } => {
                assert_eq!(repo.name, "b");
                assert_eq!(*focus, FillFocus::Task);
            }
            NewSessionModal::PickingRepo { .. } => panic!("expected Filling"),
        }
    }

    #[test]
    fn filling_typing_appends_to_focused_field() {
        let mut modal = NewSessionModal::Filling {
            repo: repo("a"),
            task: String::new(),
            branch: String::new(),
            focus: FillFocus::Task,
        };
        for c in "hi".chars() {
            modal.handle_key(key(KeyCode::Char(c)));
        }
        modal.handle_key(key(KeyCode::Tab));
        for c in "main".chars() {
            modal.handle_key(key(KeyCode::Char(c)));
        }
        match &modal {
            NewSessionModal::Filling {
                task,
                branch,
                focus,
                ..
            } => {
                assert_eq!(task, "hi");
                assert_eq!(branch, "main");
                assert_eq!(*focus, FillFocus::Branch);
            }
            NewSessionModal::PickingRepo { .. } => panic!("expected Filling"),
        }
    }

    #[test]
    fn filling_backspace_pops_focused_field() {
        let mut modal = NewSessionModal::Filling {
            repo: repo("a"),
            task: "abc".to_string(),
            branch: "main".to_string(),
            focus: FillFocus::Task,
        };
        modal.handle_key(key(KeyCode::Backspace));
        match &modal {
            NewSessionModal::Filling { task, branch, .. } => {
                assert_eq!(task, "ab");
                assert_eq!(branch, "main");
            }
            NewSessionModal::PickingRepo { .. } => panic!("expected Filling"),
        }
    }

    #[test]
    fn filling_enter_with_empty_task_does_not_submit() {
        let mut modal = NewSessionModal::Filling {
            repo: repo("a"),
            task: String::new(),
            branch: "main".to_string(),
            focus: FillFocus::Branch,
        };
        let outcome = modal.handle_key(key(KeyCode::Enter));
        assert!(matches!(outcome, KeyOutcome::Handled));
        // Focus snaps back to the offending field.
        match &modal {
            NewSessionModal::Filling { focus, .. } => assert_eq!(*focus, FillFocus::Task),
            NewSessionModal::PickingRepo { .. } => panic!("expected Filling"),
        }
    }

    #[test]
    fn filling_enter_with_empty_branch_does_not_submit() {
        let mut modal = NewSessionModal::Filling {
            repo: repo("a"),
            task: "task".to_string(),
            branch: String::new(),
            focus: FillFocus::Task,
        };
        let outcome = modal.handle_key(key(KeyCode::Enter));
        assert!(matches!(outcome, KeyOutcome::Handled));
        match &modal {
            NewSessionModal::Filling { focus, .. } => assert_eq!(*focus, FillFocus::Branch),
            NewSessionModal::PickingRepo { .. } => panic!("expected Filling"),
        }
    }

    #[test]
    fn filling_enter_with_valid_form_returns_submit() {
        let mut modal = NewSessionModal::Filling {
            repo: repo("agent-mux"),
            task: "refactor parser".to_string(),
            branch: "main".to_string(),
            focus: FillFocus::Task,
        };
        match modal.handle_key(key(KeyCode::Enter)) {
            KeyOutcome::Submit {
                repo,
                task,
                base_branch,
            } => {
                assert_eq!(repo.name, "agent-mux");
                assert_eq!(task, "refactor parser");
                assert_eq!(base_branch, "main");
            }
            other => panic!("expected Submit, got {other:?}"),
        }
    }
}
