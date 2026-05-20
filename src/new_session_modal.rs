use std::collections::HashSet;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::host::LocalHost;
use crate::repo::Repo;
use crate::session::HostId;
use crate::worktree;

/// Initial cursor hint for the picker — derived from the dashboard's
/// currently-selected session. When the user presses `n` from a focused
/// row, the dominant intent is "another session in this same
/// neighbourhood," so the picker pre-positions over the matching repo.
/// `repo_path` matches against `Repo.path` (a session's `parent_repo`
/// when it's worktree-backed, else its `project_dir`); a host-only
/// match falls back to the first repo on that host.
#[derive(Debug, Clone)]
pub struct NewSessionSeed {
    pub host: HostId,
    pub repo_path: Option<PathBuf>,
}

pub enum NewSessionModal {
    PickingRepo {
        repos: Vec<Repo>,
        /// Hosts whose `Arc<dyn Host>` is currently registered in
        /// `App.hosts`. Repos belonging to other hosts render dimmed
        /// and are skipped on Enter — matches the M2 attach UX where
        /// cached rows are visible-but-inert until their host is up.
        ready_hosts: HashSet<HostId>,
        state: ListState,
        /// `Worktree` runs the standard pick → Filling → create flow.
        /// `NoWorktree` short-circuits to `SubmitNoWorktree` on Enter
        /// — claude launches in the repo root, no `git worktree add`,
        /// no task metadata.
        mode: ModalMode,
    },
    Filling {
        repo: Repo,
        task: String,
        branch: String,
        focus: FillFocus,
    },
}

/// Distinguishes the worktree-creating flow (default, bound to `n`) from
/// the open-in-repo-root flow (bound to `N`, raised by dogfooding 2026-
/// 05-19). The mode lives on `PickingRepo` so the same picker UI serves
/// both — only the title and the on-Enter dispatch differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalMode {
    Worktree,
    NoWorktree,
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
    /// User submitted a valid form in the worktree-creating flow.
    /// Caller runs `git worktree add` + spawns claude in the new
    /// worktree.
    Submit {
        repo: Repo,
        task: String,
        base_branch: String,
    },
    /// User picked a repo in [`ModalMode::NoWorktree`]. Caller spawns
    /// claude directly in the repo's root — no worktree, no task
    /// metadata, no base branch.
    SubmitNoWorktree { repo: Repo },
}

impl NewSessionModal {
    /// Open a new modal at the repo-picker stage. Caller must pass a
    /// non-empty list of repos and the set of hosts whose `Arc<dyn Host>`
    /// is currently live in `App.hosts`. Repos belonging to other
    /// hosts are visible-but-inert (rendered dim, ignored on Enter).
    /// `seed` biases the initial cursor toward the dashboard's
    /// currently-selected session's repo (see [`NewSessionSeed`]); pass
    /// `None` to open at index 0. Opens in [`ModalMode::Worktree`] —
    /// the standard pick → Filling → create flow.
    #[must_use]
    pub fn new(
        repos: Vec<Repo>,
        ready_hosts: HashSet<HostId>,
        seed: Option<NewSessionSeed>,
    ) -> Self {
        Self::with_mode(repos, ready_hosts, seed, ModalMode::Worktree)
    }

    /// Open a new modal in [`ModalMode::NoWorktree`] — Enter on a
    /// repo emits [`KeyOutcome::SubmitNoWorktree`] and the modal closes
    /// without a Filling stage. The picker UI is otherwise identical.
    #[must_use]
    pub fn new_no_worktree(
        repos: Vec<Repo>,
        ready_hosts: HashSet<HostId>,
        seed: Option<NewSessionSeed>,
    ) -> Self {
        Self::with_mode(repos, ready_hosts, seed, ModalMode::NoWorktree)
    }

