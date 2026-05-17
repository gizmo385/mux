# TODO

Flat backlog. Each entry tagged with `#area`. Done items deleted, not struck through.

**New ideas go in here first.** When a feature, polish item, or design idea surfaces — whether from the user or the assistant — the first move is an entry below with the rationale captured at idea-time. Then, separately, decide whether to implement now or leave it. The default is "codify, then defer"; pulling an entry forward is a second decision the user makes deliberately.

## Backlog

### M0 — Local dashboard

- tune attention heuristic after dogfooding (current rules: assistant → needs-input, user/tool result → working, mtime > 1h → idle); make thresholds configurable in M5 `#m0 #attention`

### M1 — Session creation + worktree management

- M1 do-not-reproduce checklist (from user, 2026-05-16): (a) switching between worktree-backed sessions must not approach claude-squad's 10+ second cost — design every M1 code path with the "switching never blocks on I/O" discipline in mind; (b) M1 worktree creation must not bake in local-only assumptions that would later need to be unwound for remote session creation (post-M5). `#m1 #design`
- add `WorktreeManager::list` + `WorktreeManager::remove` once a caller materializes (post-M5 discard/merge workflow, or earlier if discovery needs to reconcile worktree-spawned sessions). Deferred to avoid speculative surface area. `#m1 #worktree`
- larger move on return-to-dashboard discoverability: in inside-tmux mode, name the agent-mux window predictably (e.g. `agent-mux`) so the hint can become "switch-client -t agent-mux" instead of the generic "prefix+s" picker. Deferred — the footer hint shipped first to see whether it's enough in dogfooding before introducing a window-naming convention. `#m1 #ui #attachment`

### M2 — Remote hosts

