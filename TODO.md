# TODO

Flat backlog. Each entry tagged with `#area`. Done items deleted, not struck through.

**New ideas go in here first.** When a feature, polish item, or design idea surfaces — whether from the user or the assistant — the first move is an entry below with the rationale captured at idea-time. Then, separately, decide whether to implement now or leave it. The default is "codify, then defer"; pulling an entry forward is a second decision the user makes deliberately.

## Backlog

### M0 — Local dashboard

- tune attention heuristic after dogfooding (current rules: assistant → needs-input, user/tool result → working, mtime > 1h → idle); make thresholds configurable in M4 `#m0 #attention`

### M1 — Session creation + worktree management

- M1 do-not-reproduce checklist (from user, 2026-05-16): (a) switching between worktree-backed sessions must not approach claude-squad's 10+ second cost — design every M1 code path with the "switching never blocks on I/O" discipline in mind; (b) M1 worktree creation must not bake in local-only assumptions that would later need to be unwound for remote session creation (post-M4). `#m1 #design`
- make newly-created sessions appear in the dashboard without an agent-mux restart. Today the modal flow creates the worktree, launches `claude`, and switches focus to it — but the dashboard's catalog + TranscriptWatcher were built from a fixed snapshot at startup, so the new session is invisible until restart. Needs: (a) a way to add a session to the catalog without resetting `list_state.selected()` (catalog currently only exposes `replace_all`), (b) a way to register a new transcript target with the running TranscriptWatcher mid-run, (c) a trigger — either periodic re-scan of `~/.claude/projects/`, or a `notify` watch on that directory for new JSONL files. `#m1 #ui #watcher`
- decide refresh policy for the Repo Registry. Currently it scans once at startup and never re-scans; new repos added to a workspace folder mid-session are invisible until restart. Options: TTL on modal open, manual refresh keybind, both. Deferred until M1 dogfooding shows how often this bites. `#m1 #repo`
- add `WorktreeManager::list` + `WorktreeManager::remove` once a caller materializes (post-M4 discard/merge workflow, or earlier if discovery needs to reconcile worktree-spawned sessions). Deferred to avoid speculative surface area. `#m1 #worktree`
- add a positive-path test for `worktree::resolve_default_base_branch` that exercises the `origin/HEAD` resolver (requires a bare-remote fixture in the test). Current coverage only hits the `main`/`master` fallback and the empty-result negative case. `#m1 #test`
- larger move on return-to-dashboard discoverability: in inside-tmux mode, name the agent-mux window predictably (e.g. `agent-mux`) so the hint can become "switch-client -t agent-mux" instead of the generic "prefix+s" picker. Deferred — the footer hint shipped first to see whether it's enough in dogfooding before introducing a window-naming convention. `#m1 #ui #attachment`
- title fallback (3): use first user message, truncated, when neither `task.toml` nor `ai-title` is present. Rare in practice — a session with no ai-title typically also has no messages — so deferred until dogfooding shows it bites. `#m1 #ui #discovery`

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
- extend Config (M1 minimal already shipped) with the full M4 surface; sane defaults if absent `#m4 #config`
- reload-on-edit (watch config file, re-apply) `#m4 #config`
- env-var expansion in `workspace_folders` (e.g. `$HOME/work`, `$WORK_DIR/repos`). M1 ships tilde expansion only; env vars deferred. `#m4 #config`

### Cross-cutting / deferred decisions

- decide session discovery policy: all-transcripts vs recent-only vs explicit-register vs hybrid `#m0 #discovery` — log the call once M0 dogfooding surfaces what's annoying
- decide behaviour when an attached tmux window is killed externally (drop session, mark dead, relaunch) `#m0 #attachment` — same: defer to dogfooding
- evaluate Claude Code hooks API (`Notification`, `Stop`) as a supplement or replacement for transcript tailing `#post-m0 #attention`
- post-M4: diff view (what an agent has changed against the base branch) `#post-m4 #review`
- post-M4: merge / discard workflow for completed sessions `#post-m4 #worktree`
- post-M4: remote session *creation* (spawning new sessions on SSH hosts, not just attaching to existing ones) `#post-m4 #remote`
- post-M1 polish: dashboard grouping by repo — sessions render under their parent repo headers, with collapsing per-repo. Deferred until the flat-list rendering of M1 is dogfooded; we may not need grouping if labels are enough. `#post-m1 #ui`
- decide UX when agent-mux is launched from inside an existing tmux session vs from a bare shell `#m0 #ui`
- flesh out `agent-mux-review` Layer 2 categories as project-specific rules emerge in `ARCHITECTURE.md` `#review #setup`
- post-M2: switch-latency smoke test (or criterion benchmark) that fails if a focus-change round trip exceeds a budget (target: ≤ 50ms against an in-memory catalog populated with N sessions). Reason: claude-squad's 10+ second switching is the empirical failure mode we exist to avoid. Deferred from M0 — the budget needs to cover both local and remote switching, and setting a local-only number now risks rework once M2 lands. `#post-m2 #perf`
