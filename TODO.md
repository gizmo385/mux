# TODO

Flat backlog. Each entry tagged with `#area`. Done items deleted, not struck through.

**New ideas go in here first.** When a feature, polish item, or design idea surfaces — whether from the user or the assistant — the first move is an entry below with the rationale captured at idea-time. Then, separately, decide whether to implement now or leave it. The default is "codify, then defer"; pulling an entry forward is a second decision the user makes deliberately.

## Backlog

### M0 — Local dashboard

- implement Session struct and in-memory SessionCatalog with subscribe/publish for state changes `#m0 #core`
- implement Claude Code project discovery: scan `~/.claude/projects/` and surface recent transcripts as Sessions `#m0 #discovery`
- implement local Transcript Watcher: tail JSONL with `notify`, parse entries, emit attention events `#m0 #attention`
- decide initial attention heuristic ("last entry is assistant + N seconds of stillness → needs-input"); make N configurable later `#m0 #attention`
- define AttachmentDriver trait; implement TmuxDriver (attach into a tmux window, spawn terminal in cwd) `#m0 #attachment`
- wire Dashboard render: list view with project, host, attention state, time-since-last-event `#m0 #ui`
- wire Dashboard input: arrow keys, Enter to attach, `t` to spawn terminal, `q` to quit `#m0 #ui`
- dogfood M0 for a week; record friction notes for M1 / M2 scoping `#m0 #dogfood`

### M1 — Remote hosts

- design host config format (likely `[hosts.<name>]` TOML table with `ssh = "..."` and `transcript_root = "..."`) `#m1 #config`
- implement HostAbstraction with local and ssh impls; SSH impl manages ControlMaster socket lifecycle `#m1 #remote`
- extend Transcript Watcher to poll remote transcript files at configurable interval over the host's SSH channel `#m1 #attention #remote`
- extend TmuxDriver to attach into remote tmux via a persistent local tmux window running `ssh -t host tmux attach` `#m1 #attachment #remote`

### M2 — Inline preview

- implement transcript renderer: parse Claude Code JSONL entries (user/assistant/tool_use/tool_result) into compact display lines `#m2 #preview`
- add per-row preview pane to Dashboard (last N entries, default configurable later) `#m2 #ui`
- record observations: does seeing inline preview change which sessions the user attaches to? Does the user want richer chat? This is the Shape B pivot decision input. `#m2 #dogfood`

### M3 — Customization

- design TOML schema for themes (named colours → ratatui Style) `#m3 #config`
- design TOML schema for keybinds (action name → key combo) `#m3 #config`
- implement config load at startup with sane defaults if absent `#m3 #config`
- reload-on-edit (watch config file, re-apply) `#m3 #config`

### Cross-cutting / deferred decisions

- decide session discovery policy: all-transcripts vs recent-only vs explicit-register vs hybrid `#m0 #discovery` — log the call once M0 dogfooding surfaces what's annoying
- decide behaviour when an attached tmux window is killed externally (drop session, mark dead, relaunch) `#m0 #attachment` — same: defer to dogfooding
- evaluate Claude Code hooks API (`Notification`, `Stop`) as a supplement or replacement for transcript tailing `#post-m0 #attention`
- decide UX when agent-mux is launched from inside an existing tmux session vs from a bare shell `#m0 #ui`
- flesh out `agent-mux-review` Layer 2 categories as project-specific rules emerge in `ARCHITECTURE.md` `#review #setup`
