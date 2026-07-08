//! Edited-files picker modal — list the files a session's Claude has
//! edited, fuzzy-filter, and open the chosen one in a file-scoped
//! `[[tools]]` editor (e.g. `vim {file}`).
//!
//! This is the "open the file Claude just changed" affordance. It is a
//! sibling of the quickswitcher: opened over an immutable snapshot the
//! caller hands in at open time (the selected session's
//! `Session.edited_files`), so it honours the project's "switching never
//! blocks on I/O" discipline by construction — no catalog mutation, no
//! host calls, no filesystem walk.
//!
//! The modal owns only its query buffer and selection. On Enter it yields
//! the chosen [`PathBuf`]; the caller ([`crate::main`]) substitutes it for
//! `{file}` in the tool that opened the modal (tracked via
//! [`EditedFilesModal::tool_idx`]) and runs the same `spawn_tool` path a
//! plain tool launch would — so this is a thin file-selection layer over
//! the one tool-launch flow, not a parallel one.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::quickswitcher::fuzzy_score;

/// One file row in the picker. `display` is the shortened path shown to
/// the user (relative to the session cwd when it lives under it, else
/// absolute); `path` is the absolute path substituted into the tool
/// command; `haystack` is the pre-lowercased string the fuzzy matcher
/// scores against.
#[derive(Debug, Clone)]
pub struct EditedFileEntry {
    pub display: String,
    pub path: PathBuf,
    pub haystack: String,
}

/// What the caller should do after handing a key to the modal.
pub enum EditedFilesOutcome {
    /// Modal consumed the key; keep it open.
    Handled,
    /// User pressed Esc. Caller drops the modal.
    Cancel,
    /// User pressed Enter on a file. Caller drops the modal and launches
    /// the file-scoped tool against `path`.
    Pick(PathBuf),
}

pub struct EditedFilesModal {
    /// Index into `App.config.tools` of the file-scoped tool that opened
    /// this modal. Read back at pick time so a config reload can't leave
    /// a stale binding (mirrors `Action::LaunchTool`).
    tool_idx: usize,
    /// Label of the tool that opened the modal, for the title bar.
    tool_label: String,
    entries: Vec<EditedFileEntry>,
    query: String,
    /// Indices into `entries`, best match first. Recomputed on every
    /// query edit. Empty query keeps the caller's most-recent-first order.
    filtered: Vec<usize>,
    state: ListState,
}

impl EditedFilesModal {
    /// Open a picker over `entries` (pre-ordered most-recent-edit-first).
    /// `tool_idx` / `tool_label` identify the file-scoped tool that will
    /// receive the picked path. Opens with the first entry selected.
    #[must_use]
    pub fn new(tool_idx: usize, tool_label: String, entries: Vec<EditedFileEntry>) -> Self {
        let filtered: Vec<usize> = (0..entries.len()).collect();
        let mut state = ListState::default();
        if !filtered.is_empty() {
            state.select(Some(0));
        }
        Self {
            tool_idx,
            tool_label,
            entries,
            query: String::new(),
            filtered,
            state,
        }
    }

