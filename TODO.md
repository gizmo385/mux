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
- remote discovery on high-latency proxied SSH (Coder, jumphost) — progress log of the optimisation arc. Original measurement 2026-05-18 against coder-proxied `gizmo.coder`: 233 transcripts × 4 sequential SSH round-trips per session ≈ 7+ minutes wall-clock. Shipped same day in two passes:
  1. ~~**Filter by recency at the `find` boundary.**~~ `DISCOVERY_MAX_AGE` in `src/discovery.rs`, 30 days default. 233 → 21 surviving sessions. Brought wall-clock to 55 s. M5 will surface the knob alongside `IDLE_THRESHOLD` / `REMOTE_POLL_INTERVAL`.
  2. ~~**Batch per-transcript reads into one round-trip + compute attention from the same buffer (collapses option 4).**~~ Added `Host::read_many` and `Host::is_dir_many` and a `derive_attention_from_content` helper; refactored `discover()` to do list + bulk-read + bulk-is_dir + bulk-task.toml in four total SSH round-trips regardless of N, with attention falling out of the already-fetched buffers. SSH `Compression=yes` set on the `ControlMaster`. End-to-end now ~6 s warm / ~15 s cold on the same host (down from 55 s).
  3. ~~**Stream sessions into the catalog progressively.**~~ Shipped 2026-05-18 in a smaller-than-originally-scoped form. The actual user-pain wasn't "blank dashboard while discovery runs" (the cache already paints rows on first frame); it was that *cached rows weren't attachable* until discovery finished because `App.hosts` only learned about the `Arc<dyn Host>` at `Ready` time. Fix: split `RemoteDiscoveryResult` into `Connected` (fires after SSH `ControlMaster` is up, ~3 s) and `Ready` (fires after discovery, later). `Connected` registers the host + starts the polling threads, so cached rows become attachable within seconds of launch even if discovery is still grinding. Full per-session streaming into the catalog wasn't necessary for this complaint — the cache already does that part. Revisit if a *new* dogfooding signal asks for "watch rows pop in" UX.
  4. **Head-and-tail reads for transcripts.** Cold-case discovery is dominated by ~16 MB of transcript bytes streaming through the proxy. Most of that is the middle of long transcripts that discovery doesn't need (only first lines for cwd/first-user-message, last lines for ai-title/attention). A `read_head_tail_many(paths, head_bytes, tail_bytes)` op would cut payload to ~3 MB → ~2 s. Defer until dogfooding tells us whether the cold 15 s actually bites; the warm 6 s already covers the common re-launch case.
  `#m2 #remote #perf #discovery #dogfood`

### M0 — Local dashboard polish

- footer keybind line is dense after the group-jump hints landed (2026-05-17): `j/k: move · J/K: project · ⌃j/⌃k: host · ⏎: attach · t: terminal · p: preview · n: new · q: quit  ·  return: …` will truncate on narrow terminals. Not a regression (the line was already long) but worth a follow-up. Plausible shapes: (a) drop secondary hints (`t: terminal`, `n: new`) when terminal width < threshold; (b) hide the group-jump hints once the user has used them once (a "learned" signal); (c) split into two footer rows. Defer until dogfooding shows whether it actually bites. `#m0 #ui #footer`

### M3 — Inline preview

- record observations: does seeing inline preview change which sessions the user attaches to? Does the user want richer chat? This is the Shape B pivot decision input. `#m3 #dogfood`
- decide preview verbosity config knob (M5): how many entries, whether to show tool result body, whether thinking blocks are surfaced. SPEC.md notes "configuration for preview verbosity" as part of M3 scope but defers shape; revisit once dogfooding has surfaced what the user actually wants to tune. `#m3 #m5 #config #dogfood`
- markdown-aware preview rendering (split out from the newline fix, 2026-05-17): with newlines now preserved, the next readability gain is surfacing inline markdown — bold (`**x**`), italic (`_x_`), inline code (`` `x` ``), and bullet/numbered list glyphs. Risks: parser scope creep, mis-rendered code blocks, performance on large messages. Defer until newline-preservation has been dogfooded; that change may already be enough. `#m3 #ui #preview #dogfood`

### M4 — Attention notifications

