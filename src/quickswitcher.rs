//! Quickswitcher modal — fuzzy-find across every session, tool launch,
//! and offline favorite, then jump straight to it.
//!
//! This is the "which window was that conversation in?" friction the
//! SPEC opens with, driven toward zero: open with `Ctrl-P`, type a few
//! characters of the title / project / host, press Enter, and you're
//! attached. The match is a pure in-memory filter over a snapshot the
//! caller hands in at open time, so it honours the project's
//! "switching never blocks on I/O" discipline by construction — no
//! catalog mutation, no host calls.
//!
//! The modal owns only its query buffer and selection. It knows
//! nothing about the sidebar's row model: on Enter it yields a
//! [`SwitchTarget`] and the caller ([`crate::main`]) re-seats the
//! sidebar cursor onto the matching row (via the existing
//! `SelectionAnchor` machinery) and runs the same attach path a manual
//! Enter would. That keeps the switcher a thin selection layer over the
//! one true attach flow rather than a parallel one.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::session::{HostId, SessionId};

/// One jump target. Mirrors the selectable `DisplayRow` kinds so the
/// caller can re-seat the sidebar cursor onto the matching row before
/// attaching. `Session` carries `in_favorites` only so the re-seat lands
/// on the same *copy* a manual cursor would (a favorited session renders
/// twice); attach behaviour is identical either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchTarget {
    Session { id: SessionId, in_favorites: bool },
    Tool { tmux_session: String },
    Placeholder { host: HostId, id: SessionId },
}

/// A candidate row in the switcher. `label` is the primary text (the
/// session's display title); `context` is the dim secondary line
/// (project · host). `haystack` is the pre-lowercased string the fuzzy
/// matcher scores against — the caller folds title + project + host into
/// it so a query can hit any of them.
#[derive(Debug, Clone)]
pub struct SwitchEntry {
    pub label: String,
    pub context: String,
    pub haystack: String,
    pub target: SwitchTarget,
}

/// What the caller should do after handing a key to the modal.
pub enum SwitchOutcome {
    /// Modal consumed the key; keep it open.
    Handled,
    /// User pressed Esc. Caller drops the modal.
    Cancel,
    /// User pressed Enter on a candidate. Caller drops the modal,
    /// re-seats the cursor onto the target, and attaches.
    Pick(SwitchTarget),
}

pub struct QuickSwitcher {
    entries: Vec<SwitchEntry>,
    query: String,
    /// Indices into `entries`, best match first. Recomputed on every
    /// query edit. With an empty query this is `0..entries.len()` — the
    /// caller pre-orders `entries` by recency so the unfiltered view
    /// opens on the most-recently-active session.
    filtered: Vec<usize>,
    state: ListState,
}

