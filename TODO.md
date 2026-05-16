# TODO

Flat backlog. Each entry tagged with `#area`. Done items deleted, not struck through.

**New ideas go in here first.** When a feature, polish item, or design idea surfaces — whether from the user or the assistant — the first move is an entry below with the rationale captured at idea-time. Then, separately, decide whether to implement now or leave it. The default is "codify, then defer"; pulling an entry forward is a second decision the user makes deliberately.

## Backlog

### M0 — Local dashboard

- tune attention heuristic after dogfooding (current rules: assistant → needs-input, user/tool result → working, mtime > 1h → idle); make thresholds configurable in M4 `#m0 #attention`
- dogfood M0 for a week; record friction notes for M1 / M2 scoping `#m0 #dogfood`
- add a switch-latency smoke test (or criterion benchmark) that fails if a focus-change round trip exceeds a budget (target: ≤ 50ms against an in-memory catalog populated with N sessions). Reason: claude-squad's 10+ second switching is the empirical failure mode we exist to avoid; make regressions visible in CI rather than catching them only in dogfooding. `#m0 #perf`

### M1 — Session creation + worktree management

- M1 do-not-reproduce checklist (from user, 2026-05-16): (a) switching between worktree-backed sessions must not approach claude-squad's 10+ second cost — design every M1 code path with the "switching never blocks on I/O" discipline in mind; (b) M1 worktree creation must not bake in local-only assumptions that would later need to be unwound for remote session creation (post-M4). `#m1 #design`
- wire the new-session modal's Submit to off-thread `WorktreeManager::create` + `AttachmentDriver::spawn_session`. Currently the submit just stuffs a "would create: …" line into the status bar. The dispatch must run on a background task so `git worktree add` never blocks the UI thread; modal stays in a "creating worktree…" state until the result comes back. On success, close the modal and hand off any `AttachOutcome::SuspendAndRun` via the existing suspend machinery. On error, surface in the dashboard status line. The new session is *not* eagerly registered in the catalog — the transcript watcher picks it up naturally. `#m1 #ui`
- decide refresh policy for the Repo Registry. Currently it scans once at startup and never re-scans; new repos added to a workspace folder mid-session are invisible until restart. Options: TTL on modal open, manual refresh keybind, both. Deferred until M1 dogfooding shows how often this bites. `#m1 #repo`
- add `WorktreeManager::list` + `WorktreeManager::remove` once a caller materializes (post-M4 discard/merge workflow, or earlier if discovery needs to reconcile worktree-spawned sessions). Deferred to avoid speculative surface area. `#m1 #worktree`
- add a positive-path test for `worktree::resolve_default_base_branch` that exercises the `origin/HEAD` resolver (requires a bare-remote fixture in the test). Current coverage only hits the `main`/`master` fallback and the empty-result negative case. `#m1 #test`

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
