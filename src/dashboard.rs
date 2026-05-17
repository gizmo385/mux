use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::SystemTime;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::session::{HostId, Session};

/// One row in the dashboard list. Headers are unselectable; the
/// selection model lives in main.rs and skips past `HostHeader` and
/// `ProjectHeader` rows via [`next_session_index`] /
/// [`prev_session_index`].
///
/// Carries owned `HostId` / `PathBuf` so the row is self-contained —
/// the previous parallel-`Vec<HostId>` indirection went away when
/// project headers were added (`PathBuf` isn't `Copy`, so the row enum
/// can't be `Copy` either, removing the original reason for the
/// indirection).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayRow {
    HostHeader(HostId),
    ProjectHeader(PathBuf),
    SessionRow(usize),
}

/// Group `sessions` by host (local first, SSH hosts alphabetical),
/// then by project within each host (project ordered by its
/// most-recent session, sessions within a project by recency desc).
/// Returns the flat row list ready for rendering.
///
/// Why local-first: it's the user's home base and the most common
/// destination; surfacing it at the top matches the mental model.
/// Why alphabetical for the rest: stable across runs, no surprises
/// when a new host is added.
#[must_use]
pub fn build_display_rows(sessions: &[Session]) -> Vec<DisplayRow> {
    build_display_rows_filtered(sessions, |_| true)
}

/// Like [`build_display_rows`] but emits rows only for sessions where
/// `include(i)` returns true. Host and project headers are omitted when
/// none of their children survive the filter — the grouping collapses
/// naturally rather than leaving orphaned headers.
///
/// `SessionRow(i)` indices still point into the original `sessions`
/// slice, so the dashboard can resolve a row back to its session
/// without holding a parallel filtered slice.
#[must_use]
pub fn build_display_rows_filtered<F>(sessions: &[Session], include: F) -> Vec<DisplayRow>
where
    F: Fn(usize) -> bool,
{
    let mut by_host: BTreeMap<HostId, Vec<usize>> = BTreeMap::new();
    for (i, s) in sessions.iter().enumerate() {
        if !include(i) {
            continue;
        }
        by_host.entry(s.host.clone()).or_default().push(i);
    }

    let mut ordered_hosts: Vec<HostId> = by_host.keys().cloned().collect();
    ordered_hosts.sort_by(|a, b| match (a.is_local(), b.is_local()) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => a.as_str().cmp(b.as_str()),
    });

    let mut rows = Vec::new();
    for host in ordered_hosts {
        rows.push(DisplayRow::HostHeader(host.clone()));
        let session_idxs = by_host.remove(&host).unwrap_or_default();

        let mut by_project: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
        for &i in &session_idxs {
            by_project
                .entry(sessions[i].project_dir.clone())
                .or_default()
                .push(i);
        }

        // Order projects: most-recent-session desc, then by path (stable
        // tiebreaker for the rare case where two projects share a max).
        let mut ordered_projects: Vec<(PathBuf, Vec<usize>)> = by_project.into_iter().collect();
        ordered_projects.sort_by(|(pa, ia), (pb, ib)| {
            let max_a = ia
                .iter()
                .map(|&i| sessions[i].last_activity)
                .max()
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let max_b = ib
                .iter()
                .map(|&i| sessions[i].last_activity)
                .max()
                .unwrap_or(SystemTime::UNIX_EPOCH);
            max_b.cmp(&max_a).then(pa.cmp(pb))
        });

        for (project, mut session_idxs) in ordered_projects {
            rows.push(DisplayRow::ProjectHeader(project));
            session_idxs
                .sort_by(|&a, &b| sessions[b].last_activity.cmp(&sessions[a].last_activity));
            for i in session_idxs {
                rows.push(DisplayRow::SessionRow(i));
            }
        }
    }
    rows
}

/// Case-insensitive substring match for a session against the dashboard
/// search query. Matches the three signals the user reads when scanning
/// the list: title (when present), `project_dir`, and host label. The
/// session id itself is intentionally excluded — it isn't shown by
/// default (only the last-6 fallback suffix appears for title-less
/// sessions), and matching on opaque uuid fragments produces noise.
///
/// `query_lower` must already be lowercased — callers do this once per
/// keystroke rather than per row.
#[must_use]
pub fn matches_query(session: &Session, query_lower: &str) -> bool {
    if query_lower.is_empty() {
        return true;
    }
    if let Some(title) = session.title.as_deref()
        && title.to_lowercase().contains(query_lower)
    {
        return true;
    }
    if session
        .project_dir
        .to_string_lossy()
        .to_lowercase()
        .contains(query_lower)
    {
        return true;
    }
    session.host.as_str().to_lowercase().contains(query_lower)
}