- terminal-focus suppression (don't notify when agent-mux's terminal already has focus): defer to dogfooding — likely not reliably detectable across terminal emulators, and may not be worth special-casing if it turns out rare in practice `#m4 #notifications #dogfood`
- user-facing config knobs (on/off, sound, quiet hours, per-host suppression) deliberately *not* in M4 — they belong in M5's broader config surface alongside themes and keybinds `#m4 #notifications #defer-to-m5`
- WSL2 dogfooding: confirm notifications surface on the user's WSL2 + WSLg setup. `notify-rust` uses D-Bus on Linux; WSLg ships a notification daemon but variability across distros is real. If notifications don't appear, document the workaround (or pin a different backend) rather than silently failing. `#m4 #notifications #dogfood`
- Notifier::forget wiring: when `SessionCatalog::reconcile_host` drops entries (remote session gone), the per-session suppression state in `Notifier` leaks. Each entry is ~24 bytes so it's not urgent; revisit if a long-lived process shows growth. Reconciliation would need to expose which ids were dropped. `#m4 #lifecycle #cleanup`

### M5 — Customization

- extend `[theme]` schema beyond foreground colours: background colours, per-element modifier overrides (currently bold/dim/reversed are hardcoded), and richer keys (e.g. `[theme.header]` bold = true, fg = "..."). Today's flat per-element fg-only schema covers the common dogfood case; the more expressive shape waits until a real user-pain signal asks for it. `#m5 #config #theme`
- design TOML schema for keybinds (action name → key combo) `#m5 #config`
- reload-on-edit (watch config file, re-apply) `#m5 #config`
- env-var expansion in `workspace_folders` (e.g. `$HOME/work`, `$WORK_DIR/repos`). M1 ships tilde expansion only; env vars deferred. `#m5 #config`
- expose previously-hardcoded thresholds: idle threshold (`IDLE_THRESHOLD` in `main.rs`, default 1h), remote poll interval (`REMOTE_POLL_INTERVAL` in `watcher.rs`, default 3s), and discovery max-age (`DISCOVERY_MAX_AGE` in `discovery.rs`, default 30d — added 2026-05-18 with the recency filter). All flagged in code comments as M5 work. `#m5 #config`
- quiet hours for notifications: a `[notifications]` field like `quiet_hours = ["22:00-07:00"]` to suppress dispatch during a user-defined window. Deferred from the M5 notifications slice because timezone-aware time parsing adds scope; the on/off toggle and per-host suppression already cover the common case. `#m5 #notifications`

### Post-M5

- customizable notification sounds: today's M5 surface exposes a single `sound = true/false` flag mapping to the OS "default" sound. Real customization would mean per-host or per-event sound selection (different sound for needs-input vs. attention-flap; one host gets a chime, another gets a buzz). Wait for dogfooding to surface whether the binary toggle is enough; revisit if not. `#post-m5 #notifications #config`
- remote session creation: M1's worktree-creation + spawn flow, extended to `[hosts.<name>]` SSH targets. Design sketched 2026-05-18 with user feedback on the open questions; consolidates the brief placeholder previously in the Cross-cutting section. The architectural seam: `Host` trait gains two write-side primitives (`run` for arbitrary commands, `write_file` for small files like `.agent-mux/task.toml`), the `Repo` struct gains a `host: HostId` field, `RepoRegistry` scans both local and remote workspaces through the host abstraction, and `WorktreeManager` plus the default-branch resolver route their `git` calls through `host.run` instead of shelling directly. After the plumbing, the new-session modal picker shows host-labeled rows from all configured hosts; submitting a remote repo creates the worktree on the remote and spawns `claude` there via the existing `AttachmentDriver::spawn_session` path (already host-aware from M2's attach work).

  Config schema: `[hosts.<name>]` gains an optional `workspace_folders = [...]`. Tilde stays unexpanded so the remote shell resolves it (same convention as `transcript_root`). When a host omits the field, fall back to the top-level `workspace_folders` value — keeps "same workspaces local + remote" trivial; hosts with genuinely different layouts opt in by setting the per-host list explicitly.

  Trait additions:
    - `Host::run(cwd, program, args) -> io::Result<Output>` — arbitrary command. `LocalHost` uses `std::process::Command`; `SshHost` wraps it as `ssh master 'cd <cwd> && <quoted-cmd>'` so the `cwd` plumbing stays the trait's responsibility and callers don't need to know how each host handles working directory.
    - `Host::write_file(path, content) -> io::Result<()>` — small-file write, paralleling `read_to_string`. `LocalHost` calls `fs::write`; `SshHost` pipes content via `ssh master 'cat > <path>'`. Separate primitive (not "pipe through `Host::run`") keeps the architecture's "narrow op per primitive" discipline intact and matches the read-side trait surface.

  Open questions, resolved with user 2026-05-18:
    1. **Remote repo scan UX.** Fold silently into the existing host-pending state. No separate "scanning remote repos…" indicator; remote repos appear in the picker when their host reaches `Connected`, same shape as M2's session-discovery disk cache + reconcile.
    2. **Host offline at picker time.** Cached repos from prior runs still appear, but greyed out and not selectable until their host reaches `Connected`. Matches M2's attach UX (cached rows visible but inert until host is registered).
    3. **Default workspace path.** Per above — fall back to top-level `workspace_folders` when a host doesn't override.
    4. **`task.toml` write.** Dedicated `Host::write_file` trait method, not piped through `Host::run`.

  Suggested shipping order (each commit independently green, daily-loop discipline):
    1. **`feat(host): add Host::run + Host::write_file`** — pure plumbing. No callers switch yet. Tests pin `LocalHost` behaviour + the SSH command construction (incl. `cd <cwd>` injection, quoting).
    2. **`feat(repo): make Repo host-aware + scan remote workspaces`** — adds `Repo.host`, extends `HostConfig` with `workspace_folders`, refactors `RepoRegistry::from_config` to take the host map. Disk cache at `~/.cache/agent-mux/repos/<host>.json` so the picker is instant on re-launch. Modal picker shows host labels and greys out rows whose host is in `pending_hosts` (per Q2). No worktree creation on remote yet — submitting a remote repo errors out clearly. User-visible UX improvement (host-labeled picker) lands here.
    3. **`feat(worktree): create worktrees through Host::run`** — refactors `WorktreeManager` + default-branch resolver to host-aware. Local path unchanged in behaviour. After this commit, `n → pick remote repo → submit` creates the worktree on the right host.
    4. **`feat(post-m5): remote session spawn end-to-end`** — wires the resulting remote worktree path into `AttachmentDriver::spawn_session`. Most of this is verification + tests; the host abstraction already routes `spawn_session` through `Host::ssh_argv` from M2.

  `#post-m5 #remote #design`

