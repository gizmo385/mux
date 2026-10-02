# Plan: generalizing agent support into pluggable harnesses

Status: **draft for review, 2026-10-02.** Nothing in §4–§5 is implemented. §7 records the investigation of wrapping a general, agent-agnostic layer instead; its conclusion is that this folds into this design as extra sources rather than replacing it. Follows on from `docs/plans/2026-07-09-multi-agent-cli.md`, which put Claude Code, Codex and Pi behind the `AgentCli` trait.

Origin: the user asked, after a 2026-10-02 notification-parity audit of Codex/Pi, for a spec that makes new harnesses easier to add. Each harness should be able to supply its own handling for notifications, remote support, setup checks and so on. Driving example: a work harness whose session state is only available through a **remote task-execution HTTP API**, with no transcript file and possibly no tmux pane. That harness is *not* to be built now; the design must leave a clean place for it.

---

## 1. What the 2026-10-02 audit taught us

These are concrete requirements, not background. Every one was a real bug or gap found against real `codex` 0.142.5.

1. **"Read the transcript" is not one mechanism.**
   - Claude Code opens, appends and closes its file on each write, so macOS FSEvents fires.
   - Codex holds its rollout open, so FSEvents is silent until it closes and local Codex got no live updates at all.
   - An HTTP harness has no file.
   - The way live state is observed (events, polling, a stream) has to be a per-harness choice, not a property of the watcher.
