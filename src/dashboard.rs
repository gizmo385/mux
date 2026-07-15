use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::SystemTime;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Style;

use crate::session::{HostId, Session, SessionId};

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
    /// Tail row of a project group whose session count exceeded the
    /// per-project cap: carries how many sessions are hidden, rendered
    /// as a dim `+ K more`. Non-selectable — the hidden (older)
    /// sessions are reached via search (which lifts the cap) or by
    /// favoriting. See `[ui] sessions_per_project`.
    ProjectOverflow(usize),
    /// Header for the "Tools" group surfaced at the top of the
    /// sidebar when one or more `[[tools]]` launches are currently
    /// running. Omitted when `ToolLaunchRegistry::is_empty()`.
    ToolsHeader,
    /// Index into `ToolLaunchRegistry::launches()`. Enter on a tool
    /// row re-attaches the embedded pane to that tool's tmux session.
    ToolRow(usize),
    /// Header for the user-pinned favorites group surfaced at the very
    /// top of the sidebar when `FavoritesStore` is non-empty. Omitted
    /// otherwise so an empty set doesn't produce a bare header.
    FavoritesHeader,
    /// Index into the `sessions` slice for a row that appears inside
    /// the pinned favorites group. Distinct from [`Self::SessionRow`]
    /// so the same session id can render twice (favorited sessions
    /// also still appear in their natural host/project group) without
    /// the re-seat logic collapsing the two copies — `main.rs` tracks
    /// which copy the user is on via `(SessionId, in_favorites: bool)`
    /// and prefers the matching kind on re-seat.
    FavoriteSessionRow(usize),
    /// Index into the `placeholders` slice for a favorited session that
    /// isn't in the live catalog yet — rendered as a dimmed
    /// "unconfirmed" row inside the favorites group so a favorite
    /// doesn't vanish while its host is still connecting (or is
    /// offline). Replaced by a [`Self::FavoriteSessionRow`] once
    /// discovery surfaces the real session. See [`FavoritePlaceholder`].
    FavoritePlaceholderRow(usize),
}

/// A favorited session that isn't currently present in the live
/// catalog. Built from the `FavoritesStore`'s cached metadata so the
/// pinned favorites group can render the row even before (or without)
/// the live session — decoupling favorites visibility from catalog
/// presence. `main.rs` recomputes the placeholder list each frame from
/// the store; the index in [`DisplayRow::FavoritePlaceholderRow`] is
/// into that per-frame slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FavoritePlaceholder {
    pub host: HostId,
    pub id: SessionId,
    pub title: Option<String>,
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
    build_display_rows_filtered(sessions, &[], None, |_| true, |_| false, |_| String::new())
}

