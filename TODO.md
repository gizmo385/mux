# TODO

Flat backlog. Each entry tagged with `#area`. Done items deleted, not struck through.

**New ideas go in here first.** When a feature, polish item, or design idea surfaces — whether from the user or the assistant — the first move is an entry below with the rationale captured at idea-time. Then, separately, decide whether to implement now or leave it. The default is "codify, then defer"; pulling an entry forward is a second decision the user makes deliberately.

## Backlog

### M0 — Local dashboard

- tune attention heuristic after dogfooding (current rules: assistant → needs-input, user/tool result → working, mtime > 1h → idle); make thresholds configurable in M4 `#m0 #attention`

### M1 — Session creation + worktree management

- M1 do-not-reproduce checklist (from user, 2026-05-16): (a) switching between worktree-backed sessions must not approach claude-squad's 10+ second cost — design every M1 code path with the "switching never blocks on I/O" discipline in mind; (b) M1 worktree creation must not bake in local-only assumptions that would later need to be unwound for remote session creation (post-M4). `#m1 #design`
- add `WorktreeManager::list` + `WorktreeManager::remove` once a caller materializes (post-M4 discard/merge workflow, or earlier if discovery needs to reconcile worktree-spawned sessions). Deferred to avoid speculative surface area. `#m1 #worktree`
- larger move on return-to-dashboard discoverability: in inside-tmux mode, name the agent-mux window predictably (e.g. `agent-mux`) so the hint can become "switch-client -t agent-mux" instead of the generic "prefix+s" picker. Deferred — the footer hint shipped first to see whether it's enough in dogfooding before introducing a window-naming convention. `#m1 #ui #attachment`

### M2 — Remote hosts

- wire `SshHost` into discovery + the catalog: instantiate one `SshHost` per `[hosts.<name>]` config entry at startup, run discovery against each host's `transcript_root`, and feed the resulting `Session`s (with the right `HostId`) into the catalog. The `Host` trait + `LocalHost` (chunk 1) and `SshHost` itself (chunk 2) shipped; this chunk is the wire-up. `#m2 #remote`
- extend Transcript Watcher to poll remote transcript files at configurable interval over the host's SSH channel `#m2 #attention #remote`
- extend TmuxDriver to attach into remote tmux via a persistent local tmux window running `ssh -t host tmux attach` `#m2 #attachment #remote`
- `SshHost` assumes a GNU `find` on the remote (uses `-printf '%T@ %p\0'`). macOS remotes need Homebrew `findutils` or a BSD-stat fallback. Defer until a macOS remote actually surfaces. `#m2 #remote #portability`

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
- bound discovery memory: `Host::read_to_string` currently allocates the full transcript for cwd/title extraction at startup. For multi-MB transcripts × N sessions this is a real transient spike, and over SSH it's a per-session round-trip of the whole file. Likely fix: cap discovery reads to head (e.g. first 64 KB for cwd + first-user-message) + tail (e.g. last 32 KB for the latest `ai-title`); add a `read_head` companion to `read_tail` on the `Host` trait, both bounded. Deferred — wait for SSH-impl dogfooding to confirm the memory/latency cost is real before adding trait surface. `#post-m2 #perf #host`
