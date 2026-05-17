use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::SystemTime;

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
    let mut by_host: BTreeMap<HostId, Vec<usize>> = BTreeMap::new();
    for (i, s) in sessions.iter().enumerate() {
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
        Session {
            id: SessionId(id.to_string()),
            host: HostId(host.to_string()),
            project_dir: PathBuf::from(project),
            transcript_path: PathBuf::from(format!("/t/{id}.jsonl")),
            last_activity: SystemTime::UNIX_EPOCH + Duration::from_secs(10_000 - seconds_ago),
            attention: Attention::Unknown,
            title: None,
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
}