/// Like [`build_display_rows`] but emits rows only for sessions where
/// `include(i)` returns true, and prepends a pinned favorites group
/// for sessions where `is_favorite(i)` returns true (recency desc
/// across all favorited sessions, irrespective of host/project).
///
/// Host and project headers in the natural grouping are omitted when
/// none of their children survive the filter — the grouping collapses
/// naturally rather than leaving orphaned headers. Same rule for the
/// favorites group: an empty favorited set produces no
/// `FavoritesHeader` row, not an empty group.
///
/// A favorited session appears *twice* in the output (once as
/// [`DisplayRow::FavoriteSessionRow`] in the pinned group, once as
/// [`DisplayRow::SessionRow`] in its natural host/project group).
/// This is deliberate: neither view loses information, and the
/// per-project completeness view stays whole. The selection-tracking
/// in `main.rs` disambiguates the two copies via
/// `(SessionId, in_favorites: bool)`.
///
/// `SessionRow(i)` / `FavoriteSessionRow(i)` indices still point into
/// the original `sessions` slice, so the dashboard can resolve a row
/// back to its session without holding a parallel filtered slice.
///
/// `is_favorite` is also filtered through `include` — a favorited
/// session that doesn't match the current search query is hidden from
/// the favorites group too. Asymmetric "favorites ignore search"
/// surprises users more than it helps; the favorites group simply
/// shrinks (possibly to empty, taking its header with it) under a
/// narrow query.
///
/// `placeholders` are favorited sessions with no live catalog entry
/// (host still connecting, or offline); each becomes a
/// [`DisplayRow::FavoritePlaceholderRow`] appended after the live
/// favorite rows. They are passed in already search-filtered (the
/// caller owns the placeholder list), so the group header appears when
/// *either* a live favorite or a placeholder survives.
///
/// `cap` limits each project group (in the natural host → project tree
/// only — not favorites) to its `n` most-recent sessions, appending a
/// [`DisplayRow::ProjectOverflow`] carrying the hidden count. `None`
/// shows every session. The caller passes `None` while a search is
/// active (every match stays visible) and normalises a configured `0`
/// to `None`, so `n` here is always ≥ 1.
#[must_use]
pub fn build_display_rows_filtered<F, G, H>(
    sessions: &[Session],
    placeholders: &[FavoritePlaceholder],
    cap: Option<usize>,
    include: F,
    is_favorite: G,
    fav_sort_key: H,
) -> Vec<DisplayRow>
where
    F: Fn(usize) -> bool,
    G: Fn(usize) -> bool,
    H: Fn(usize) -> String,
{
    let mut rows = Vec::new();

    // Pinned favorites group first. Live favorites lead (sorted
    // alphabetically by their displayed label — see below), then the
    // placeholder rows for favorites whose session isn't in the catalog
    // yet (the caller passes those pre-sorted). The `is_favorite`
    // predicate passes through `include` so the search filter applies
    // symmetrically (per the spec's resolved "favorites obey the
    // filter" decision); placeholders arrive pre-filtered.
    let mut favorite_idxs: Vec<usize> = sessions
        .iter()
        .enumerate()
        .filter(|(i, _)| include(*i) && is_favorite(*i))
        .map(|(i, _)| i)
        .collect();
    // Favorites sort alphabetically, not by recency. The favorites group
    // is the user's curated, stable set of pins; recency ordering made
    // rows jump around under the user as `last_activity` ticked on every
    // transcript update, which dogfooding flagged as a navigation hazard
    // (2026-06-25). `fav_sort_key` yields the row's displayed label
    // (rename → title → id-suffix), already lowercased, so the order
    // matches what the eye reads. `sort_by_cached_key` computes each key
    // once and is stable, so equal labels keep a deterministic order.
    favorite_idxs.sort_by_cached_key(|&i| fav_sort_key(i));
    if !favorite_idxs.is_empty() || !placeholders.is_empty() {
        rows.push(DisplayRow::FavoritesHeader);
        for i in favorite_idxs {
            rows.push(DisplayRow::FavoriteSessionRow(i));
        }
        for i in 0..placeholders.len() {
            rows.push(DisplayRow::FavoritePlaceholderRow(i));
        }
    }

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

    for host in ordered_hosts {
        rows.push(DisplayRow::HostHeader(host.clone()));
        let session_idxs = by_host.remove(&host).unwrap_or_default();

        // Worktree-backed sessions group under their parent repo so a
        // session created from `discord` and one from a `discord-<task>`
        // worktree share one project header instead of fragmenting into
        // per-worktree groups. `parent_repo` is `Some` only when the
        // session's cwd is a git worktree (see `Session.parent_repo` and
        // `worktree::parse_parent_repo`); regular checkouts and external
        // (non-git) cwds fall through to grouping by `project_dir` as
        // before. Sessions keep their actual `project_dir` — only the
        // grouping key changes.
        let mut by_project: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
        for &i in &session_idxs {
            let key = sessions[i]
                .parent_repo
                .clone()
                .unwrap_or_else(|| sessions[i].project_dir.clone());
            by_project.entry(key).or_default().push(i);
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
            // Cap to the N most-recent; the rest collapse behind a
            // `+ K more` overflow row. `cap == Some(0)` is normalised to
            // "no cap" by the caller, so `n` here is always ≥ 1.
            let total = session_idxs.len();
            let shown = match cap {
                Some(n) if n < total => n,
                _ => total,
            };
            for &i in &session_idxs[..shown] {
                rows.push(DisplayRow::SessionRow(i));
            }
            if shown < total {
                rows.push(DisplayRow::ProjectOverflow(total - shown));
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

/// Where the user's keyboard goes. `Sidebar` is the default — the
/// dashboard list owns the keys, as it has since M0. `Terminal` is
/// entered when the user attaches via the embedded PTY driver
/// (`PtyDriver`); every key flows to the embedded process *except* the
/// leader sequence (`Ctrl-a Esc`), which transitions back to `Sidebar`.
///
/// The PTY itself lives in `App.embedded`; this enum describes input
/// routing only. The invariant — `Focus::Terminal` only valid when
/// `embedded.is_some()` — is maintained by the transitions in
/// [`attach_selected`](crate::main) and the PTY-exit drain.
///
/// `leader_armed` is the state between "user pressed the leader" and
/// "user pressed the second key of the chord." A leader followed by
/// `Esc` returns to `Sidebar`; a leader followed by anything else
/// forwards both bytes to the PTY (tmux-style passthrough) and disarms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Sidebar,
    Terminal {
        leader_armed: bool,
    },
}

/// Whether the leader chord is currently armed (waiting for the second
/// key). Pure helper so the main loop's `match (focus, key)` stays
/// readable without nested pattern matches.
#[must_use]
pub fn leader_armed(focus: Focus) -> bool {
    matches!(focus, Focus::Terminal { leader_armed: true })
}

/// Whether `key` is the embedded-terminal leader. Hard-coded as
/// `Ctrl-a` for the Phase-3 ship; M5 will surface this in config so
/// users can dodge collisions with their tmux prefix.
///
/// The check is strict-equal on `KeyModifiers::CONTROL` — `Ctrl-Shift-a`
/// is *not* the leader, because tmux-style users expect a clean two-
/// key chord without an accidental Shift triggering it.
#[must_use]
pub fn is_pty_leader(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('a') && key.modifiers == KeyModifiers::CONTROL
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
        if is_selectable(&rows[idx]) {
            return Some(idx);
        }
    }
    None
}

/// Whether `row` accepts selection / Enter dispatch. Today: sessions
/// (in either the pinned favorites group or the natural host/project
/// group), favorite placeholders (so the user can land on one to
/// dismiss it with the favorite keybind), and tool launches. Headers
/// (host, project, tools, favorites) are skipped by j/k navigation so
/// the cursor never lands on a non-actionable line.
#[must_use]
fn is_selectable(row: &DisplayRow) -> bool {
    matches!(
        row,
        DisplayRow::SessionRow(_)
            | DisplayRow::FavoriteSessionRow(_)
            | DisplayRow::FavoritePlaceholderRow(_)
            | DisplayRow::ToolRow(_)
    )
}

/// First session-bearing row index in `rows` (either a
/// `FavoriteSessionRow` or a `SessionRow`), or `None` if there are
/// none. Used to seed selection when the catalog goes from empty to
/// non-empty. Tool rows are not eligible — the dashboard's default
/// selection should land on real work, not on a transient
/// `[[tools]]` launch. When the favorites group is non-empty, this
/// lands on the first favorite (favorites render first in the row
/// list); otherwise the first natural session row.
#[must_use]
pub fn first_session_index(rows: &[DisplayRow]) -> Option<usize> {
    rows.iter().position(|r| {
        matches!(
            r,
            DisplayRow::SessionRow(_) | DisplayRow::FavoriteSessionRow(_)
        )
    })
}

/// First `SessionRow` index belonging to the project group the current
/// selection sits *under* — i.e., the next project's first session,
/// wrapping at the end. Returns `None` when there's no current selection,
/// no enclosing project, or there's only one project group in `rows`
/// (so there's nowhere distinct to jump to).
#[must_use]
pub fn next_project_index(current: Option<usize>, rows: &[DisplayRow]) -> Option<usize> {
    walk_to_group(current, rows, 1, is_project_header)
}

/// First `SessionRow` index of the previous project group, wrapping at
/// the start. Same semantics as [`next_project_index`] in reverse — lands
/// on the first session under the previous project header, not the last,
/// so repeated `K` presses page through projects deterministically from
/// their first session.
#[must_use]
pub fn prev_project_index(current: Option<usize>, rows: &[DisplayRow]) -> Option<usize> {
    walk_to_group(current, rows, -1, is_project_header)
}

/// First selectable-row index belonging to the next top-level *section*
/// — the Tools group, the Favorites group, or a host group — wrapping at
/// the end. The coarsest of the three jump granularities (`j`/`k` =
/// session, `J`/`K` = project, `⌃j`/`⌃k` = section). Lands on the
/// section's first selectable row: a tool launch, a favorite (or offline
/// placeholder), or the first session under the host. Returns `None`
/// when only one section is on screen (nowhere distinct to go).
#[must_use]
pub fn next_section_index(current: Option<usize>, rows: &[DisplayRow]) -> Option<usize> {
    walk_to_group(current, rows, 1, is_section_header)
}

/// First selectable-row index of the previous top-level section, wrapping
/// at the start. Mirror of [`next_section_index`].
#[must_use]
pub fn prev_section_index(current: Option<usize>, rows: &[DisplayRow]) -> Option<usize> {
    walk_to_group(current, rows, -1, is_section_header)
}

fn is_project_header(row: &DisplayRow) -> bool {
    matches!(row, DisplayRow::ProjectHeader(_))
}

/// Index of the closest preceding "context" header for the row at
/// `selected` — the header the row visually sits under. Used by the
/// draw loop to pin that header on screen as the user scrolls within a
/// long group; ratatui's `List` only adjusts the offset enough to keep
/// the *selected* row visible, so without this nudge the group's
/// header scrolls off and stays off until the cursor leaves the group.
///
/// Returns `None` when `selected` is itself a header, when no matching
/// header precedes it, or when `selected` is out of bounds. Only the
/// closest preceding `ProjectHeader` is returned for sessions — the
/// `HostHeader` further up is intentionally not pinned, since pinning
/// both would burn two sidebar rows on context and the user's reported
/// loss was specifically the project name.
#[must_use]
pub fn anchor_header_for_selection(rows: &[DisplayRow], selected: usize) -> Option<usize> {
    let row = rows.get(selected)?;
    let predicate: fn(&DisplayRow) -> bool = match row {
        DisplayRow::SessionRow(_) => |r| matches!(r, DisplayRow::ProjectHeader(_)),
        DisplayRow::FavoriteSessionRow(_) | DisplayRow::FavoritePlaceholderRow(_) => {
            |r| matches!(r, DisplayRow::FavoritesHeader)
        }
        DisplayRow::ToolRow(_) => |r| matches!(r, DisplayRow::ToolsHeader),
        DisplayRow::HostHeader(_)
        | DisplayRow::ProjectHeader(_)
        | DisplayRow::ProjectOverflow(_)
        | DisplayRow::ToolsHeader
        | DisplayRow::FavoritesHeader => return None,
    };
    rows[..selected].iter().rposition(predicate)
}

/// A top-level section anchor: the Tools group header, the Favorites
/// group header, or a host header. These are the groups `⌃j`/`⌃k` cycle
/// through. Favorites and Tools render *above* the host→project tree, so
/// without treating their headers as section anchors they were
/// unreachable by explicit group-jump — only by `j`/`k` paging or the
/// quickswitcher.
fn is_section_header(row: &DisplayRow) -> bool {
    matches!(
        row,
        DisplayRow::HostHeader(_) | DisplayRow::FavoritesHeader | DisplayRow::ToolsHeader
    )
}

/// Shared engine for the four group-jump helpers. Identifies the group
/// header (`is_header`) the current selection sits under, finds the
/// next or previous header of the same kind (wrapping), then returns
/// the first selectable-row index after that header (a session, favorite,
/// or tool row, depending on the section).
///
/// Returns `None` when there's no current selection, when the current
/// row has no preceding header (malformed row list), or when there are
/// fewer than two groups of this kind (no distinct destination).
fn walk_to_group(
    current: Option<usize>,
    rows: &[DisplayRow],
    step: isize,
    is_header: impl Fn(&DisplayRow) -> bool,
) -> Option<usize> {
    let cur = current?;
    if rows.is_empty() {
        return None;
    }
    let cur_group = (0..=cur).rev().find(|&i| is_header(&rows[i]))?;
    let groups: Vec<usize> = (0..rows.len()).filter(|&i| is_header(&rows[i])).collect();
    if groups.len() < 2 {
        return None;
    }
    let cur_pos = groups.iter().position(|&i| i == cur_group)?;
    let next_pos = if step > 0 {
        (cur_pos + 1) % groups.len()
    } else {
        (cur_pos + groups.len() - 1) % groups.len()
    };
    let target_header = groups[next_pos];
    (target_header + 1..rows.len()).find(|&i| is_selectable(&rows[i]))
}

/// Apply `colour` as a foreground if `Some`; leave the style untouched
/// otherwise. Keeps the `Theme::field: Option<Color>` "absent means
/// inherit terminal default" semantics centralised — render paths just
/// call this and don't branch.
#[must_use]
pub fn apply_fg(style: Style, colour: Option<ratatui::style::Color>) -> Style {
    match colour {
        Some(c) => style.fg(c),
        None => style,
    }
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
        session_with_parent(id, host, project, None, seconds_ago, title)
    }

    fn session_with_parent(
        id: &str,
        host: &str,
        project: &str,
        parent_repo: Option<&str>,
        seconds_ago: u64,
        title: Option<&str>,
    ) -> Session {
        Session {
            id: SessionId(id.to_string()),
            host: HostId(host.to_string()),
            agent: crate::agent::AgentKind::Claude,
            project_dir: PathBuf::from(project),
            transcript_path: PathBuf::from(format!("/t/{id}.jsonl")),
            last_activity: SystemTime::UNIX_EPOCH + Duration::from_secs(10_000 - seconds_ago),
            attention: Attention::Unknown,
            title: title.map(str::to_string),
            parent_repo: parent_repo.map(PathBuf::from),
            has_live_pane: None,
            hook_pinned: None,
            blocking_prompt: false,
            attention_entered_at: None,
            started_at: None,
            edited_files: Vec::new(),
        }
    }

    /// Helper: stringify the row layout as a flat Vec for assertions —
    /// host headers as `"H:<id>"`, project headers as `"P:<path>"`,
    /// session rows as `"S:<id>"`, favorites header as `"FH"`,
    /// favorite session rows as `"F:<id>"`.
    fn layout(sessions: &[Session]) -> Vec<String> {
        layout_with_favorites(sessions, |_| false)
    }

    /// Variant of [`layout`] that exercises the favorites group via
    /// a per-test predicate. Plain [`layout`] passes `|_| false` so
    /// the favorites section never appears, keeping the pre-favorites
    /// expected-output assertions unchanged.
    /// Sort key a favorite row uses in tests: its displayed label
    /// (title, else session id), lowercased — mirroring the production
    /// `favorite_sort_key` without the rename-override layer (which the
    /// pure builder never sees).
    fn fav_key_for_test(s: &Session) -> String {
        s.title
            .clone()
            .unwrap_or_else(|| s.id.0.clone())
            .to_lowercase()
    }

    fn layout_with_favorites<G>(sessions: &[Session], is_favorite: G) -> Vec<String>
    where
        G: Fn(usize) -> bool,
    {
        build_display_rows_filtered(
            sessions,
            &[],
            None,
            |_| true,
            is_favorite,
            |i| fav_key_for_test(&sessions[i]),
        )
        .into_iter()
        .map(|r| match r {
            DisplayRow::HostHeader(host) => format!("H:{host}"),
            DisplayRow::ProjectHeader(path) => format!("P:{}", path.display()),
            DisplayRow::SessionRow(i) => format!("S:{}", sessions[i].id.0),
            DisplayRow::ToolsHeader => "TH:tools".to_string(),
            DisplayRow::ToolRow(i) => format!("T:{i}"),
            DisplayRow::FavoritesHeader => "FH".to_string(),
            DisplayRow::FavoriteSessionRow(i) => format!("F:{}", sessions[i].id.0),
            DisplayRow::FavoritePlaceholderRow(i) => format!("FP:{i}"),
            DisplayRow::ProjectOverflow(n) => format!("OV:{n}"),
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
    fn worktree_session_groups_under_parent_repo_header() {
        // Regression for the dogfooded issue: a session in `/discord`
        // and a worktree-backed session in `/discord-fix-bug` (parent
        // = `/discord`) used to land under two separate project
        // headers. They must share one header keyed by the parent
        // repo path, with the worktree session still pointing at its
        // own `project_dir`.
        let s = vec![
            session_with_parent("plain", "local", "/discord", None, 100, None),
            session_with_parent(
                "worktree",
                "local",
                "/discord-fix-bug",
                Some("/discord"),
                10,
                None,
            ),
        ];
        let l = layout(&s);
        assert_eq!(
            l,
            vec!["H:local", "P:/discord", "S:worktree", "S:plain"],
            "worktree should fold into /discord's group, not get its own header"
        );
    }

    #[test]
    fn two_worktrees_of_the_same_repo_share_one_header() {
        // Two worktrees of `/discord`, no plain-checkout session, must
        // still collapse to a single `/discord` project header.
        let s = vec![
            session_with_parent(
                "wt1",
                "local",
                "/discord-fix-bug",
                Some("/discord"),
                100,
                None,
            ),
            session_with_parent(
                "wt2",
                "local",
                "/discord-refactor",
                Some("/discord"),
                10,
                None,
            ),
        ];
        let l = layout(&s);
        assert_eq!(l, vec!["H:local", "P:/discord", "S:wt2", "S:wt1"]);
    }

    #[test]
    fn sessions_without_parent_repo_still_group_by_project_dir() {
        // External (non-git) sessions and plain-checkout sessions
        // — `parent_repo` is `None` for both — must fall through to
        // the legacy behaviour: group by `project_dir`. Pins that
        // unifying worktrees doesn't accidentally regress how
        // external sessions display.
        let s = vec![
            session_with_parent("a", "local", "/scratch", None, 100, None),
            session_with_parent("b", "local", "/scratch", None, 10, None),
            session_with_parent("c", "local", "/notes", None, 50, None),
        ];
        let l = layout(&s);
        assert_eq!(
            l,
            vec!["H:local", "P:/scratch", "S:b", "S:a", "P:/notes", "S:c"]
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
    fn favorites_section_appears_above_host_groups_when_non_empty() {
        // Two sessions across two projects; favorite the older one.
        // The pinned favorites section comes first, then the natural
        // host/project tree (which still contains both sessions —
        // favorites duplicate, they don't relocate).
        let s = vec![
            session("a", "local", "/p1", 100),
            session("b", "local", "/p2", 5),
        ];
        let favs = std::collections::HashSet::from([0usize]);
        let l = layout_with_favorites(&s, |i| favs.contains(&i));
        assert_eq!(
            l,
            vec![
                "FH", "F:a", // pinned at the top
                "H:local", "P:/p2", "S:b", "P:/p1", "S:a", // natural tree, recency-ordered
            ]
        );
    }

    #[test]
    fn favorites_section_omitted_when_no_favorites_set() {
        // The pinned-group header must not appear with an empty set
        // — otherwise the user sees `── favorites ──` followed by
        // nothing, which reads as visual noise.
        let s = vec![session("a", "local", "/p", 0)];
        let l = layout_with_favorites(&s, |_| false);
        assert!(!l.iter().any(|r| r == "FH"));
        assert!(!l.iter().any(|r| r.starts_with("F:")));
    }

    #[test]
    fn favorites_section_orders_alphabetically_across_hosts_and_projects() {
        // Three favorited sessions spanning two hosts and three
        // projects must list alphabetically by their displayed label
        // inside the pinned group, regardless of their natural
        // host/project grouping *and* regardless of recency — the
        // curated favorites set stays put under the user rather than
        // reshuffling as `last_activity` ticks (2026-06-25 dogfood).
        // These are title-less, so the label is the session id: the
        // recency order (new, mid, old) must NOT survive; "mid" sorts
        // first alphabetically despite being the middle by recency.
        let s = vec![
            session("old", "local", "/p1", 1000),
            session("new", "alpenglow", "/p3", 5),
            session("mid", "local", "/p2", 100),
        ];
        let l = layout_with_favorites(&s, |_| true);
        // Strip everything after the natural-tree start so the
        // assertion focuses on favorites ordering only.
        let head: Vec<_> = l.iter().take_while(|r| !r.starts_with("H:")).collect();
        assert_eq!(head, vec!["FH", "F:mid", "F:new", "F:old"]);
    }

    #[test]
    fn favorites_sort_by_title_when_present_not_id() {
        // When favorites carry titles, the alphabetical order follows
        // the *title* (what the row displays), not the underlying id.
        // ids are deliberately in the opposite order to the titles.
        let s = vec![
            session_with_title("zzz", "local", "/p", 0, Some("Apple")),
            session_with_title("aaa", "local", "/p", 0, Some("Zebra")),
            session_with_title("mmm", "local", "/p", 0, Some("Mango")),
        ];
        let l = layout_with_favorites(&s, |_| true);
        let head: Vec<_> = l.iter().take_while(|r| !r.starts_with("H:")).collect();
        assert_eq!(head, vec!["FH", "F:zzz", "F:mmm", "F:aaa"]);
    }

    #[test]
    fn favorites_obey_the_search_filter() {
        // Resolved spec decision: favorites also disappear when the
        // search query excludes them — asymmetric "favorites always
        // show" surprises users more than it helps. Here only "keep"
        // matches the query, and it's the only thing that should
        // appear in the favorites group too (despite both being
        // favorited).
        let s = vec![
            session_with_title("a", "local", "/p", 0, Some("skip me")),
            session_with_title("b", "local", "/p", 5, Some("keep me")),
        ];
        let q = "keep";
        let rows = build_display_rows_filtered(
            &s,
            &[],
            None,
            |i| matches_query(&s[i], q),
            |_| true, // both favorited
            |i| fav_key_for_test(&s[i]),
        );
        let labels: Vec<_> = rows
            .iter()
            .filter_map(|r| match r {
                DisplayRow::FavoriteSessionRow(i) => Some(s[*i].id.0.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, vec!["b"], "filtered-out favorite must not appear");
    }

    #[test]
    fn favorite_session_row_indices_still_point_at_original_slice() {
        // Symmetric contract to SessionRow: FavoriteSessionRow(i) is
        // an index into the original sessions slice, never a
        // re-indexed favorites-only position.
        let s = vec![
            session("a", "local", "/p", 0),
            session("b", "local", "/p", 5),
            session("c", "local", "/p", 10),
        ];
        let favs = std::collections::HashSet::from([1usize]);
        let rows = build_display_rows_filtered(
            &s,
            &[],
            None,
            |_| true,
            |i| favs.contains(&i),
            |i| fav_key_for_test(&s[i]),
        );
        let favorite_rows: Vec<usize> = rows
            .iter()
            .filter_map(|r| match r {
                DisplayRow::FavoriteSessionRow(i) => Some(*i),
                _ => None,
            })
            .collect();
        assert_eq!(favorite_rows, vec![1]);
    }

    #[test]
    fn favorites_group_appends_placeholders_after_live_favorites() {
        // A favorited session that's live renders as a normal favorite
        // row; a favorited session with no live catalog entry renders
        // as a placeholder appended after it — so neither vanishes.
        let s = vec![session("a", "local", "/p", 0)];
        let placeholders = vec![FavoritePlaceholder {
            host: HostId("alpenglow".into()),
            id: SessionId("gone".into()),
            title: Some("Remote work".into()),
        }];
        let favs = std::collections::HashSet::from([0usize]);
        let rows = build_display_rows_filtered(
            &s,
            &placeholders,
            None,
            |_| true,
            |i| favs.contains(&i),
            |i| fav_key_for_test(&s[i]),
        );
        assert_eq!(rows[0], DisplayRow::FavoritesHeader);
        assert_eq!(rows[1], DisplayRow::FavoriteSessionRow(0));
        assert_eq!(rows[2], DisplayRow::FavoritePlaceholderRow(0));
    }

    #[test]
    fn favorites_header_appears_when_only_placeholders_exist() {
        // No live favorites at all, just one offline favorite. The
        // group (header + placeholder) must still render rather than
        // letting the favorite disappear until its host reconnects.
        let s = vec![session("a", "local", "/p", 0)];
        let placeholders = vec![FavoritePlaceholder {
            host: HostId("alpenglow".into()),
            id: SessionId("gone".into()),
            title: None,
        }];
        let rows = build_display_rows_filtered(
            &s,
            &placeholders,
            None,
            |_| true,
            |_| false,
            |_| String::new(),
        );
        assert_eq!(rows[0], DisplayRow::FavoritesHeader);
        assert_eq!(rows[1], DisplayRow::FavoritePlaceholderRow(0));
    }

    #[test]
    fn no_favorites_and_no_placeholders_produces_no_favorites_header() {
        // Guard the empty case: an empty favorites set and no
        // placeholders must not emit a bare `── favorites ──` header.
        let s = vec![session("a", "local", "/p", 0)];
        let rows =
            build_display_rows_filtered(&s, &[], None, |_| true, |_| false, |_| String::new());
        assert!(!rows.contains(&DisplayRow::FavoritesHeader));
    }

    #[test]
    fn project_cap_truncates_to_most_recent_and_emits_overflow() {
        // Four sessions in one project, cap 2 → the two most-recent
        // SessionRows plus a `ProjectOverflow(2)` for the hidden rest.
        // (4th arg is "seconds ago": smaller = more recent.)
        let s = vec![
            session("a", "local", "/p", 40),
            session("b", "local", "/p", 30),
            session("c", "local", "/p", 20),
            session("d", "local", "/p", 10),
        ];
        let rows =
            build_display_rows_filtered(&s, &[], Some(2), |_| true, |_| false, |_| String::new());
        let shown: Vec<usize> = rows
            .iter()
            .filter_map(|r| match r {
                DisplayRow::SessionRow(i) => Some(*i),
                _ => None,
            })
            .collect();
        assert_eq!(shown, vec![3, 2], "the two most-recent (d, c) survive");
        assert!(
            rows.contains(&DisplayRow::ProjectOverflow(2)),
            "the other two collapse behind +2 more"
        );
    }

    #[test]
    fn project_cap_none_shows_all_without_overflow() {
        let s = vec![
            session("a", "local", "/p", 30),
            session("b", "local", "/p", 20),
            session("c", "local", "/p", 10),
        ];
        let rows =
            build_display_rows_filtered(&s, &[], None, |_| true, |_| false, |_| String::new());
        assert_eq!(
            rows.iter()
                .filter(|r| matches!(r, DisplayRow::SessionRow(_)))
                .count(),
            3
        );
        assert!(
            !rows
                .iter()
                .any(|r| matches!(r, DisplayRow::ProjectOverflow(_)))
        );
    }

    #[test]
    fn project_cap_at_or_above_count_emits_no_overflow() {
        let s = vec![
            session("a", "local", "/p", 20),
            session("b", "local", "/p", 10),
        ];
        let rows =
            build_display_rows_filtered(&s, &[], Some(5), |_| true, |_| false, |_| String::new());
        assert!(
            !rows
                .iter()
                .any(|r| matches!(r, DisplayRow::ProjectOverflow(_))),
            "cap ≥ count must not emit an overflow row"
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

    #[test]
    fn anchor_header_for_session_returns_closest_preceding_project_header() {
        let s = vec![
            session("a", "local", "/p1", 0),
            session("b", "local", "/p2", 1),
        ];
        let rows = build_display_rows(&s);
        // Layout: H:local(0) P:/p1(1) S:a(2) P:/p2(3) S:b(4)
        // — both sessions anchor to their own project header.
        assert_eq!(anchor_header_for_selection(&rows, 2), Some(1));
        assert_eq!(anchor_header_for_selection(&rows, 4), Some(3));
    }

    #[test]
    fn anchor_header_returns_none_for_headers_themselves() {
        let s = vec![session("a", "local", "/p", 0)];
        let rows = build_display_rows(&s);
        // Layout: H:local(0) P:/p(1) S:a(2)
        assert_eq!(anchor_header_for_selection(&rows, 0), None);
        assert_eq!(anchor_header_for_selection(&rows, 1), None);
    }

    #[test]
    fn anchor_header_for_favorite_session_returns_favorites_header() {
        let s = vec![session("a", "local", "/p", 0)];
        let rows = build_display_rows_filtered(
            &s,
            &[],
            None,
            |_| true,
            |_| true,
            |i| fav_key_for_test(&s[i]),
        );
        // Layout: FH(0) F:a(1) H:local(2) P:/p(3) S:a(4)
        assert_eq!(anchor_header_for_selection(&rows, 1), Some(0));
        // The natural-group SessionRow still anchors to its ProjectHeader.
        assert_eq!(anchor_header_for_selection(&rows, 4), Some(3));
    }

    #[test]
    fn anchor_header_out_of_bounds_is_none() {
        let rows: Vec<DisplayRow> = vec![];
        assert_eq!(anchor_header_for_selection(&rows, 0), None);
    }

    // ------- next/prev_project_index, next/prev_section_index -------

    #[test]
    fn next_project_index_jumps_to_first_session_of_next_project() {
        let s = vec![
            session("a", "local", "/x", 0),
            session("b", "local", "/y", 0),
            session("c", "local", "/y", 0),
        ];
        let rows = build_display_rows(&s);
        // Layout: H:local(0) P:/x(1) S:a(2) P:/y(3) S:b(4) S:c(5)
        assert_eq!(next_project_index(Some(2), &rows), Some(4));
        // From the middle of /y, wraps back to /x's first session.
        assert_eq!(next_project_index(Some(5), &rows), Some(2));
    }

    #[test]
    fn prev_project_index_jumps_to_first_session_of_previous_project() {
        let s = vec![
            session("a", "local", "/x", 0),
            session("b", "local", "/y", 0),
            session("c", "local", "/y", 0),
        ];
        let rows = build_display_rows(&s);
        // Layout: H:local(0) P:/x(1) S:a(2) P:/y(3) S:b(4) S:c(5)
        // From either session under /y, K lands on /x's first session.
        assert_eq!(prev_project_index(Some(4), &rows), Some(2));
        assert_eq!(prev_project_index(Some(5), &rows), Some(2));
        // From /x, wraps to /y's first session.
        assert_eq!(prev_project_index(Some(2), &rows), Some(4));
    }

    #[test]
    fn project_jumps_return_none_when_only_one_project_exists() {
        let s = vec![
            session("a", "local", "/p", 0),
            session("b", "local", "/p", 0),
        ];
        let rows = build_display_rows(&s);
        assert_eq!(next_project_index(Some(2), &rows), None);
        assert_eq!(prev_project_index(Some(2), &rows), None);
    }

    #[test]
    fn next_section_index_jumps_to_first_session_of_next_host() {
        let s = vec![
            session("a", "alpenglow", "/p", 0),
            session("b", "local", "/q", 0),
        ];
        let rows = build_display_rows(&s);
        // Layout: H:local(0) P:/q(1) S:b(2) H:alpenglow(3) P:/p(4) S:a(5)
        assert_eq!(next_section_index(Some(2), &rows), Some(5));
        // From the alpenglow side, wraps to local's first session.
        assert_eq!(next_section_index(Some(5), &rows), Some(2));
    }

    #[test]
    fn prev_section_index_wraps_at_start() {
        let s = vec![
            session("a", "alpenglow", "/p", 0),
            session("b", "local", "/q", 0),
        ];
        let rows = build_display_rows(&s);
        // Symmetric with next: from local we go to alpenglow and back.
        assert_eq!(prev_section_index(Some(2), &rows), Some(5));
        assert_eq!(prev_section_index(Some(5), &rows), Some(2));
    }

    #[test]
    fn section_jumps_return_none_when_only_one_section_exists() {
        let s = vec![
            session("a", "local", "/x", 0),
            session("b", "local", "/y", 0),
        ];
        let rows = build_display_rows(&s);
        // Only `local` host and no Favorites/Tools sections, even with
        // multiple projects under it.
        assert_eq!(next_section_index(Some(2), &rows), None);
        assert_eq!(prev_section_index(Some(2), &rows), None);
    }

    #[test]
    fn section_jump_reaches_favorites_and_tools_above_the_host_tree() {
        // Favorites and Tools render above the host→project tree and were
        // unreachable by host-only group-jump. Construct the full row
        // shape `current_rows` produces (Tools, then Favorites, then the
        // host tree) and confirm `⌃j` cycles all three as sections.
        let s = [session("a", "local", "/x", 0)];
        let rows = vec![
            DisplayRow::ToolsHeader,                             // 0
            DisplayRow::ToolRow(0), // 1  <- Tools section's first selectable
            DisplayRow::FavoritesHeader, // 2
            DisplayRow::FavoriteSessionRow(0), // 3  <- Favorites' first selectable
            DisplayRow::HostHeader(s[0].host.clone()), // 4
            DisplayRow::ProjectHeader(s[0].project_dir.clone()), // 5
            DisplayRow::SessionRow(0), // 6  <- host section's first selectable
        ];
        // From the tool row: ⌃j → favorites, ⌃j → host, ⌃j → wrap to tools.
        assert_eq!(next_section_index(Some(1), &rows), Some(3));
        assert_eq!(next_section_index(Some(3), &rows), Some(6));
        assert_eq!(next_section_index(Some(6), &rows), Some(1));
        // ⌃k walks the same cycle in reverse.
        assert_eq!(prev_section_index(Some(1), &rows), Some(6));
        assert_eq!(prev_section_index(Some(6), &rows), Some(3));
        assert_eq!(prev_section_index(Some(3), &rows), Some(1));
    }

    #[test]
    fn section_jump_lands_on_offline_favorite_placeholder() {
        // A favorites section can hold only offline placeholders (no live
        // session yet). The jump must still land there — placeholders are
        // selectable and attachable-as-offline.
        let s = [session("a", "local", "/x", 0)];
        let rows = vec![
            DisplayRow::FavoritesHeader,                         // 0
            DisplayRow::FavoritePlaceholderRow(0), // 1  <- only selectable in favorites
            DisplayRow::HostHeader(s[0].host.clone()), // 2
            DisplayRow::ProjectHeader(s[0].project_dir.clone()), // 3
            DisplayRow::SessionRow(0),             // 4
        ];
        assert_eq!(next_section_index(Some(4), &rows), Some(1));
        assert_eq!(prev_section_index(Some(1), &rows), Some(4));
    }

    #[test]
    fn group_jumps_return_none_when_no_current_selection() {
        let s = vec![
            session("a", "local", "/x", 0),
            session("b", "local", "/y", 0),
        ];
        let rows = build_display_rows(&s);
        assert_eq!(next_project_index(None, &rows), None);
        assert_eq!(prev_project_index(None, &rows), None);
        assert_eq!(next_section_index(None, &rows), None);
        assert_eq!(prev_section_index(None, &rows), None);
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
        build_display_rows_filtered(
            sessions,
            &[],
            None,
            |i| matches_query(&sessions[i], &q),
            |_| false,
            |_| String::new(),
        )
        .into_iter()
        .map(|r| match r {
            DisplayRow::HostHeader(host) => format!("H:{host}"),
            DisplayRow::ProjectHeader(path) => format!("P:{}", path.display()),
            DisplayRow::SessionRow(i) => format!("S:{}", sessions[i].id.0),
            DisplayRow::ToolsHeader => "TH:tools".to_string(),
            DisplayRow::ToolRow(i) => format!("T:{i}"),
            DisplayRow::FavoritesHeader => "FH".to_string(),
            DisplayRow::FavoriteSessionRow(i) => format!("F:{}", sessions[i].id.0),
            DisplayRow::FavoritePlaceholderRow(i) => format!("FP:{i}"),
            DisplayRow::ProjectOverflow(n) => format!("OV:{n}"),
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
        let rows = build_display_rows_filtered(
            &s,
            &[],
            None,
            |i| matches_query(&s[i], q),
            |_| false,
            |_| String::new(),
        );
        let session_rows: Vec<usize> = rows
            .iter()
            .filter_map(|r| match r {
                DisplayRow::SessionRow(i) => Some(*i),
                _ => None,
            })
            .collect();
        assert_eq!(session_rows, vec![1]);
    }

    // ------- Focus -------

    #[test]
    fn focus_default_is_sidebar() {
        // The keyboard goes to the dashboard list out of the box —
        // launching agent-mux without an embedded driver must not
        // silently steal input into a non-existent terminal pane.
        assert_eq!(Focus::default(), Focus::Sidebar);
    }

    #[test]
    fn leader_armed_helper_unwraps_terminal_variant() {
        assert!(!leader_armed(Focus::Sidebar));
        assert!(!leader_armed(Focus::Terminal {
            leader_armed: false
        }));
        assert!(leader_armed(Focus::Terminal { leader_armed: true }));
    }

    #[test]
    fn is_pty_leader_matches_ctrl_a_exactly() {
        assert!(is_pty_leader(&KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL
        )));
        // Bare 'a' is not the leader — must include Ctrl.
        assert!(!is_pty_leader(&KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::empty()
        )));
        // Ctrl-b (tmux's default prefix) deliberately doesn't trigger
        // our leader — picked to avoid colliding when nesting.
        assert!(!is_pty_leader(&KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::CONTROL
        )));
    }

    #[test]
    fn is_pty_leader_rejects_ctrl_shift_a() {
        // Strict-equal on modifiers — Ctrl-Shift-a is a different chord
        // and shouldn't fire the leader, because users expect a clean
        // two-key chord without accidental Shift triggering it.
        assert!(!is_pty_leader(&KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )));
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
