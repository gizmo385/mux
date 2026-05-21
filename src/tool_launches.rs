//! Tool-launch registry.
//!
//! Tracks `[[tools]]` invocations the user fires from the dashboard
//! so the embedded pane can re-attach to a running tool after swapping
//! focus to a Claude session and back. Each launch wraps its command
//! in a detached tmux session (see `PtyDriver::spawn_tool_*_embed`),
//! and the registry remembers the tmux session name so the dashboard
//! can rebuild an `EmbedSpec` that points at the same target later.
//!
//! Lifecycle is simple: the registry only grows on successful launch,
//! and entries are pruned when `tmux attach` fails (the user pressed
//! Enter on a tool row whose tmux session has since been killed).
//! There is no background poller — the discipline is "fail loudly at
//! the next interaction" rather than "discover deaths preemptively."

use std::path::PathBuf;
use std::time::SystemTime;

use crate::session::HostId;

/// One running tool launch tracked by [`ToolLaunchRegistry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolLaunch {
    /// User-facing tool name from `[[tools]]` config — the `name`
    /// field, or the program basename when `name` was omitted.
    pub name: String,
    /// Host the tmux session lives on. Re-attach builds an
    /// `EmbedSpec` whose argv is local-tmux or ssh-wrapped depending
    /// on this field.
    pub host: HostId,
    /// Tmux session name — agent-mux's only handle on the running
    /// process. `tmux attach -t <tmux_session>` is the re-attach
    /// command.
    pub tmux_session: String,
    /// The source session's `project_dir` (the cwd the tool ran in).
    /// Rendered in the sidebar row so multiple launches of the same
    /// tool against different projects don't look identical — the
    /// 2026-05-21 dogfood signal was `⚒ lazygit · ⚒ lazygit` with no
    /// way to tell them apart.
    pub project_dir: PathBuf,
    /// When the user fired the launch. Used to display "Xs ago" in
    /// the sidebar row.
    pub launched_at: SystemTime,
}

/// In-memory list of running tool launches. The dashboard reads from
/// this to render the "Tools" sidebar group; `launch_tool` writes to
/// it on successful spawn; failed re-attach prunes the dead entry.
#[derive(Debug, Default, Clone)]
pub struct ToolLaunchRegistry {
    launches: Vec<ToolLaunch>,
}

impl ToolLaunchRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a new launch. Newer launches append to the tail so the
    /// dashboard's render order is "oldest at top, newest at bottom"
    /// — predictable for users who fire `g` (lazygit) repeatedly.
    /// Returns the index the launch was inserted at, so callers can
    /// reference it for immediate UI updates.
    pub fn push(&mut self, launch: ToolLaunch) -> usize {
        let idx = self.launches.len();
        self.launches.push(launch);
        idx
    }

    /// Remove the entry at `index` if it exists, returning the
    /// removed launch. Used by the "prune on attach failure" path —
    /// when the user picks a tool row whose tmux session has died,
    /// the dashboard removes the row so the next render shows the
    /// shrunken list.
    pub fn remove(&mut self, index: usize) -> Option<ToolLaunch> {
        if index < self.launches.len() {
            Some(self.launches.remove(index))
        } else {
            None
        }
    }

    /// Lookup by tmux session name. Used when the embedded PTY for a
    /// tool launch exits and the prune path needs to find the entry
    /// to drop. Returns the index for `remove`.
    #[must_use]
    pub fn position_by_tmux_session(&self, name: &str) -> Option<usize> {
        self.launches.iter().position(|l| l.tmux_session == name)
    }

    #[must_use]
    pub fn launches(&self) -> &[ToolLaunch] {
        &self.launches
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.launches.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.launches.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch(name: &str, tmux: &str) -> ToolLaunch {
        ToolLaunch {
            name: name.to_string(),
            host: HostId::local(),
            tmux_session: tmux.to_string(),
            project_dir: PathBuf::from("/work/proj"),
            launched_at: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn push_appends_at_tail_and_returns_index() {
        let mut r = ToolLaunchRegistry::new();
        assert_eq!(r.push(launch("a", "agent-mux-tool-1")), 0);
        assert_eq!(r.push(launch("b", "agent-mux-tool-2")), 1);
        assert_eq!(r.len(), 2);
        assert_eq!(r.launches()[0].name, "a");
        assert_eq!(r.launches()[1].name, "b");
    }

    #[test]
    fn remove_drops_entry_and_shifts_later_indices() {
        let mut r = ToolLaunchRegistry::new();
        r.push(launch("a", "agent-mux-tool-1"));
        r.push(launch("b", "agent-mux-tool-2"));
        r.push(launch("c", "agent-mux-tool-3"));

        let dropped = r.remove(1).expect("found");
        assert_eq!(dropped.name, "b");
        let names: Vec<&str> = r.launches().iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, vec!["a", "c"]);
    }

    #[test]
    fn remove_returns_none_for_out_of_range_index() {
        let mut r = ToolLaunchRegistry::new();
        r.push(launch("a", "x"));
        assert!(r.remove(5).is_none());
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn position_by_tmux_session_locates_matching_entry() {
        let mut r = ToolLaunchRegistry::new();
        r.push(launch("a", "agent-mux-tool-1"));
        r.push(launch("b", "agent-mux-tool-2"));
        assert_eq!(r.position_by_tmux_session("agent-mux-tool-2"), Some(1));
        assert_eq!(r.position_by_tmux_session("nope"), None);
    }
}