/// Dashboard search/filter state. `None` on App means "no search active".
///
/// Two modes, distinct because the keyboard ownership differs:
/// - `Editing`: the search bar owns input — characters append to the
///   query, the list filters live. Used while the user is composing
///   the filter.
/// - `Active`: the filter persists but j/k/Enter/n/etc. work normally.
///   Entered by pressing Enter from `Editing` ("I'm done typing, let
///   me navigate the results"). `/` re-enters `Editing`.
///
/// `Esc` from either mode exits search entirely.
#[derive(Debug, Clone, Default)]
pub struct SearchState {
    pub query: String,
    pub mode: SearchMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchMode {
    #[default]
    Editing,
    Active,
}

/// What the main loop should do in response to a search-mode key event.
/// Only meaningful for `Editing`-mode input — `Active`-mode keys are
/// handled directly in the main loop so they can fall through to the
/// regular action dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchOutcome {
    /// Key consumed; query may have changed, mode unchanged. Caller
    /// should re-seat selection in case the previously-selected row
    /// fell out of the filter.
    Edited,
    /// User pressed Enter — transitioned `Editing` → `Active`.
    Commit,
    /// User pressed Esc — caller should drop the search state.
    Exit,
}

impl SearchState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Route an `Editing`-mode key. Panics in debug if called while the
    /// state is in `Active` mode — the main loop handles those keys
    /// itself so they can fall through to the regular action dispatch.
    pub fn handle_editing_key(&mut self, key: KeyEvent) -> SearchOutcome {
        debug_assert!(matches!(self.mode, SearchMode::Editing));
        match key.code {
            KeyCode::Esc => SearchOutcome::Exit,
            KeyCode::Enter => {
                self.mode = SearchMode::Active;
                SearchOutcome::Commit
            }
            KeyCode::Backspace => {
                self.query.pop();
                SearchOutcome::Edited
            }
            KeyCode::Char(c) if is_printable_input(key.modifiers) => {
                self.query.push(c);
                SearchOutcome::Edited
            }
            _ => SearchOutcome::Edited,
        }
    }
}

/// A key combo counts as text input only when no control modifiers are
/// pressed. Shift is fine (it just delivers an uppercase char); Ctrl
/// and Alt should fall through to action dispatch so e.g. Ctrl-C still
/// quits while the search bar has focus.
fn is_printable_input(modifiers: KeyModifiers) -> bool {
    modifiers.is_empty() || modifiers == KeyModifiers::SHIFT
}

/// Next selectable (`SessionRow`) index after `current`, wrapping at
/// the end. Returns `None` only if `rows` contains no session rows.
#[must_use]
pub fn next_session_index(current: Option<usize>, rows: &[DisplayRow]) -> Option<usize> {
    walk_session_index(current, rows, 1)
}

/// Previous selectable (`SessionRow`) index before `current`, wrapping
/// at the start. Returns `None` only if `rows` contains no session rows.
#[must_use]
pub fn prev_session_index(current: Option<usize>, rows: &[DisplayRow]) -> Option<usize> {
    walk_session_index(current, rows, -1)
}

fn walk_session_index(current: Option<usize>, rows: &[DisplayRow], step: isize) -> Option<usize> {
    let n = rows.len();
    if n == 0 {
        return None;
    }
    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    let n_i = n as isize;
    let start: isize = match current {
        #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
        Some(c) => (c as isize + step).rem_euclid(n_i),
        None => 0,
    };
    for offset in 0..n_i {
        let i = (start + offset * step).rem_euclid(n_i);
        #[allow(clippy::cast_sign_loss)]
        let idx = i as usize;
        if matches!(rows[idx], DisplayRow::SessionRow(_)) {
            return Some(idx);
        }
    }
    None
}