- `SshHost` assumes a GNU `find` on the remote (uses `-printf '%T@ %p\0'`). macOS remotes need Homebrew `findutils` or a BSD-stat fallback. Defer until a macOS remote actually surfaces. `#m2 #remote #portability`
- remote polling: surface persistent `list_transcripts` failures (e.g. SSH master died mid-run) instead of silently retrying every tick. Today the polling loop swallows errors so attention freezes silently when a host goes away. Likely shape: an `AttentionUpdate(_, Unknown)` for every known session on the affected host after N consecutive failures, plus a footer hint mirroring the startup `connect_errors` line. Defer until dogfooding shows it's actually a problem — for stable home-LAN SSH the silent retry is often fine. `#m2 #remote #attention`
- remote polling: known-set never shrinks. When a transcript is deleted on the remote, the polling thread's `HashMap` keeps the entry forever and the catalog keeps a stale row. Probably wants to be addressed together with deletion handling in the catalog (no current code path removes sessions). Defer until session-removal is on the menu. `#m2 #remote #lifecycle`
- remote attach: nested-tmux prefix collision when running agent-mux from inside local tmux. Documented as a known gotcha in README; users handle it with a different inner-tmux prefix or `<prefix> <prefix>` passthrough. Revisit if dogfooding shows it's worse than expected — possible mitigations: pin the remote tmux to a separate socket name we could later override config on, or wrap with byobu. `#m2 #remote #ux`
- remote attach: the auto-spawned `agent-mux-<id>` tmux sessions on the remote accumulate over time — every resumed conversation leaves one behind. The user can `tmux kill-session` manually; a discard/cleanup affordance from the dashboard would be nicer. Plausibly bundles with the post-M5 worktree discard/merge workflow. `#m2 #remote #cleanup`
- remote-session cache: cleanup of orphaned `~/.cache/agent-mux/sessions/<host>.json` files when a host is removed from config. Today the file is left on disk; harmless (it's never read because we only load cache for hosts in current config) but leaks bytes. Probably bundles with the post-M5 shutdown/cleanup work. `#m2 #remote #cleanup`
- remote-session cache: decide whether to visually distinguish cached-but-not-yet-refreshed rows from live ones. Shipped with no distinction in 2026-05 — argument was that the cache reflects last-known state and the staleness window is bounded by the SSH handshake (~seconds). Revisit if dogfooding shows confusion ("why is this row showing stale attention?"). Lowest-cost option: extra dim modifier on rows whose host is in `pending_hosts`. `#m2 #remote #ux #post-cache`

### M3 — Inline preview

- record observations: does seeing inline preview change which sessions the user attaches to? Does the user want richer chat? This is the Shape B pivot decision input. `#m3 #dogfood`
- decide preview verbosity config knob (M5): how many entries, whether to show tool result body, whether thinking blocks are surfaced. SPEC.md notes "configuration for preview verbosity" as part of M3 scope but defers shape; revisit once dogfooding has surfaced what the user actually wants to tune. `#m3 #m5 #config #dogfood`

### M4 — Attention notifications

- pull in `notify-rust` (cross-platform: libnotify on Linux, NSUserNotification on macOS) as the notification backend `#m4 #notifications`
- fire one notification at the catalog's attention-update boundary when the previous attention was `Working`/`Idle`/`Unknown` and the new attention is `NeedsInput`; payload carries the session's title + host label so the user knows where to look `#m4 #notifications`
- in-process suppression: debounce against rapid attention flapping; per-session "I've seen this, hush" so we don't re-notify on every transition while the user is mid-decision `#m4 #notifications`
- terminal-focus suppression (don't notify when agent-mux's terminal already has focus): defer to dogfooding — likely not reliably detectable across terminal emulators, and may not be worth special-casing if it turns out rare in practice `#m4 #notifications #dogfood`
- user-facing config knobs (on/off, sound, quiet hours, per-host suppression) deliberately *not* in M4 — they belong in M5's broader config surface alongside themes and keybinds `#m4 #notifications #defer-to-m5`

### M5 — Customization

- design TOML schema for themes (named colours → ratatui Style) `#m5 #config`
- design TOML schema for keybinds (action name → key combo) `#m5 #config`
- extend Config (M1 minimal already shipped) with the full M5 surface; sane defaults if absent `#m5 #config`
- reload-on-edit (watch config file, re-apply) `#m5 #config`
- env-var expansion in `workspace_folders` (e.g. `$HOME/work`, `$WORK_DIR/repos`). M1 ships tilde expansion only; env vars deferred. `#m5 #config`
- expose previously-hardcoded thresholds: idle threshold (`IDLE_THRESHOLD` in `main.rs`, default 1h) and remote poll interval (`REMOTE_POLL_INTERVAL` in `watcher.rs`, default 3s). Both flagged in code comments as M5 work. `#m5 #config`
- expose M4 notification knobs (on/off, sound, quiet hours, per-host suppression). Deferred from M4 deliberately — M4 ships with sane in-process suppression only; M5 adds the user-facing surface. `#m5 #config #notifications`

### Cross-cutting / deferred decisions

- decide session discovery policy: all-transcripts vs recent-only vs explicit-register vs hybrid `#m0 #discovery` — log the call once M0 dogfooding surfaces what's annoying
- decide behaviour when an attached tmux window is killed externally (drop session, mark dead, relaunch) `#m0 #attachment` — same: defer to dogfooding
- evaluate Claude Code hooks API (`Notification`, `Stop`) as a supplement or replacement for transcript tailing `#post-m0 #attention`
- post-M5: diff view (what an agent has changed against the base branch) `#post-m5 #review`
- post-M5: merge / discard workflow for completed sessions `#post-m5 #worktree`
- post-M5: remote session *creation* (spawning new sessions on SSH hosts, not just attaching to existing ones) `#post-m5 #remote`
- decide UX when agent-mux is launched from inside an existing tmux session vs from a bare shell `#m0 #ui`
- flesh out `agent-mux-review` Layer 2 categories as project-specific rules emerge in `ARCHITECTURE.md` `#review #setup`
- post-M2: switch-latency smoke test (or criterion benchmark) that fails if a focus-change round trip exceeds a budget (target: ≤ 50ms against an in-memory catalog populated with N sessions). Reason: claude-squad's 10+ second switching is the empirical failure mode we exist to avoid. Deferred from M0 — the budget needs to cover both local and remote switching, and setting a local-only number now risks rework once M2 lands. `#post-m2 #perf`
- bound discovery memory: `Host::read_to_string` currently allocates the full transcript for cwd/title extraction at startup. For multi-MB transcripts × N sessions this is a real transient spike, and over SSH it's a per-session round-trip of the whole file. Likely fix: cap discovery reads to head (e.g. first 64 KB for cwd + first-user-message) + tail (e.g. last 32 KB for the latest `ai-title`); add a `read_head` companion to `read_tail` on the `Host` trait, both bounded. Deferred — wait for SSH-impl dogfooding to confirm the memory/latency cost is real before adding trait surface. `#post-m2 #perf #host`
- stale-socket sweep on `SshHost::connect`: when a previous agent-mux is killed mid-`thread::sleep`, its `agent-mux-ssh-<host>-<pid>.sock` file may persist in `$TMPDIR` past the remote master's `ControlPersist` window. Harmless on first principles (a fresh PID gets a fresh path so the socket file never collides with a new connect attempt) but leaks files until tmpdir cleanup. Mitigation: at connect time, list `$TMPDIR/agent-mux-ssh-*-<pid>.sock` entries whose `<pid>` is no longer a live process and remove them. Reason it's a real concern only at scale: a heavy dogfooder + monthly tmpdir rotation could leave dozens of zero-byte sockets. Filed during the 2026-05 shutdown audit. `#cleanup #lifecycle #post-m5`
