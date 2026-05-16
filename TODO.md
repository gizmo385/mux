# TODO

Flat backlog. Each entry tagged with `#area`. Done items deleted, not struck through.

**New ideas go in here first.** When a feature, polish item, or design idea surfaces — whether from the user or the assistant — the first move is an entry below with the rationale captured at idea-time. Then, separately, decide whether to implement now or leave it. The default is "codify, then defer"; pulling an entry forward is a second decision the user makes deliberately.

## Backlog

### M0 — Local dashboard

- tune attention heuristic after dogfooding (current rules: assistant → needs-input, user/tool result → working, mtime > 1h → idle); make thresholds configurable in M4 `#m0 #attention`
- dogfood M0 for a week; record friction notes for M1 / M2 scoping `#m0 #dogfood`

### M1 — Session creation + worktree management

- ask user what specifically didn't work in claude-squad; turn answers into a "do-not-reproduce" checklist for M1 design `#m1 #design`
- decide worktree directory layout (alongside-repo vs in-repo `.agent-mux/worktrees/` vs global `~/.local/state/agent-mux/worktrees/`) `#m1 #design`
- decide worktree fate on session close (keep / prompt / auto-clean if unmodified / auto-clean always) `#m1 #design`
- decide task metadata format (plain text file vs TOML with structured fields) `#m1 #design`
- decide base-branch resolution policy (default `main` / current branch / always prompt) `#m1 #design`
- implement WorktreeManager: `create(repo, base_branch, task) -> PathBuf`, `list()`, `remove(path)` via `git worktree` shell-out `#m1 #worktree`
- extend AttachmentDriver trait with `spawn_session(cwd) -> SessionId`; wire TmuxDriver to launch `claude` in the new worktree's tmux window `#m1 #attachment`
- new-session UI flow in dashboard: keybind opens a small modal/prompt, captures task name + base branch, dispatches to WorktreeManager + AttachmentDriver, registers the new session in the catalog `#m1 #ui`
- persist task metadata in the new worktree (format TBD per above) `#m1 #worktree`

### M2 — Remote hosts

- design host config format (likely `[hosts.<name>]` TOML table with `ssh = "..."` and `transcript_root = "..."`) `#m2 #config`
- implement HostAbstraction with local and ssh impls; SSH impl manages ControlMaster socket lifecycle `#m2 #remote`
- extend Transcript Watcher to poll remote transcript files at configurable interval over the host's SSH channel `#m2 #attention #remote`
- extend TmuxDriver to attach into remote tmux via a persistent local tmux window running `ssh -t host tmux attach` `#m2 #attachment #remote`

### M3 — Inline preview

- implement transcript renderer: parse Claude Code JSONL entries (user/assistant/tool_use/tool_result) into compact display lines `#m3 #preview`
- add per-row preview pane to Dashboard (last N entries, default configurable later) `#m3 #ui`
- record observations: does seeing inline preview change which sessions the user attaches to? Does the user want richer chat? This is the Shape B pivot decision input. `#m3 #dogfood`

### M4 — Customization

- design TOML schema for themes (named colours → ratatui Style) `#m4 #config`
- design TOML schema for keybinds (action name → key combo) `#m4 #config`
- implement config load at startup with sane defaults if absent `#m4 #config`
- reload-on-edit (watch config file, re-apply) `#m4 #config`

### Cross-cutting / deferred decisions

- decide session discovery policy: all-transcripts vs recent-only vs explicit-register vs hybrid `#m0 #discovery` — log the call once M0 dogfooding surfaces what's annoying
- decide behaviour when an attached tmux window is killed externally (drop session, mark dead, relaunch) `#m0 #attachment` — same: defer to dogfooding
- evaluate Claude Code hooks API (`Notification`, `Stop`) as a supplement or replacement for transcript tailing `#post-m0 #attention`
- post-M4: diff view (what an agent has changed against the base branch) `#post-m4 #review`
- post-M4: merge / discard workflow for completed sessions `#post-m4 #worktree`
- post-M4: remote session *creation* (spawning new sessions on SSH hosts, not just attaching to existing ones) `#post-m4 #remote`
- decide UX when agent-mux is launched from inside an existing tmux session vs from a bare shell `#m0 #ui`
- flesh out `agent-mux-review` Layer 2 categories as project-specific rules emerge in `ARCHITECTURE.md` `#review #setup`