2. **Some states are never in the transcript.** Codex approvals are never persisted. Signals from outside the transcript (hooks, a terminal bell, an API's status field) are first-class inputs, not a Claude-specific bolt-on.
3. **Signals can arrive before the session exists.** A hook fired before discovery saw the rollout was silently dropped. The catalog has to accept signals for sessions it doesn't know yet.
4. **Producers have output contracts.**
   - Codex parses hook stdout as a JSON decision and treats exit code 2 as "block".
   - Codex also requires the user to trust a hook in its own UI first.
   - Each harness owns how its signal producer is installed, and how agent-mux checks that the producer will actually run.
5. **Spawn identity differs.** Claude and Pi accept a caller-chosen id. Codex reveals its id only after the first prompt. An API harness gets an id back from a create call.
6. **Remote is per-capability, not per-agent.**
   - **Rollout reads:** work over SSH.
   - **Hooks:** need the agent-mux binary installed and trusted on the remote, and the remote side has to agree with the local side on where markers go.
   - **Trust checks:** need remote reads.
   - **HTTP harness:** has no "host" at all.

## 2. Where the coupling lives today

Summary of a 2026-10-02 code survey; file:line detail is in the session notes.

- **`AgentCli`** (`src/agent.rs`) covers parsing only: transcript root and tree shape, head/tail parsing, id-from-path, the spawn plan and the resume string. It is `&'static`, holds no state and has no config, so it can't own a thread, credentials or a base URL.
- **`Host`** (`src/host.rs`) has five jobs at once:
  - state reads (`list_transcripts`, `read_tail`)
  - filesystem writes
  - command execution (`run`)
  - transport metadata (`ssh_argv`)
  - connection health (`ensure_connected`)
- **`HostId`** also has several jobs: exec target, cache file key, sidebar group, and the `Session.host` tag. An HTTP service is a *state source* and an *attach target*, but not an exec target.
- **The glue between them is hard-wired free functions:**
  - discovery, the `notify` thread, the per-host pollers and the hook-marker drains all assume `Session.transcript_path: PathBuf` and mtime;
  - attach and spawn always produce tmux argv;
  - `LivePanes` assumes every session can have a tmux pane.
- **`AgentKind` is a closed `Copy` enum** with a static registry. The cache maps unknown labels to Claude. `[agents.<label>]` only accepts claude/codex/pi.
- **Attention events can't express the HTTP case.** `Attention` is four states plus a `blocking` flag. A hook pin can only force `NeedsInput`, and it is released by "a later transcript mtime". That assumes a transcript-based heuristic exists, so it doesn't generalize to an authoritative status from an API.

## 3. Goals and non-goals

**Goals**
- Adding a harness means implementing a small set of **optional capabilities**, not touching the watcher, catalog, drivers and `main.rs`.
- A harness can be **file-backed** (today's three), **service-backed** (HTTP and similar), or **adapter-backed** (an external process, possibly written in another language, speaking a small protocol).
- Notifications, remote support, setup and health checks, attach and spawn are all **per-harness extensions** with agent-neutral defaults.
- With no new config, today's three agents behave exactly as they do now.
- The project's disciplines keep holding:
  - session switching never blocks on I/O
  - one event loop
  - no `unsafe`, which rules out loading plugins as dynamic libraries
  - state comes from a single authority per session

**Non-goals for this plan**
- Building the HTTP work harness.
- Rendering conversations in agent-mux.
- Replacing tmux for the agents that already use it.

## 4. Design

### 4.1 Vocabulary

| Term | Meaning |
|---|---|
| **Harness** | An agent integration. Replaces "agent CLI" as the unit: `claude`, `codex`, `pi`, `work-tasks`, … |
| **Location** | Where a harness's sessions live. Either a **machine** (`local`, or an SSH host, as today) or a **service** (`work-api`: a base URL and credentials). This is the sidebar grouping. It splits off from `Host`, which shrinks to being a machine's transport. |
| **Source** | The running thing that discovers sessions and reports their state, for one harness at one location. It owns its own thread. |
| **Signal** | An out-of-band state hint that isn't the source's main state stream: a hook marker, a bell, a push event. |
| **Capability** | An optional piece of behaviour a harness provides (spawn, attach, signals, doctor, …). Anything missing gets a neutral default or is hidden in the UI. |

### 4.2 The normalized event model (the core change)

Every source and every signal producer emits one vocabulary, replacing today's transcript-shaped `WatcherEvent::{NewTranscript, Attention, Hook}`:

```rust
enum SourceEvent {
    /// A session exists (or its metadata changed). Carries everything the
    /// row needs; no path required.
    Upsert { key: SessionKey, meta: SessionMeta },
    /// The session is gone (deleted upstream, archived, expired).
    Gone { key: SessionKey },
    /// State observation. `authority` decides precedence (§4.3).
    Status {
        key: SessionKey,
        state: SessionState,          // Working | Blocked{kind} | AwaitingInput{stop_reason} | Done | Failed | Unknown (ACP-aligned, §7)
        authority: Authority,         // Heuristic | Signal | Authoritative
        observed_at: SystemTime,      // replaces "transcript mtime" as the clock
        message: Option<String>,      // toast body / last assistant text
        detail: Option<String>,       // e.g. "approval: rm -rf build/"
    },
    Activity { key: SessionKey, at: SystemTime },
    EditedFiles { key: SessionKey, recent: Vec<PathBuf> },
}

struct SessionKey { harness: HarnessId, location: LocationId, id: SessionId }
```

- **`SessionKey` includes the location.** The catalog stops looking sessions up by id alone. Two locations, or two harnesses, can share an id without colliding.
- **`SessionMeta`** keeps everything `Session` already holds minus the file assumptions. `transcript_path` becomes optional, harness-private data. `project_dir` becomes `Option<WorkDir { location, path }>`, so the "open a terminal / tool / git status in the session's directory" features stay available only when a directory exists.
- **New states, kept out of the existing enum.** `Done` and `Failed` are added for API harnesses that report terminal states. Today's `Attention` (`NeedsInput` / `Working` / `Idle` / `Unknown`) plus `blocking_prompt` becomes a display mapping of `SessionState`. The notifier keeps triggering on "entered AwaitingInput / Blocked / Done / Failed" and stays agent-neutral.

### 4.3 Precedence rules

Today's hook "pin" rule, restated for the new model:

- **Authoritative** status (an API's own `state` field) always wins and clears any pin.
- **Signal** status (a hook, a bell) pins the state until the source reports a newer `observed_at` that has moved past it. This is today's mtime rule, generalized from "transcript mtime" to "source clock".
  - A harness can mark its heuristic `Working` reports as **not** releasing a pin. That is today's `from_tool_use` flag, renamed `Status { .. }.holds_pin` so its meaning is explicit.
- **Heuristic** status applies when nothing above it is pinned.
- **Signals for unknown keys** go into a pending table with a time limit and are applied on `Upsert`. This is the 2026-10-02 bug fix, generalized.

### 4.4 Capabilities a harness provides

The trait shape is deliberately split, so that a harness implements only what it has:

```rust
trait Harness: Send + Sync {
    fn id(&self) -> &HarnessId;
    fn label(&self) -> &str;                       // "codex" — UI tag, config key
    /// Build the source for one location. Owns its thread; emits SourceEvents.
    fn source(&self, loc: &Location, cfg: &HarnessConfig, tx: EventSender)
        -> Result<Box<dyn Source>, HarnessError>;
    fn attach(&self) -> Option<&dyn Attach>        { None }
    fn spawn(&self) -> Option<&dyn Spawn>          { None }
    fn signals(&self) -> Option<&dyn SignalSetup>  { None }
    fn doctor(&self) -> Option<&dyn Doctor>        { None }
    fn notify(&self) -> Option<&dyn NotifyFormat>  { None }
}
```

| Capability | Contract | File-backed default (claude/codex/pi) | Service/adapter example |
|---|---|---|---|
| **Source** | Discover and observe sessions; emit `Upsert`/`Status`/…; never block the UI thread. | `TranscriptSource<AgentParser>`: today's listing, `notify` *and* the mtime-poll backstop (lesson 1), and tail parsing via the existing per-agent parsers. | `HttpSource`: polls the API, or subscribes to it if supported, and maps API states to `SessionState` with `Authority::Authoritative`. |
| **Attach** | Return an `AttachPlan`, computed ahead of time and cached on the session so switching never does I/O. | `Tmux { target }`, built through today's pane resolution and resume fallback. | `Command { argv, location }` (e.g. `worktool tasks attach <id>` streaming in the embedded PTY), `Url { url }` (opened externally), or `None { reason }`. |
| **Spawn** | Create a session. | `PinnedId` (claude, pi), `DiscoverAfterSpawn` (codex), unchanged. | `Created { key }` returned by an API call made on a background thread; the row appears through `Upsert`. |
| **SignalSetup** | Install and verify an out-of-band producer; its output contract is documented. | claude: Notification hook. codex: `PermissionRequest` hook plus a trust check. Possible later: the bell signal from TODO.md. | Usually none, since the API is already authoritative. Optionally webhook or SSE registration. |
| **Doctor** | Background health checks, shown in `agent-mux doctor` and as footer hints. | "codex hook installed and trusted on host X", "transcript root present". | "token valid", "API reachable", "API version supported". |
| **NotifyFormat** | Optionally shape notification title, body and urgency from `SessionMeta` plus the `Status` message. | Default formatter (today's). | Task name in the title, a link to the task URL. |

Notes on the table:
- **Attach and `LivePanes`.** The live-pane indicator becomes three-valued (`Live` / `WillResume` / `NotApplicable`), so non-tmux sessions don't render as permanently "dimmed, will resume".
- **Spawn in the new-session flow.** Worktree creation becomes a spawn *option* that only machine locations offer.

### 4.5 Three ways to provide a harness

1. **Built-in Rust module** (`src/harnesses/<name>.rs`). Today's three agents move here: they keep their parsers and gain `TranscriptSource`. Best when deep integration matters.
2. **External adapter process**, the recommended way to plug in a work or private harness. agent-mux starts a configured command, one process per (harness, location), and speaks a versioned JSON-lines protocol over its stdio:
   ```
   → {"v":1,"method":"initialize","params":{"location":"work-api"}}
   ← {"v":1,"result":{"capabilities":["source","attach","spawn","doctor"],"poll_hint_ms":3000}}
   ← {"v":1,"event":"upsert","key":{"id":"T-123"},"meta":{"title":"fix flaky test","work_dir":null}}
   ← {"v":1,"event":"status","key":{"id":"T-123"},"state":"blocked","authority":"authoritative","observed_at":"…","detail":"needs approval: deploy to staging"}
   → {"v":1,"method":"attach_plan","params":{"key":{"id":"T-123"}}}
   ← {"v":1,"result":{"kind":"command","argv":["worktool","tasks","attach","T-123"]}}
   ```
   - **Isolation:** the HTTP client, auth, retries and API quirks live in the adapter, in whatever language the work team prefers. agent-mux never holds the credentials.
   - **Supervision:** crash means restart with backoff and a footer error, mirroring `ensure_connected`. Requests have timeouts. Capabilities are negotiated in `initialize`.
   - This is how the work harness lands without a Rust change, and without loading unsafe code into agent-mux's process.
3. **Config-only harness** (later and optional): `kind = "command"` with shell templates for `list`/`status`/`attach` that print JSON. That is the adapter protocol with polling instead of streaming. Defer until someone needs it.

### 4.6 Config

```toml
# today's agents, unchanged spelling (alias for kind = "builtin")
[agents.codex]
enabled = true

# new: harness instances
[harnesses.work]
kind = "adapter"
command = ["worktool", "agent-mux-adapter"]   # spoken to over stdio
locations = ["work-api"]                       # a service location, defined below
env = { WORKTOOL_PROFILE = "default" }         # credentials stay in the adapter's own config

[locations.work-api]
kind = "service"
label = "work"                                 # sidebar group header
```

- `AgentKind` becomes `HarnessId(Arc<str>)` plus a registry built at startup. The built-in labels still parse.
- The cache stores the raw label instead of mapping unknown labels to Claude.
- Validation changes from "known label" to "known or declared".
- Exhaustive `match` over a closed enum gives way to capability lookups. That loss is accepted, since a harness's behaviour now lives behind its trait objects.

### 4.7 Remote support, per capability

| Capability | Machine location over SSH (today's hosts) | Service location |
|---|---|---|
| Source | `TranscriptSource` through `Host` (list, tail, mtime poll). Already works for Codex. | The adapter or HTTP source talks to the service; no host involved. |
| Signals | Producer on the remote. `install-hooks --host <name>` writes the remote `hooks.json` through `Host::write_file`, pointing at the remote agent-mux binary. Marker placement uses the payload's own `transcript_path` so producer and consumer agree. | Not applicable, or the service pushes events. |
| Doctor | Remote reads on the per-host discovery thread (the remote Codex trust check). | Adapter `doctor`. |
| Attach / spawn | Today's ssh + tmux argv. | Adapter attach plan or create call. |

## 5. Migration (work packages)

Ordered so that every step is shippable and Claude-only stays unchanged. A "pure refactor" step must leave behaviour unchanged.

- **H0.** Spec amendment (SPEC.md):
  - Replace the agent-qualification rule "persists local tail-parseable transcripts + resumes by id" with "a harness provides session enumeration, state, and either an attach plan or an explicit view-only declaration".
  - Keep "Replacing tmux" out of scope for tmux-based harnesses, while allowing non-tmux attach plans for harnesses that never had tmux.
  - Revisit the exclusion of RPC/app-server modes in the 2026-07-09 plan (§5).
- **H1.** Key the catalog by `SessionKey`, i.e. by location as well as id (pure refactor).
- **H2.** Introduce `SourceEvent`, `SessionState` and `Authority`. Map today's three producers onto it, restate the pin rules as in §4.3, and add the pending-signal table. Notification triggers move from "entered NeedsInput" to the `SessionState` transitions.
- **H3.** Extract `TranscriptSource`. The watcher, discovery and pollers become one source type parameterized by an agent parser and a `Host` (pure refactor). The mtime-poll backstop and the "re-check unwatched roots" behaviour live here.
- **H4.** Split `Location` from `Host`; add `AttachPlan` and the three-valued live indicator.
- **H5.** Replace `AgentKind` with a `HarnessId` registry; harness config and cache labels.
- **H6.** Add the `Doctor` capability and the `agent-mux doctor` command. Move the Codex trust check here and add the remote version.
- **H7.** Add the adapter protocol plus a **reference adapter**: a small fake task API with a Python adapter used in tests, plus a protocol conformance test suite every harness must pass.
- **H8.** Remote `install-hooks --host`.
- Later, out of tree: the work harness as an adapter.

**Test strategy:**
- A conformance suite, golden `SourceEvent` sequences per harness, run against each built-in harness's real fixtures (Codex now has real interactive rollouts under `tests/fixtures/codex/`).
- The zero-cost **mock-model E2E rig** proven on 2026-10-02 (real `codex` in tmux against a localhost Responses-API stub, with an `osascript` shim capturing notifications) graduates into `scripts/` as a repeatable smoke test.

## 6. Risks

- **The refactor touches the spine** (watcher, catalog, `main.rs`). H1–H3 must be behaviour-preserving and land behind the existing test suite before anything new rides on them.
- **Adapter protocol versioning.** It has to be versioned from day one, with capabilities negotiated at startup.
- **Pin semantics** are the subtle part. Write them as a small pure state machine with table-driven tests before wiring them in.
- **Open-ended scope.** The SPEC criterion in H0 keeps "any agent" bounded.

## 7. Path B: wrap a general, agent-agnostic layer instead?

Investigated 2026-10-02 with hands-on tests wherever possible. Every candidate was run against a localhost mock model, so no API spend. The candidates were Pi, herdr, coder/agentapi, and ACP; the protocol work also turned up Codex's own app-server. Scratch traces are noted in TODO.md.

| Layer | What it is | State fidelity for Claude/Codex | Native TUI in tmux | Remote | Verdict |
|---|---|---|---|---|---|
| **Pi** (1.0.0) | A coding agent with providers, extensions and RPC mode | Excellent *for Pi sessions*. One extension gives working (`agent_start`), done (`agent_settled`) and blocked (`ui_prompt_start`). | Yes, for Pi itself | Pi on the remote; markers over SSH | **Not a wrapper: it replaces Claude Code and Codex.** You lose native permissions, sandbox, hooks and subagents, and Claude through Pi is billed per token as extra usage. Keep Pi as a first-class harness and use its extension as the reference "rich signals" design. |
| **herdr** (v0.9.3) | An agent-first multiplexer with its own PTY server and a socket API | Screen-scraped. Codex blocked came from the OSC title; Codex turn-end read `unknown`. No last message or edited files. | Replaces tmux; a server restart kills agents | Its own server on each host | **Not a substrate.** At most an optional *source* if users run herdr. Ideas worth borrowing: per-agent detection manifests, **OSC-title signals** (now shipped for Codex), a monotonic `seq`, and an `explain` view. |
| **coder/agentapi** (v0.12.2) | An HTTP wrapper over an emulated terminal | `running`/`stable` only. A Codex approval prompt reads `stable`, the same as done. | Fixed 80-column emulated screen | HTTP | **Not viable.** Archived, and it loses the blocked state. |
| **ACP** | A JSON-RPC client↔agent protocol; ~40 agents, Claude/Codex/Pi via adapters | Excellent. `session/request_permission` is first-class; turns end with `stopReason`. | **No.** The ACP client *is* the UI, so it can't observe a native TUI. Shared multi-client sessions (Agent Host Protocol) are early. | stdio; an HTTP/WS transport is at the RFD stage | **Borrow its vocabulary** for `SessionState` (below). An ACP *client* source is only for agents with no TUI. Gotcha: `codex-acp` defaults to a model-based auto-review of approvals. |
| **Codex app-server** (0.142) | Codex's own daemon (`app-server --listen ws://…`); the native TUI connects with `codex --remote` | **Exact and push-based.** An observer that only called `initialize` received `thread/status/changed`: `active` → `active["waitingOnApproval"]` → `active` → `idle`. Approval *requests* go only to the TUI. | Yes, the native TUI is kept in tmux | Port-forward over the existing ControlMaster | **Best Codex source when agent-mux spawns the session.** Needs a spawn-plan change and daemon lifecycle management, and doesn't cover plain `codex` started outside agent-mux. |

**Conclusion: don't wrap one universal layer.** No single layer gives agent-agnostic fidelity while keeping native TUIs in tmux:
- The general layers either replace the user's agents (Pi), replace tmux (herdr), or replace the UI (ACP).
- The one that wraps arbitrary terminal agents (agentapi, or herdr's scraping) loses exactly the states agent-mux exists to surface.

The best signals come from what **each agent already exposes in structured form**: Claude's transcript and hooks, Codex's title, hook or app-server, Pi's extension events. That is the §4 design: a normalized event model plus per-harness `Source` and signal capabilities. Path B therefore *folds into* Path A as additional sources rather than replacing it:

- **`SessionState` adopts ACP's vocabulary:** `Working`, `Blocked { kind: Approval | Input }`, `AwaitingInput { stop_reason }`, `Done`, `Failed`, `Unknown`. `Blocked.kind` distinguishes an approval from a question (`request_user_input`), which no current signal does.
- **Candidate sources under §4.4/§4.5:**
  - a Codex app-server observer (exact, push)
  - a Pi signal extension
  - an ACP client source (headless agents)
  - a herdr source (users already on herdr)
  - the HTTP/adapter source for the work harness
- **The work harness** stays an adapter-backed source (§4.5). Its API's task states map onto `SessionState` the same way A2A's `working` / `input-required` / `completed` would.

## 8. Open questions

- Does the work harness's API **push** events (webhooks, SSE) or only support polling? This decides whether `HttpSource` needs a long-lived connection.
- What does "attach" mean for a remote task: a log stream, an interactive shell in the task's sandbox, or a web URL? This decides which `AttachPlan` variants the work adapter needs first.
- Should `Done`/`Failed` sessions age out of the sidebar, as idle sessions effectively do today?