impl QuickSwitcher {
    /// Open a switcher over `entries`. Pass them pre-ordered by recency
    /// — that order is what the user sees before typing. Opens with the
    /// first entry selected (if any).
    #[must_use]
    pub fn new(entries: Vec<SwitchEntry>) -> Self {
        let filtered: Vec<usize> = (0..entries.len()).collect();
        let mut state = ListState::default();
        if !filtered.is_empty() {
            state.select(Some(0));
        }
        Self {
            entries,
            query: String::new(),
            filtered,
            state,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> SwitchOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => SwitchOutcome::Cancel,
            KeyCode::Enter => match self.selected_target() {
                Some(t) => SwitchOutcome::Pick(t),
                None => SwitchOutcome::Handled,
            },
            // fzf-style Ctrl-n / Ctrl-p navigate without leaving the
            // text field — and Ctrl-P (the open key) reads as "up" once
            // the modal owns the keyboard, so a held Ctrl-P walks down…
            // up the list naturally.
            KeyCode::Up => {
                self.move_up();
                SwitchOutcome::Handled
            }
            KeyCode::Char('p') if ctrl => {
                self.move_up();
                SwitchOutcome::Handled
            }
            KeyCode::Down => {
                self.move_down();
                SwitchOutcome::Handled
            }
            KeyCode::Char('n') if ctrl => {
                self.move_down();
                SwitchOutcome::Handled
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.recompute();
                SwitchOutcome::Handled
            }
            // Printable input extends the query. Guard on no Ctrl/Alt so
            // chords (Ctrl-n above, a stray Alt-x) don't leak literal
            // characters into the buffer.
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.query.push(c);
                self.recompute();
                SwitchOutcome::Handled
            }
            _ => SwitchOutcome::Handled,
        }
    }

    fn selected_target(&self) -> Option<SwitchTarget> {
        let row = self.state.selected()?;
        let entry_idx = *self.filtered.get(row)?;
        self.entries.get(entry_idx).map(|e| e.target.clone())
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

    /// Re-rank `entries` against the current query. Empty query keeps
    /// the caller's recency order; otherwise entries that the query is a
    /// fuzzy subsequence of are kept, scored, and sorted best-first
    /// (ties broken by original recency order). Selection resets to the
    /// top match — the user is re-aiming with each keystroke.
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
        let block = Block::default().borders(Borders::ALL).title(" switch to… ");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let layout = Layout::vertical([
            Constraint::Length(1), // query
            Constraint::Min(0),    // results
            Constraint::Length(1), // hint
        ])
        .split(inner);

        // Query line with a trailing block cursor so it reads as a live
        // text field (mirrors the search bar's Editing affordance).
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
                .map(|e| {
                    ListItem::new(Line::from(vec![
                        Span::raw(e.label.clone()),
                        Span::raw("  "),
                        Span::styled(e.context.clone(), dim),
                    ]))
                })
                .collect();
            let list = List::new(items)
                .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
                .highlight_symbol("▌ ");
            frame.render_stateful_widget(list, layout[1], &mut self.state);
        }

        let count = self.filtered.len();
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {count} match{} · ⏎: switch · Esc: cancel ", plural(count)),
                dim,
            ))),
            layout[2],
        );
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "es" }
}

