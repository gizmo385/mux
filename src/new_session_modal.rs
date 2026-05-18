use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::repo::Repo;
use crate::session::HostId;
use crate::worktree;

pub enum NewSessionModal {
    PickingRepo {
        repos: Vec<Repo>,
        /// Hosts whose `Arc<dyn Host>` is currently registered in
        /// `App.hosts`. Repos belonging to other hosts render dimmed
        /// and are skipped on Enter — matches the M2 attach UX where
        /// cached rows are visible-but-inert until their host is up.
        ready_hosts: HashSet<HostId>,
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
    /// non-empty list of repos and the set of hosts whose `Arc<dyn Host>`
    /// is currently live in `App.hosts`. Repos belonging to other
    /// hosts are visible-but-inert (rendered dim, ignored on Enter).
    #[must_use]
    pub fn new(repos: Vec<Repo>, ready_hosts: HashSet<HostId>) -> Self {
        debug_assert!(!repos.is_empty(), "modal opened with no repos");
        let mut state = ListState::default();
        state.select(Some(0));
        Self::PickingRepo {
            repos,
            ready_hosts,
            state,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> KeyOutcome {
        if key.code == KeyCode::Esc {
            return KeyOutcome::Cancel;
        }
        // PickingRepo may need to transition self into Filling on Enter, so
        // it returns the new state separately rather than mutating self
        // through the destructure (which would double-borrow).
        if let Self::PickingRepo {
            repos,
            ready_hosts,
            state,
        } = self
        {
            let (outcome, next) = handle_picking(repos, ready_hosts, state, key);
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
            Self::PickingRepo {
                repos,
                ready_hosts,
                state,
            } => draw_picking(frame, area, repos, ready_hosts, state),
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
    ready_hosts: &HashSet<HostId>,
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
            // A repo whose host hasn't reached `Connected` yet is
            // inert — we can't run `git worktree add` on it, so the
            // picker silently no-ops rather than transitioning into
            // a form the user can't submit. Once the host connects,
            // the row un-dims and Enter works.
            if !ready_hosts.contains(&repo.host) {
                return (KeyOutcome::Handled, None);
            }
            // Default-branch resolution is a local-only fast path
            // today; for remote repos we leave the field blank and
            // let the user type it. Step 3 routes this through
            // `Host::run` so remote default-branch resolution works
            // too.
            let branch = if repo.host.is_local() {
                worktree::resolve_default_base_branch(&repo.path).unwrap_or_default()
            } else {
                String::new()
            };
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

fn draw_picking(
    frame: &mut Frame<'_>,
    area: Rect,
    repos: &[Repo],
    ready_hosts: &HashSet<HostId>,
    state: &mut ListState,
) {
    let layout = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);
    let dim = Style::new().add_modifier(Modifier::DIM);
    let items: Vec<ListItem<'_>> = repos
        .iter()
        .map(|r| {
            let ready = ready_hosts.contains(&r.host);
            // Host label as a leading bracket so the user can scan the
            // column at a glance. Local repos still show `[local]` so
            // mixed-host workspaces don't have ambiguous unlabelled
            // rows.
            let row = format!("[{}] {}  {}", r.host.as_str(), r.name, r.path.display());
            let line = if ready {
                Line::from(Span::raw(row))
            } else {
                // Trailing marker explains *why* the row is dim — a
                // user who sees the host name in `connect failed:`
                // can correlate. The full line is dimmed too so the
                // visual weight matches the inert state.
                Line::from(Span::styled(format!("{row}  (host not ready)"), dim))
            };
            ListItem::new(line)
        })
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
            dim,
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

    fn local_repo(name: &str) -> Repo {
        Repo {
            host: HostId::local(),
            path: PathBuf::from(format!("/tmp/{name}")),
            name: name.to_string(),
        }
    }

    fn remote_repo(host: &str, name: &str) -> Repo {
        Repo {
            host: HostId(host.into()),
            path: PathBuf::from(format!("/srv/{name}")),
            name: name.to_string(),
        }
    }

    fn ready_local() -> HashSet<HostId> {
        let mut s = HashSet::new();
        s.insert(HostId::local());
        s
    }

    #[test]
    fn new_starts_in_picker_with_first_selected() {
        let modal = NewSessionModal::new(vec![local_repo("a"), local_repo("b")], ready_local());
        match modal {
            NewSessionModal::PickingRepo { state, .. } => {
                assert_eq!(state.selected(), Some(0));
            }
            NewSessionModal::Filling { .. } => panic!("expected PickingRepo"),
        }
    }

    #[test]
    fn esc_returns_cancel_from_any_state() {
        let mut modal = NewSessionModal::new(vec![local_repo("a")], ready_local());
        assert!(matches!(
            modal.handle_key(key(KeyCode::Esc)),
            KeyOutcome::Cancel
        ));

        modal = NewSessionModal::Filling {
            repo: local_repo("a"),
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
        let mut modal = NewSessionModal::new(
            vec![local_repo("a"), local_repo("b"), local_repo("c")],
            ready_local(),
        );
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
        let mut modal = NewSessionModal::new(vec![local_repo("a"), local_repo("b")], ready_local());
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
    fn picker_enter_on_non_ready_host_is_inert() {
        // The decision-2 behaviour: cached repos whose host hasn't
        // reached `Connected` are visible-but-inert in the picker —
        // selecting them no-ops rather than transitioning into a
        // Filling state the user couldn't submit from. Pin this so
        // future refactors of the host registration don't silently
        // re-enable broken submits.
        let mut ready = HashSet::new();
        ready.insert(HostId::local());
        let mut modal = NewSessionModal::new(
            vec![local_repo("here"), remote_repo("gizmo", "there")],
            ready,
        );
        // Move to the remote repo.
        modal.handle_key(key(KeyCode::Down));
        let outcome = modal.handle_key(key(KeyCode::Enter));
        assert!(matches!(outcome, KeyOutcome::Handled));
        // Stays in PickingRepo — no transition into Filling.
        assert!(matches!(modal, NewSessionModal::PickingRepo { .. }));
    }

    #[test]
    fn picker_enter_on_ready_remote_host_transitions_to_filling_with_blank_branch() {
        // Remote repos transition into Filling once their host is
        // ready, but with an empty branch — the local default-branch
        // resolver doesn't apply, so the user is asked to type it.
        // Step 3 routes that resolution through `Host::run` so
        // remote repos pre-fill the same way.
        let mut ready = HashSet::new();
        ready.insert(HostId("gizmo".into()));
        let mut modal = NewSessionModal::new(vec![remote_repo("gizmo", "alpha")], ready);
        let _ = modal.handle_key(key(KeyCode::Enter));
        match &modal {
            NewSessionModal::Filling { repo, branch, .. } => {
                assert_eq!(repo.host.as_str(), "gizmo");
                assert!(
                    branch.is_empty(),
                    "remote repos should not pre-fill branch yet: {branch:?}"
                );
            }
            NewSessionModal::PickingRepo { .. } => panic!("expected Filling"),
        }
    }

    #[test]
    fn filling_typing_appends_to_focused_field() {
        let mut modal = NewSessionModal::Filling {
            repo: local_repo("a"),
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
            repo: local_repo("a"),
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
            repo: local_repo("a"),
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
            repo: local_repo("a"),
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
            repo: local_repo("agent-mux"),
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