/// First `SessionRow` index in `rows`, or `None` if there are no
/// sessions. Used to seed selection when the catalog goes from empty
/// to non-empty.
#[must_use]
pub fn first_session_index(rows: &[DisplayRow]) -> Option<usize> {
    rows.iter()
        .position(|r| matches!(r, DisplayRow::SessionRow(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Attention, SessionId};
    use std::path::PathBuf;
    use std::time::Duration;

    fn session(id: &str, host: &str, project: &str, seconds_ago: u64) -> Session {
        session_with_title(id, host, project, seconds_ago, None)
    }

    fn session_with_title(
        id: &str,
        host: &str,
        project: &str,
        seconds_ago: u64,
        title: Option<&str>,
    ) -> Session {
        Session {
            id: SessionId(id.to_string()),
            host: HostId(host.to_string()),
            project_dir: PathBuf::from(project),
            transcript_path: PathBuf::from(format!("/t/{id}.jsonl")),
            last_activity: SystemTime::UNIX_EPOCH + Duration::from_secs(10_000 - seconds_ago),
            attention: Attention::Unknown,
            title: title.map(str::to_string),
            has_live_pane: None,
        }
    }

    /// Helper: stringify the row layout as a flat Vec for assertions —
    /// host headers as `"H:<id>"`, project headers as `"P:<path>"`,
    /// session rows as `"S:<id>"`.
    fn layout(sessions: &[Session]) -> Vec<String> {
        build_display_rows(sessions)
            .into_iter()
            .map(|r| match r {
                DisplayRow::HostHeader(host) => format!("H:{host}"),
                DisplayRow::ProjectHeader(path) => format!("P:{}", path.display()),
                DisplayRow::SessionRow(i) => format!("S:{}", sessions[i].id.0),
            })
            .collect()
    }

    #[test]
    fn empty_input_yields_empty_layout() {
        let rows = layout(&[]);
        assert!(rows.is_empty());
    }

    #[test]
    fn single_session_gets_host_project_and_session_rows() {
        let s = vec![session("a", "local", "/p", 0)];
        assert_eq!(layout(&s), vec!["H:local", "P:/p", "S:a"]);
    }

    #[test]
    fn local_host_comes_before_ssh_hosts() {
        let s = vec![
            session("a", "alpenglow", "/p", 0),
            session("b", "local", "/p", 0),
        ];
        let l = layout(&s);
        assert_eq!(
            l,
            vec!["H:local", "P:/p", "S:b", "H:alpenglow", "P:/p", "S:a"]
        );
    }

    #[test]
    fn ssh_hosts_are_alphabetical_after_local() {
        let s = vec![
            session("a", "zeta", "/p", 0),
            session("b", "alpha", "/p", 0),
            session("c", "local", "/p", 0),
        ];
        let l = layout(&s);
        assert_eq!(
            l,
            vec![
                "H:local", "P:/p", "S:c", "H:alpha", "P:/p", "S:b", "H:zeta", "P:/p", "S:a",
            ]
        );
    }

    #[test]
    fn sessions_in_same_project_share_one_project_header() {
        // Two sessions in /p1, one in /p2 — should produce one P:/p1
        // header followed by both p1 sessions (recency desc within
        // project), then P:/p2 with its single session.
        let s = vec![
            session("p1-a", "local", "/p1", 100), // older p1
            session("p2-x", "local", "/p2", 50),
            session("p1-b", "local", "/p1", 10), // most recent overall
        ];
        let l = layout(&s);
        // p1 wins project ordering (max 10s vs p2's 50s).
        assert_eq!(
            l,
            vec!["H:local", "P:/p1", "S:p1-b", "S:p1-a", "P:/p2", "S:p2-x"]
        );
    }

    #[test]
    fn projects_order_by_their_most_recent_session() {
        let s = vec![
            session("cold", "local", "/p-cold", 1000),
            session("hot", "local", "/p-hot", 5),
        ];
        let l = layout(&s);
        assert_eq!(
            l,
            vec!["H:local", "P:/p-hot", "S:hot", "P:/p-cold", "S:cold"]
        );
    }

    #[test]
    fn host_with_no_sessions_gets_no_header() {
        let s = vec![session("only", "local", "/p", 0)];
        let l = layout(&s);
        assert!(!l.iter().any(|r| r == "H:alpenglow"));
    }

    #[test]
    fn next_session_index_skips_both_kinds_of_headers() {
        let s = vec![
            session("a", "alpenglow", "/p", 0),
            session("b", "local", "/p", 0),
        ];
        let rows = build_display_rows(&s);
        // Layout: H:local(0) P:/p(1) S:b(2) H:alpenglow(3) P:/p(4) S:a(5)
        assert_eq!(next_session_index(Some(2), &rows), Some(5));
        assert_eq!(next_session_index(Some(5), &rows), Some(2));
    }

    #[test]
    fn prev_session_index_skips_both_kinds_of_headers() {
        let s = vec![
            session("a", "alpenglow", "/p", 0),
            session("b", "local", "/p", 0),
        ];
        let rows = build_display_rows(&s);
        assert_eq!(prev_session_index(Some(2), &rows), Some(5));
        assert_eq!(prev_session_index(Some(5), &rows), Some(2));
    }

    #[test]
    fn next_session_index_with_no_current_starts_at_first_session() {
        let s = vec![session("a", "local", "/p", 0)];
        let rows = build_display_rows(&s);
        // Layout: H:local(0) P:/p(1) S:a(2)
        assert_eq!(next_session_index(None, &rows), Some(2));
    }

    #[test]
    fn next_session_index_returns_none_when_no_sessions_at_all() {
        let rows = build_display_rows(&[]);
        assert_eq!(next_session_index(None, &rows), None);
        assert_eq!(next_session_index(Some(0), &rows), None);
    }

    #[test]
    fn first_session_index_returns_index_of_first_session_row() {
        let s = vec![
            session("a", "alpenglow", "/p", 0),
            session("b", "local", "/p", 0),
        ];
        let rows = build_display_rows(&s);
        // Layout: H:local(0) P:/p(1) S:b(2) H:alpenglow(3) P:/p(4) S:a(5)
        assert_eq!(first_session_index(&rows), Some(2));
    }

    #[test]
    fn first_session_index_returns_none_for_header_only_input() {
        let rows = vec![DisplayRow::HostHeader(HostId("solo".into()))];
        assert_eq!(first_session_index(&rows), None);
    }

    // ------- matches_query -------

    #[test]
    fn matches_query_empty_matches_every_session() {
        let s = session_with_title("a", "local", "/p", 0, Some("anything"));
        assert!(matches_query(&s, ""));
    }

    #[test]
    fn matches_query_finds_substring_of_title_case_insensitively() {
        // The match itself is case-insensitive on the *title* — the
        // contract is that the caller has already lowercased the query.
        let s = session_with_title("a", "local", "/p", 0, Some("Refactor Parser"));
        assert!(matches_query(&s, "parser"));
        assert!(matches_query(&s, "refactor"));
        assert!(!matches_query(&s, "missing"));
    }

    #[test]
    fn matches_query_finds_substring_of_project_dir() {
        let s = session_with_title("a", "local", "/home/me/Agent-MUX", 0, None);
        assert!(matches_query(&s, "agent"));
        assert!(matches_query(&s, "agent-mux"));
    }

    #[test]
    fn matches_query_finds_substring_of_host_label() {
        let s = session_with_title("a", "Alpenglow", "/p", 0, None);
        assert!(matches_query(&s, "alpen"));
        assert!(matches_query(&s, "glow"));
    }

    #[test]
    fn matches_query_returns_false_when_no_field_contains_query() {
        let s = session_with_title("a", "local", "/p", 0, Some("hello"));
        assert!(!matches_query(&s, "xyz"));
    }

    #[test]
    fn matches_query_ignores_session_id() {
        // The id is not surfaced as a search target — the user doesn't
        // see opaque uuid fragments by default.
        let s = session_with_title("deadbeef", "local", "/p", 0, Some("hi"));
        assert!(!matches_query(&s, "deadbeef"));
    }

    // ------- build_display_rows_filtered -------

    fn filtered_layout(sessions: &[Session], query: &str) -> Vec<String> {
        let q = query.to_lowercase();
        build_display_rows_filtered(sessions, |i| matches_query(&sessions[i], &q))
            .into_iter()
            .map(|r| match r {
                DisplayRow::HostHeader(host) => format!("H:{host}"),
                DisplayRow::ProjectHeader(path) => format!("P:{}", path.display()),
                DisplayRow::SessionRow(i) => format!("S:{}", sessions[i].id.0),
            })
            .collect()
    }

    #[test]
    fn filtered_rows_empty_query_matches_unfiltered_layout() {
        let s = vec![
            session("a", "alpenglow", "/p", 0),
            session("b", "local", "/p", 0),
        ];
        assert_eq!(filtered_layout(&s, ""), layout(&s));
    }

    #[test]
    fn filtered_rows_drops_host_header_when_all_its_sessions_fail_filter() {
        // alpenglow has only one session and it doesn't match — its
        // host header must collapse with it.
        let s = vec![
            session_with_title("a", "alpenglow", "/cold", 0, Some("cold task")),
            session_with_title("b", "local", "/hot", 0, Some("hot task")),
        ];
        let l = filtered_layout(&s, "hot");
        assert_eq!(l, vec!["H:local", "P:/hot", "S:b"]);
    }

    #[test]
    fn filtered_rows_drops_project_header_when_all_its_sessions_fail_filter() {
        // Within local: /cold has no matching session, /hot has one.
        let s = vec![
            session_with_title("a", "local", "/cold", 0, Some("cold task")),
            session_with_title("b", "local", "/hot", 0, Some("hot task")),
        ];
        let l = filtered_layout(&s, "hot");
        assert_eq!(l, vec!["H:local", "P:/hot", "S:b"]);
    }

    #[test]
    fn filtered_rows_empty_result_when_nothing_matches() {
        let s = vec![session_with_title("a", "local", "/p", 0, Some("hi"))];
        assert!(filtered_layout(&s, "nope").is_empty());
    }

    #[test]
    fn filtered_session_row_indices_still_point_at_original_slice() {
        // The contract: SessionRow(i) is an index into the *original*
        // sessions slice, never a re-indexed filtered position. Here
        // session at index 1 has the matching title, so the only
        // emitted row should be `SessionRow(1)`.
        let s = vec![
            session_with_title("a", "local", "/p", 0, Some("skip me")),
            session_with_title("b", "local", "/p", 1, Some("keep me")),
        ];
        let q = "keep";
        let rows = build_display_rows_filtered(&s, |i| matches_query(&s[i], q));
        let session_rows: Vec<usize> = rows
            .iter()
            .filter_map(|r| match r {
                DisplayRow::SessionRow(i) => Some(*i),
                _ => None,
            })
            .collect();
        assert_eq!(session_rows, vec![1]);
    }

    // ------- SearchState -------

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn search_state_starts_empty_in_editing_mode() {
        let s = SearchState::new();
        assert_eq!(s.query, "");
        assert_eq!(s.mode, SearchMode::Editing);
    }

    #[test]
    fn search_state_char_keys_append_to_query() {
        let mut s = SearchState::new();
        assert_eq!(
            s.handle_editing_key(key(KeyCode::Char('h'))),
            SearchOutcome::Edited
        );
        s.handle_editing_key(key(KeyCode::Char('i')));
        assert_eq!(s.query, "hi");
        assert_eq!(s.mode, SearchMode::Editing);
    }

    #[test]
    fn search_state_backspace_pops_last_char() {
        let mut s = SearchState::new();
        s.handle_editing_key(key(KeyCode::Char('h')));
        s.handle_editing_key(key(KeyCode::Char('i')));
        s.handle_editing_key(key(KeyCode::Backspace));
        assert_eq!(s.query, "h");
    }

    #[test]
    fn search_state_backspace_on_empty_query_is_noop() {
        let mut s = SearchState::new();
        let _ = s.handle_editing_key(key(KeyCode::Backspace));
        assert_eq!(s.query, "");
    }

    #[test]
    fn search_state_enter_commits_to_active_mode() {
        let mut s = SearchState::new();
        s.handle_editing_key(key(KeyCode::Char('x')));
        assert_eq!(
            s.handle_editing_key(key(KeyCode::Enter)),
            SearchOutcome::Commit
        );
        assert_eq!(s.mode, SearchMode::Active);
        assert_eq!(s.query, "x"); // commit preserves the query
    }

    #[test]
    fn search_state_esc_signals_exit() {
        let mut s = SearchState::new();
        s.handle_editing_key(key(KeyCode::Char('x')));
        assert_eq!(s.handle_editing_key(key(KeyCode::Esc)), SearchOutcome::Exit);
    }

    #[test]
    fn search_state_ignores_chars_with_control_modifiers() {
        // Ctrl-C must not be consumed by the search input — it has to
        // fall through to the main loop's unconditional-quit handler.
        let mut s = SearchState::new();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        s.handle_editing_key(ctrl_c);
        assert_eq!(s.query, "");
    }

    #[test]
    fn search_state_accepts_shifted_chars() {
        // Capital letters arrive with KeyModifiers::SHIFT; those must
        // still append (a user typing "Foo" shouldn't be ignored).
        let mut s = SearchState::new();
        let shift_f = KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT);
        s.handle_editing_key(shift_f);
        assert_eq!(s.query, "F");
    }
}