/// Score `needle` against `haystack` (both already lowercased) as a
/// fuzzy subsequence. Returns `None` when `needle` isn't a subsequence
/// at all; otherwise a score where higher is a tighter match. The
/// scoring rewards consecutive runs and word-boundary starts (so
/// "depl" prefers "deploy-fix" over a scattered "…d…e…p…l…") and lightly
/// penalises long gaps and long haystacks. An empty needle scores 0 and
/// matches everything.
#[must_use]
pub fn fuzzy_score(haystack: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = haystack.chars().collect();
    let mut score = 0i32;
    let mut cursor = 0usize;
    let mut prev_match: Option<usize> = None;
    for nc in needle.chars() {
        let offset = hay[cursor..].iter().position(|&c| c == nc)?;
        let pos = cursor + offset;
        score += 1;
        // Consecutive-character bonus: a contiguous run is a much
        // stronger signal than the same chars scattered.
        if prev_match == Some(pos.wrapping_sub(1)) {
            score += 6;
        }
        // Word-boundary bonus: matching at the start of the string or
        // just after a separator (space, '-', '_', '/', '.').
        let at_boundary = pos == 0 || hay.get(pos - 1).is_some_and(|c| !c.is_alphanumeric());
        if at_boundary {
            score += 4;
        }
        // Gap penalty: characters skipped to reach this match, capped so
        // one long jump doesn't dominate. The cap keeps this in `0..=3`,
        // so the conversion can't truncate.
        score -= i32::try_from(offset.min(3)).unwrap_or(3);
        prev_match = Some(pos);
        cursor = pos + 1;
    }
    // Mild preference for shorter haystacks (less noise around the hit).
    score -= i32::try_from(hay.len() / 40).unwrap_or(i32::MAX);
    Some(score)
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
    use crossterm::event::KeyEventKind;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    fn entry(label: &str, haystack: &str, id: &str) -> SwitchEntry {
        SwitchEntry {
            label: label.to_string(),
            context: String::new(),
            haystack: haystack.to_lowercase(),
            target: SwitchTarget::Session {
                id: SessionId(id.to_string()),
                in_favorites: false,
            },
        }
    }

    #[test]
    fn fuzzy_score_returns_none_when_not_a_subsequence() {
        assert!(fuzzy_score("deploy-fix", "xyz").is_none());
    }

    #[test]
    fn fuzzy_score_matches_subsequence() {
        assert!(fuzzy_score("deploy-fix", "dfx").is_some());
    }

    #[test]
    fn fuzzy_score_prefers_contiguous_prefix_over_scattered() {
        let contiguous = fuzzy_score("deploy-fix", "depl").unwrap();
        let scattered = fuzzy_score("dance-pelican", "depl").unwrap();
        assert!(
            contiguous > scattered,
            "contiguous {contiguous} should beat scattered {scattered}"
        );
    }

    #[test]
    fn fuzzy_score_empty_needle_matches() {
        assert_eq!(fuzzy_score("anything", ""), Some(0));
    }

    #[test]
    fn empty_query_keeps_recency_order() {
        let sw = QuickSwitcher::new(vec![
            entry("alpha", "alpha", "a"),
            entry("beta", "beta", "b"),
        ]);
        assert_eq!(sw.filtered, vec![0, 1]);
        assert_eq!(sw.state.selected(), Some(0));
    }

    #[test]
    fn typing_filters_and_ranks() {
        let mut sw = QuickSwitcher::new(vec![
            entry("web server", "web server", "a"),
            entry("deploy fix", "deploy fix", "b"),
            entry("debug pane", "debug pane", "c"),
        ]);
        for c in "de".chars() {
            sw.handle_key(key(KeyCode::Char(c)));
        }
        // "web server" has no 'd' then 'e' subsequence → dropped.
        assert_eq!(sw.filtered.len(), 2);
        assert!(sw.filtered.iter().all(|&i| i != 0));
        // Selection re-aims to the top match on each keystroke.
        assert_eq!(sw.state.selected(), Some(0));
    }

    #[test]
    fn no_match_clears_selection_and_pick_is_inert() {
        let mut sw = QuickSwitcher::new(vec![entry("alpha", "alpha", "a")]);
        for c in "zzz".chars() {
            sw.handle_key(key(KeyCode::Char(c)));
        }
        assert!(sw.filtered.is_empty());
        assert_eq!(sw.state.selected(), None);
        assert!(matches!(
            sw.handle_key(key(KeyCode::Enter)),
            SwitchOutcome::Handled
        ));
    }

    #[test]
    fn enter_picks_selected_target() {
        let mut sw = QuickSwitcher::new(vec![
            entry("alpha", "alpha", "a"),
            entry("beta", "beta", "b"),
        ]);
        sw.handle_key(key(KeyCode::Down));
        match sw.handle_key(key(KeyCode::Enter)) {
            SwitchOutcome::Pick(SwitchTarget::Session { id, .. }) => {
                assert_eq!(id, SessionId("b".to_string()));
            }
            _ => panic!("expected Pick of second entry"),
        }
    }

    #[test]
    fn esc_cancels() {
        let mut sw = QuickSwitcher::new(vec![entry("alpha", "alpha", "a")]);
        assert!(matches!(
            sw.handle_key(key(KeyCode::Esc)),
            SwitchOutcome::Cancel
        ));
    }

    #[test]
    fn ctrl_n_and_p_navigate_without_typing() {
        let mut sw = QuickSwitcher::new(vec![
            entry("alpha", "alpha", "a"),
            entry("beta", "beta", "b"),
        ]);
        sw.handle_key(ctrl(KeyCode::Char('n')));
        assert_eq!(sw.state.selected(), Some(1));
        assert!(sw.query.is_empty(), "Ctrl-n must not type 'n'");
        sw.handle_key(ctrl(KeyCode::Char('p')));
        assert_eq!(sw.state.selected(), Some(0));
        assert!(sw.query.is_empty(), "Ctrl-p must not type 'p'");
    }

    #[test]
    fn down_wraps_around() {
        let mut sw = QuickSwitcher::new(vec![
            entry("alpha", "alpha", "a"),
            entry("beta", "beta", "b"),
        ]);
        sw.handle_key(key(KeyCode::Down));
        sw.handle_key(key(KeyCode::Down));
        assert_eq!(sw.state.selected(), Some(0));
    }

    #[test]
    fn backspace_widens_results() {
        let mut sw = QuickSwitcher::new(vec![
            entry("alpha", "alpha", "a"),
            entry("beta", "beta", "b"),
        ]);
        sw.handle_key(key(KeyCode::Char('a')));
        sw.handle_key(key(KeyCode::Char('l')));
        let narrowed = sw.filtered.len();
        sw.handle_key(key(KeyCode::Backspace));
        sw.handle_key(key(KeyCode::Backspace));
        assert!(sw.filtered.len() >= narrowed);
        assert_eq!(sw.filtered.len(), 2);
    }
}