    /// Which `[[tools]]` binding this modal launches on pick.
    #[must_use]
    pub fn tool_idx(&self) -> usize {
        self.tool_idx
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> EditedFilesOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => EditedFilesOutcome::Cancel,
            KeyCode::Enter => match self.selected_path() {
                Some(p) => EditedFilesOutcome::Pick(p),
                None => EditedFilesOutcome::Handled,
            },
            KeyCode::Up => {
                self.move_up();
                EditedFilesOutcome::Handled
            }
            KeyCode::Char('p') if ctrl => {
                self.move_up();
                EditedFilesOutcome::Handled
            }
            KeyCode::Down => {
                self.move_down();
                EditedFilesOutcome::Handled
            }
            KeyCode::Char('n') if ctrl => {
                self.move_down();
                EditedFilesOutcome::Handled
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.recompute();
                EditedFilesOutcome::Handled
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.query.push(c);
                self.recompute();
                EditedFilesOutcome::Handled
            }
            _ => EditedFilesOutcome::Handled,
        }
    }

    fn selected_path(&self) -> Option<PathBuf> {
        let row = self.state.selected()?;
        let entry_idx = *self.filtered.get(row)?;
        self.entries.get(entry_idx).map(|e| e.path.clone())
    }

    fn move_up(&mut self) {
        let len = self.filtered.len();
        if len == 0 {
            return;
        }
        let i = self.state.selected().unwrap_or(0);
        let prev = if i == 0 { len - 1 } else { i - 1 };
        self.state.select(Some(prev));
    }

    fn move_down(&mut self) {
        let len = self.filtered.len();
        if len == 0 {
            return;
        }
        let i = self.state.selected().unwrap_or(0);
        self.state.select(Some((i + 1) % len));
    }

    /// Re-rank `entries` against the current query. Empty query keeps the
    /// caller's most-recent-first order; otherwise entries the query is a
    /// fuzzy subsequence of are kept, scored, and sorted best-first (ties
    /// broken by original order). Selection resets to the top match.
    fn recompute(&mut self) {
        let q = self.query.to_lowercase();
        if q.is_empty() {
            self.filtered = (0..self.entries.len()).collect();
        } else {
            let mut scored: Vec<(usize, i32)> = self
                .entries
                .iter()
                .enumerate()
                .filter_map(|(i, e)| fuzzy_score(&e.haystack, &q).map(|s| (i, s)))
                .collect();
            scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            self.filtered = scored.into_iter().map(|(i, _)| i).collect();
        }
        if self.filtered.is_empty() {
            self.state.select(None);
        } else {
            self.state.select(Some(0));
        }
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>) {
        let area = centered_rect(60, 60, frame.area());
        frame.render_widget(Clear, area);
        let title = format!(" open with {}… ", self.tool_label);
        let block = Block::default().borders(Borders::ALL).title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let layout = Layout::vertical([
            Constraint::Length(1), // query
            Constraint::Min(0),    // results
            Constraint::Length(1), // hint
        ])
        .split(inner);

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("> ", Style::new().add_modifier(Modifier::DIM)),
                Span::raw(self.query.clone()),
                Span::styled("▌", Style::new().add_modifier(Modifier::DIM)),
            ])),
            layout[0],
        );

        let dim = Style::new().add_modifier(Modifier::DIM);
        if self.filtered.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled("  no matches", dim))),
                layout[1],
            );
        } else {
            let items: Vec<ListItem<'_>> = self
                .filtered
                .iter()
                .filter_map(|&i| self.entries.get(i))
                .map(|e| ListItem::new(Line::from(Span::raw(e.display.clone()))))
                .collect();
            let list = List::new(items)
                .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
                .highlight_symbol("▌ ");
            frame.render_stateful_widget(list, layout[1], &mut self.state);
        }

        let count = self.filtered.len();
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {count} file{} · ⏎: open · Esc: cancel ", plural(count)),
                dim,
            ))),
            layout[2],
        );
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn entry(display: &str, path: &str) -> EditedFileEntry {
        EditedFileEntry {
            display: display.to_string(),
            path: PathBuf::from(path),
            haystack: display.to_lowercase(),
        }
    }

    fn modal() -> EditedFilesModal {
        EditedFilesModal::new(
            3,
            "edit".into(),
            vec![
                entry("src/main.rs", "/work/proj/src/main.rs"),
                entry("src/watcher.rs", "/work/proj/src/watcher.rs"),
                entry("README.md", "/work/proj/README.md"),
            ],
        )
    }

    #[test]
    fn opens_on_first_entry_and_reports_tool_idx() {
        let m = modal();
        assert_eq!(m.tool_idx(), 3);
        assert_eq!(m.state.selected(), Some(0));
        assert_eq!(m.filtered, vec![0, 1, 2]);
    }

    #[test]
    fn enter_picks_the_selected_path() {
        let mut m = modal();
        m.handle_key(key(KeyCode::Down));
        match m.handle_key(key(KeyCode::Enter)) {
            EditedFilesOutcome::Pick(p) => {
                assert_eq!(p, PathBuf::from("/work/proj/src/watcher.rs"));
            }
            _ => panic!("expected Pick"),
        }
    }

    #[test]
    fn typing_filters_to_matching_files() {
        let mut m = modal();
        for c in "readme".chars() {
            m.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(m.filtered.len(), 1);
        match m.handle_key(key(KeyCode::Enter)) {
            EditedFilesOutcome::Pick(p) => {
                assert_eq!(p, PathBuf::from("/work/proj/README.md"));
            }
            _ => panic!("expected Pick of README"),
        }
    }

    #[test]
    fn no_match_clears_selection_and_enter_is_inert() {
        let mut m = modal();
        for c in "zzzzz".chars() {
            m.handle_key(key(KeyCode::Char(c)));
        }
        assert!(m.filtered.is_empty());
        assert_eq!(m.state.selected(), None);
        assert!(matches!(
            m.handle_key(key(KeyCode::Enter)),
            EditedFilesOutcome::Handled
        ));
    }

    #[test]
    fn esc_cancels() {
        let mut m = modal();
        assert!(matches!(
            m.handle_key(key(KeyCode::Esc)),
            EditedFilesOutcome::Cancel
        ));
    }

    #[test]
    fn backspace_widens_results() {
        let mut m = modal();
        m.handle_key(key(KeyCode::Char('r')));
        m.handle_key(key(KeyCode::Char('e')));
        let narrowed = m.filtered.len();
        m.handle_key(key(KeyCode::Backspace));
        m.handle_key(key(KeyCode::Backspace));
        assert!(m.filtered.len() >= narrowed);
        assert_eq!(m.filtered.len(), 3);
    }
}