    fn with_mode(
        repos: Vec<Repo>,
        ready_hosts: HashSet<HostId>,
        seed: Option<NewSessionSeed>,
        mode: ModalMode,
    ) -> Self {
        debug_assert!(!repos.is_empty(), "modal opened with no repos");
        let initial = seed.and_then(|s| seeded_index(&repos, &s)).unwrap_or(0);
        let mut state = ListState::default();
        state.select(Some(initial));
        Self::PickingRepo {
            repos,
            ready_hosts,
            state,
            mode,
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
            mode,
        } = self
        {
            let (outcome, next) = handle_picking(repos, ready_hosts, state, *mode, key);
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
                mode,
            } => draw_picking(frame, area, repos, ready_hosts, state, *mode),
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
    mode: ModalMode,
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
            // inert — we can't run `git worktree add` (or spawn
            // anything via `Host::run`) on it, so the picker silently
            // no-ops rather than transitioning into a state the user
            // can't progress from. Once the host connects, the row
            // un-dims and Enter works.
            if !ready_hosts.contains(&repo.host) {
                return (KeyOutcome::Handled, None);
            }
            if mode == ModalMode::NoWorktree {
                // Skip Filling entirely — there's no worktree to name
                // and no base branch to resolve. The caller spawns
                // claude directly in the repo root.
                return (KeyOutcome::SubmitNoWorktree { repo }, None);
            }
            // Default-branch resolution is a local-only fast path
            // today; for remote repos we leave the field blank and
            // let the user type it.
            let branch = if repo.host.is_local() {
                // Synchronous local git call; fast. Remote default-
                // branch resolution would require an SSH round-trip
                // mid-keypress which would block the UI; the field
                // stays empty for remote repos and the user types
                // their branch. A future pass can pre-resolve remote
                // branches asynchronously during workspace scan.
                let host = LocalHost::new();
                worktree::resolve_default_base_branch(&host, &repo.path).unwrap_or_default()
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

/// Pick the picker's initial cursor position from a seed. Prefer an
/// exact host + path match; fall back to the first repo on the seeded
/// host; return `None` to let the caller default to index 0.
fn seeded_index(repos: &[Repo], seed: &NewSessionSeed) -> Option<usize> {
    if let Some(path) = seed.repo_path.as_ref()
        && let Some(idx) = repos
            .iter()
            .position(|r| r.host == seed.host && &r.path == path)
    {
        return Some(idx);
    }
    repos.iter().position(|r| r.host == seed.host)
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
    mode: ModalMode,
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
    // Surface the mode in the title so the user can tell at a glance
    // whether they pressed `n` (worktree) or `N` (no worktree) — both
    // open the same picker UI and otherwise look identical.
    let title = match mode {
        ModalMode::Worktree => " new session: pick a repo ",
        ModalMode::NoWorktree => " new session (no worktree): pick a repo ",
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
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
            Self::SubmitNoWorktree { repo } => f
                .debug_struct("SubmitNoWorktree")
                .field("repo", &repo.name)
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
        let modal =
            NewSessionModal::new(vec![local_repo("a"), local_repo("b")], ready_local(), None);
        match modal {
            NewSessionModal::PickingRepo { state, .. } => {
                assert_eq!(state.selected(), Some(0));
            }
            NewSessionModal::Filling { .. } => panic!("expected PickingRepo"),
        }
    }

    #[test]
    fn esc_returns_cancel_from_any_state() {
        let mut modal = NewSessionModal::new(vec![local_repo("a")], ready_local(), None);
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
            None,
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
        let mut modal =
            NewSessionModal::new(vec![local_repo("a"), local_repo("b")], ready_local(), None);
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
            None,
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
        let mut modal = NewSessionModal::new(vec![remote_repo("gizmo", "alpha")], ready, None);
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
    fn seed_with_exact_host_and_path_lands_on_that_repo() {
        // Worktree-backed sessions match against their parent repo's
        // path; the picker should put the cursor on the matching row
        // so the user can hit Enter and stay in-context.
        let repos = vec![
            local_repo("alpha"),
            remote_repo("gizmo", "beta"),
            local_repo("gamma"),
        ];
        let seed = NewSessionSeed {
            host: HostId::local(),
            repo_path: Some(PathBuf::from("/tmp/gamma")),
        };
        let modal = NewSessionModal::new(repos, ready_local(), Some(seed));
        match modal {
            NewSessionModal::PickingRepo { state, .. } => {
                assert_eq!(state.selected(), Some(2));
            }
            NewSessionModal::Filling { .. } => panic!("expected PickingRepo"),
        }
    }

    #[test]
    fn seed_with_host_match_only_falls_back_to_first_repo_on_that_host() {
        // No exact path match (the seed points at a repo the registry
        // doesn't know) — pre-position on the first repo for the seed's
        // host so the user is at least in the right neighbourhood.
        let mut ready = HashSet::new();
        ready.insert(HostId::local());
        ready.insert(HostId("gizmo".into()));
        let repos = vec![
            local_repo("alpha"),
            remote_repo("gizmo", "beta"),
            remote_repo("gizmo", "delta"),
        ];
        let seed = NewSessionSeed {
            host: HostId("gizmo".into()),
            repo_path: Some(PathBuf::from("/srv/missing")),
        };
        let modal = NewSessionModal::new(repos, ready, Some(seed));
        match modal {
            NewSessionModal::PickingRepo { state, .. } => {
                // First gizmo repo is at index 1.
                assert_eq!(state.selected(), Some(1));
            }
            NewSessionModal::Filling { .. } => panic!("expected PickingRepo"),
        }
    }

    #[test]
    fn no_worktree_mode_emits_submit_no_worktree_on_enter_without_filling_stage() {
        // The defining behaviour of `ModalMode::NoWorktree`: Enter on a
        // ready repo skips Filling entirely and emits SubmitNoWorktree
        // carrying the picked repo. Pin this so a future refactor of
        // the dispatch can't accidentally route this case through the
        // task/branch form.
        let mut modal = NewSessionModal::new_no_worktree(
            vec![local_repo("alpha"), local_repo("beta")],
            ready_local(),
            None,
        );
        modal.handle_key(key(KeyCode::Down));
        match modal.handle_key(key(KeyCode::Enter)) {
            KeyOutcome::SubmitNoWorktree { repo } => {
                assert_eq!(repo.name, "beta");
            }
            other => panic!("expected SubmitNoWorktree, got {other:?}"),
        }
    }

    #[test]
    fn no_worktree_mode_still_no_ops_on_non_ready_host() {
        // Same visible-but-inert behaviour as the worktree flow — a
        // host that hasn't reached `Connected` yet can't spawn anything
        // through `Host::run`, so Enter is silently dropped instead of
        // surfacing as a no-op submit the caller couldn't fulfil.
        let mut ready = HashSet::new();
        ready.insert(HostId::local());
        let mut modal = NewSessionModal::new_no_worktree(
            vec![local_repo("here"), remote_repo("gizmo", "there")],
            ready,
            None,
        );
        modal.handle_key(key(KeyCode::Down));
        let outcome = modal.handle_key(key(KeyCode::Enter));
        assert!(matches!(outcome, KeyOutcome::Handled));
        assert!(matches!(modal, NewSessionModal::PickingRepo { .. }));
    }

    #[test]
    fn seed_with_no_matches_falls_back_to_index_zero() {
        // Seed references a host that doesn't have any repos in the
        // registry — defaults to the existing index-0 behaviour rather
        // than failing or showing nothing selected.
        let repos = vec![local_repo("alpha"), local_repo("beta")];
        let seed = NewSessionSeed {
            host: HostId("unknown".into()),
            repo_path: None,
        };
        let modal = NewSessionModal::new(repos, ready_local(), Some(seed));
        match modal {
            NewSessionModal::PickingRepo { state, .. } => {
                assert_eq!(state.selected(), Some(0));
            }
            NewSessionModal::Filling { .. } => panic!("expected PickingRepo"),
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