### Cross-cutting / deferred decisions

- decide session discovery policy: all-transcripts vs recent-only vs explicit-register vs hybrid `#m0 #discovery` — log the call once M0 dogfooding surfaces what's annoying. **Update 2026-05-18:** dogfooding has surfaced it — 233 stale transcripts on a long-lived remote box, see the "remote discovery is O(N) sequential ssh round-trips" entry under M2 for measured numbers. The recent-only option is now the leading candidate because it composes cleanly with that entry's option (1).
- decide behaviour when an attached tmux window is killed externally (drop session, mark dead, relaunch) `#m0 #attachment` — same: defer to dogfooding
- evaluate Claude Code hooks API (`Notification`, `Stop`) as a supplement or replacement for transcript tailing `#post-m0 #attention`
- post-M5: diff view (what an agent has changed against the base branch) `#post-m5 #review`
- post-M5: merge / discard workflow for completed sessions `#post-m5 #worktree`
- decide UX when agent-mux is launched from inside an existing tmux session vs from a bare shell `#m0 #ui`
- flesh out `agent-mux-review` Layer 2 categories as project-specific rules emerge in `ARCHITECTURE.md` `#review #setup`
- post-M2: switch-latency smoke test (or criterion benchmark) that fails if a focus-change round trip exceeds a budget (target: ≤ 50ms against an in-memory catalog populated with N sessions). Reason: claude-squad's 10+ second switching is the empirical failure mode we exist to avoid. Deferred from M0 — the budget needs to cover both local and remote switching, and setting a local-only number now risks rework once M2 lands. `#post-m2 #perf`
- bound discovery memory: `Host::read_to_string` currently allocates the full transcript for cwd/title extraction at startup. For multi-MB transcripts × N sessions this is a real transient spike, and over SSH it's a per-session round-trip of the whole file. Likely fix: cap discovery reads to head (e.g. first 64 KB for cwd + first-user-message) + tail (e.g. last 32 KB for the latest `ai-title`); add a `read_head` companion to `read_tail` on the `Host` trait, both bounded. Deferred — wait for SSH-impl dogfooding to confirm the memory/latency cost is real before adding trait surface. `#post-m2 #perf #host`
- stale-socket sweep on `SshHost::connect`: when a previous agent-mux is killed mid-`thread::sleep`, its `agent-mux-ssh-<host>-<pid>.sock` file may persist in `$TMPDIR` past the remote master's `ControlPersist` window. Harmless on first principles (a fresh PID gets a fresh path so the socket file never collides with a new connect attempt) but leaks files until tmpdir cleanup. Mitigation: at connect time, list `$TMPDIR/agent-mux-ssh-*-<pid>.sock` entries whose `<pid>` is no longer a live process and remove them. Reason it's a real concern only at scale: a heavy dogfooder + monthly tmpdir rotation could leave dozens of zero-byte sockets. Filed during the 2026-05 shutdown audit. `#cleanup #lifecycle #post-m5`
