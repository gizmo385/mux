use std::collections::{HashMap, HashSet};
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, SystemTime};

use crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use agent_mux::attachment::{
    AttachOutcome, AttachmentDriver, EmbedSpec, PtyDriver, SuspendCommand, TmuxDriver,
};
use agent_mux::cache;
use agent_mux::catalog::SessionCatalog;
use agent_mux::cli;
use agent_mux::config::{self, Config, Theme, ToolBinding};
use agent_mux::dashboard::{
    DisplayRow, Focus, SearchMode, SearchOutcome, SearchState, apply_fg, build_display_rows,
    build_display_rows_filtered, first_session_index, is_pty_leader, matches_query,
    next_host_index, next_project_index, next_session_index, prev_host_index, prev_project_index,
    prev_session_index,
};
use agent_mux::delete_worktree_modal::{
    DeleteWorktreeModal, KeyOutcome as DeleteWorktreeKeyOutcome,
};
use agent_mux::discovery::{build_session, claude_projects_dir, discover};
use agent_mux::embedded_pty::{
    EmbeddedPty, PtyEvent, encode_key_for_pty, encode_mouse_event, encode_paste,
};
use agent_mux::host::{Host, LocalHost, SshHost};
use agent_mux::new_session_modal::{KeyOutcome, NewSessionModal, NewSessionSeed};
use agent_mux::notifications::{Notifier, Transition, pick_dispatcher};
use agent_mux::repo::{Repo, RepoRegistry, scan_host_workspaces};
use agent_mux::session::{Attention, HostId, Session, SessionId};
use agent_mux::session_names::{SessionNameStore, default_store_path};
use agent_mux::tool_launches::{ToolLaunch, ToolLaunchRegistry};
use agent_mux::watcher::{REMOTE_POLL_INTERVAL, TranscriptWatcher, WatcherEvent};
use agent_mux::worktree::WorktreeManager;

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Sessions older than this are shown as Idle regardless of transcript
/// content. M0 default; will become configurable in M5.
const IDLE_THRESHOLD: Duration = Duration::from_secs(60 * 60);

/// Event-loop tick. Bounds the latency between an attention update arriving
/// in the channel and the dashboard re-rendering it.
const TICK: Duration = Duration::from_millis(100);

/// Event-loop tick while [`Focus::Terminal`] is active. The embedded
/// PTY's reader thread updates its parser asynchronously and posts a
/// `PtyEvent::Output` to wake the loop, but `event::poll` on stdin
/// doesn't observe that channel — so the worst-case latency from
/// "child wrote bytes" to "redraw" is bounded by this tick. 16 ms
/// (~60fps) feels native; 100 ms (the sidebar tick) lags visibly when
/// the user is typing.
const TICK_TERMINAL: Duration = Duration::from_millis(16);

/// Default grid the embedded PTY spawns into. Phase 4 will replace
/// this with the actual rendered area at attach time; Phase 3 spawns
/// at this size and lets vt100 / the child's SIGWINCH handler reflow.
const DEFAULT_PTY_ROWS: u16 = 24;
const DEFAULT_PTY_COLS: u16 = 80;

/// Width (in cells) of the dashboard sidebar when an embedded PTY is
/// active. Wide enough to render the host + project headers without
/// truncating common labels; narrow enough that the terminal pane
/// dominates the screen. M5 candidate for `[ui]` config.
const SIDEBAR_WIDTH: u16 = 40;

/// How long the Repo Registry's cached scan is allowed to age before the
/// next picker-open re-scans. Short enough that newly-cloned repos appear
/// without a restart; long enough that rapid open/close of the modal
/// doesn't repeat the depth-1 walk on every keystroke.
const REPO_REFRESH_TTL: Duration = Duration::from_secs(30);

fn main() -> io::Result<()> {
    // `args().skip(1)` drops argv[0] (program path). Anything left is
    // a subcommand. No subcommand → launch the TUI; that's the
    // original behaviour and the common case.
    //
    // The embedded-PTY arc landed in 2026-05; the default attach now
    // renders the active session inside agent-mux's TUI alongside the
    // sidebar (see SPEC.md / ARCHITECTURE.md for the design).
    //
    // `--no-embed` opts back into the legacy `tmux switch-client` /
    // `SuspendAndRun` behaviour for users who prefer it. `--embedded`
    // is silently accepted for one release as the inverse — it's a
    // no-op now (the default already enables it) but eats the flag
    // so any user with it aliased doesn't see an "unknown subcommand"
    // error.
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    let no_embed = argv.iter().any(|s| s == "--no-embed");
    argv.retain(|s| s != "--no-embed" && s != "--embedded");
    let embedded = !no_embed;
    let mut stdout = io::stdout();
    match argv.first().map(String::as_str) {
        None => run_tui(embedded),
        Some("themes") => cli::print_themes(&mut stdout, stdout_is_terminal()),
        Some("config") => {
            let searched = config::config_search_paths();
            let loaded_from = searched.iter().find(|p| p.exists()).cloned();
            let result = match &loaded_from {
                Some(p) => Config::load_from(p),
                None => Ok(Config::default()),
            };
            cli::print_config(&mut stdout, &searched, loaded_from.as_deref(), &result)
        }
        Some("notify-test") => {
            let cfg = Config::load().unwrap_or_default();
            let (dispatcher, backend_label) = pick_dispatcher(cfg.notifications.backend);
            let notifier = Notifier::new(dispatcher, cfg.notifications);
            cli::print_notify_test(&mut stdout, &notifier, backend_label)
        }
        Some("hook") => {
            // Producer side of the Claude Code Notification hook
            // ingress. Reads payload from stdin; if the event is
            // input-required (permission/idle/elicitation prompt),
            // writes a marker file to
            // `<transcripts-root>/.agent-mux-hooks/<unix-millis>-<sid>.json`.
            // The transcripts root comes from the payload's
            // `transcript_path` field — same path shape on local and
            // remote machines, so the local watcher and the remote
            // poller find markers at the same relative location. The
            // cache-dir fallback only fires for malformed payloads
            // missing `transcript_path` (production payloads always
            // include it).
            let fallback = agent_mux::hook_ingest::fallback_hook_dir()
                .ok_or_else(|| io::Error::other("no cache directory resolved on this platform"))?;
            let mut stderr = io::stderr();
            match agent_mux::hook_ingest::receive_hook_from_stdin(
                &mut io::stdin().lock(),
                &fallback,
                SystemTime::now(),
                &mut stderr,
            )? {
                Some(path) => writeln!(
                    stdout,
                    "agent-mux: hook marker written to {}",
                    path.display()
                ),
                None => Ok(()),
            }
        }
        Some("install-hooks") => {
            // Mutates ~/.claude/settings.json to wire the Notification
            // hook command at this binary's path. Idempotent; updates
            // a stale entry in place; preserves everything else in the
            // user's settings file. `--dry-run` prints the planned
            // content without writing.
            let dry_run = argv.iter().any(|s| s == "--dry-run");
            let settings = agent_mux::hook_install::default_settings_path()
                .ok_or_else(|| io::Error::other("no home directory resolved on this platform"))?;
            let binary = std::env::current_exe()?;
            agent_mux::hook_install::install_hooks_at(&settings, &binary, dry_run, &mut stdout)
        }
        Some("help" | "--help" | "-h") => cli::print_help(&mut stdout),
        Some(other) => {
            let mut stderr = io::stderr();
            writeln!(stderr, "agent-mux: unknown subcommand {other:?}\n")?;
            cli::print_help(&mut stderr)?;
            std::process::exit(2);
        }
    }
}

fn run_tui(embedded: bool) -> io::Result<()> {
    // The driver choice is decided once at boot and stays for the life
    // of the process — there's no in-app switcher. Dogfooders flip the
    // flag between runs.
    let driver: Box<dyn AttachmentDriver> = if embedded {
        Box::new(PtyDriver::new())
    } else {
        Box::new(TmuxDriver::new())
    };
    let mut app = App::new(driver, embedded)?;
    let mut terminal = setup_terminal(embedded)?;
    let result = run(&mut terminal, &mut app);
    restore_terminal(&mut terminal, embedded)?;
    result
}

/// Whether to emit ANSI escapes from non-TUI subcommands. When stdout
/// is a real terminal we want the colour swatch; piped into a file or
/// pager that doesn't strip escapes we want plain text.
fn stdout_is_terminal() -> bool {
    use std::io::IsTerminal;
    io::stdout().is_terminal()
}

fn enter_screen(embedded: bool) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    if embedded {
        // Mouse + bracketed paste are only enabled in embedded mode.
        // Non-embedded runs preserve the terminal's normal text-
        // selection-with-mouse behaviour. Inside embedded mode the
        // user expects mouse to work against the running child; native
        // selection still works behind Shift.
        execute!(stdout, EnableMouseCapture, EnableBracketedPaste)?;
    }
    // Focus reporting (DEC 1004): emits `ESC [I` / `ESC [O` on the
    // terminal-window-focus-gain/loss boundary, which the notifier
    // uses to decide whether the user is actually looking at
    // agent-mux. Enabled in both embedded and non-embedded modes —
    // attention suppression matters either way. Terminals that don't
    // implement focus reporting silently drop the request and the
    // app falls back to "always notify" (the `terminal_focused`
    // field defaults to false).
    execute!(stdout, EnableFocusChange)?;
    Ok(())
}

fn leave_screen(embedded: bool) -> io::Result<()> {
    let mut stdout = io::stdout();
    execute!(stdout, DisableFocusChange)?;
    if embedded {
        execute!(stdout, DisableBracketedPaste, DisableMouseCapture)?;
    }
    execute!(stdout, LeaveAlternateScreen)?;
    disable_raw_mode()?;
    Ok(())
}

fn setup_terminal(embedded: bool) -> io::Result<Tui> {
    enter_screen(embedded)?;
    Terminal::new(CrosstermBackend::new(io::stdout()))
}

fn restore_terminal(terminal: &mut Tui, embedded: bool) -> io::Result<()> {
    leave_screen(embedded)?;
    terminal.show_cursor()?;
    Ok(())
}

/// Release the alt-screen and raw mode so a foreground subprocess can use
/// the terminal, run it, then re-enter and force a redraw. Used when the
/// attachment driver hands a command off (e.g. `tmux attach` from outside
/// tmux, or `$SHELL` for an outside-tmux spawn-terminal).
fn suspend_and_run(
    terminal: &mut Tui,
    cmd: &SuspendCommand,
    embedded: bool,
) -> io::Result<Option<String>> {
    leave_screen(embedded)?;
    let mut process = Command::new(&cmd.program);
    process.args(&cmd.args);
    if let Some(cwd) = &cmd.cwd {
        process.current_dir(cwd);
    }
    let status = process.status();
    enter_screen(embedded)?;
    terminal.clear()?;
    match status {
        Ok(_) => Ok(None),
        Err(e) => Ok(Some(format!("{}: {e}", cmd.program))),
    }
}

struct App {
    catalog: SessionCatalog,
    list_state: ListState,
    home: Option<PathBuf>,
    /// All known Host backends keyed by `HostId`. Populated synchronously
    /// with `LocalHost` at startup; remote `SshHost`s land here as their
    /// background connect threads succeed. Lifetime-extends each `SshHost`
    /// so its `Drop` (which runs `ssh -O exit`) fires on app teardown.
    hosts: HashMap<HostId, Arc<dyn Host>>,
    watcher: TranscriptWatcher,
    /// Background watcher for Claude Code hook marker files (Phase 1
    /// of the Notification-hook integration). Held only so its
    /// `notify::RecommendedWatcher` isn't dropped — the watcher feeds
    /// `WatcherEvent::Hook` into the same channel as the transcript
    /// watcher, drained by `drain_updates`. `None` when the cache
    /// directory couldn't be resolved or the notify backend rejected
    /// the watch; that case leaves agent-mux on the heuristic-only
    /// attention path with an eprintln at startup.
    _hook_watcher: Option<notify::RecommendedWatcher>,
    updates: Receiver<WatcherEvent>,
    driver: Box<dyn AttachmentDriver>,
    status: Option<String>,
    config: Config,
    registry: RepoRegistry,
    modal: Option<NewSessionModal>,
    /// Open delete-worktree confirmation modal. Kept as a sibling of
    /// `modal` rather than collapsing both into one `enum` variant —
    /// the open paths guard against opening one while the other is up,
    /// so the at-most-one invariant is upheld at the call sites with
    /// no shared `take()` plumbing to refactor.
    delete_modal: Option<DeleteWorktreeModal>,
    create_tx: Sender<NewSessionResult>,
    create_rx: Receiver<NewSessionResult>,
    creating: Option<CreatingSession>,
    /// Channel for completed worktree deletions. Same pattern as
    /// `create_tx/rx`: spawn a background thread for the slow git call,
    /// post the result here, drain on tick.
    delete_tx: Sender<DeleteWorktreeResult>,
    delete_rx: Receiver<DeleteWorktreeResult>,
    /// Channel for background remote-workspace scan results. Each
    /// `Connected` event spawns a scan thread for the host's
    /// `effective_workspace_folders`; the result lands here and
    /// reconciles into the [`RepoRegistry`] in the main loop. Kept
    /// separate from `remote_rx` so a slow workspace scan can't
    /// back-pressure session discovery.
    repo_scan_tx: Sender<RepoScanResult>,
    repo_scan_rx: Receiver<RepoScanResult>,
    /// Background SSH discovery results stream in here for the lifetime of
    /// the app — one message per configured host (success or failure).
    /// Drained each tick; once every configured host has reported, the
    /// channel goes quiet but stays open in case future chunks reconnect.
    remote_rx: Receiver<RemoteDiscoveryResult>,
    /// How many configured SSH hosts have not yet reported. Used by the
    /// footer to show "connecting to N host(s)…" while at least one is
    /// outstanding.
    pending_hosts: usize,
    /// Sticky list of hosts whose startup connect failed, with the full
    /// error message captured for future surfacing (logs, a detail
    /// modal). The footer renders just the host names so a multi-host
    /// failure doesn't blow up into an unreadable line; the messages
    /// stay around so a connect failure isn't silently lost when the
    /// transient `status` field cycles to a different message.
    connect_errors: Vec<(HostId, String)>,
    /// Hosts whose remote workspace scan completed with zero repos.
    /// Carries the folders the scan visited so the footer hint can be
    /// specific: "no repos in <folders> on <host>." Cleared when a
    /// subsequent (non-empty) scan lands for the same host. Surfaces
    /// the silent-empty case dogfooding hit 2026-05-19 — a configured
    /// path that doesn't exist (or contains no `.git/` children) on
    /// the remote box looked indistinguishable from "host hasn't
    /// connected yet" before this.
    empty_scans: Vec<(HostId, Vec<PathBuf>)>,
    /// Captured at startup: `$TMUX` set means we attach via `tmux
    /// switch-client` (no parent/child relationship to break), so the return
    /// hint differs from the outside-tmux case where we suspend and run
    /// `tmux attach` as a subprocess that `prefix+d` cleanly exits.
    in_tmux: bool,
    /// Dashboard search/filter state. `None` when no filter is active.
    /// `Some(Editing)` means the search bar owns the keyboard; `Some(Active)`
    /// means the filter persists but normal navigation works. See
    /// [`SearchState`] for the full mode contract.
    search: Option<SearchState>,
    /// M4: fires OS notifications on session attention transitions
    /// into `NeedsInput`. Owns its own per-session suppression state.
    notifier: Notifier,
    /// Resolved (parsed) M5 theme colours. Stored on the app so render
    /// paths look up `Option<Color>` from a typed struct rather than
    /// re-parsing strings every frame.
    theme: Theme,
    /// The single embedded PTY slot. `Some` when the user has attached
    /// via `PtyDriver`; `None` for `TmuxDriver` runs and for the
    /// initial state of every run. We deliberately do not pre-warm or
    /// hot-cache other sessions — the Plan-agent-decided shape is a
    /// *swappable* PTY, killed and respawned when the user attaches to
    /// a different session. Pivot to N hot PTYs only if dogfooding
    /// shows the swap cost is visible.
    embedded: Option<Embedded>,
    /// Input routing for the embedded-PTY arc. Default `Sidebar` keeps
    /// the legacy keybinds active. Transitions to `Terminal` on a
    /// successful `EmbedPty` attach; back to `Sidebar` on prefix-escape
    /// (`Ctrl-a Esc`) or PTY exit.
    focus: Focus,
    /// Whether agent-mux's terminal window currently owns OS-level
    /// focus. Driven by DEC 1004 focus-reporting events
    /// (`Event::FocusGained` / `Event::FocusLost`); enabled in
    /// [`enter_screen`]. Used by the notification path to suppress
    /// toasts only when the user is *both* watching the embedded
    /// session and has the terminal focused — `Focus::Terminal`
    /// alone isn't enough, since an alt-tab away leaves the in-app
    /// focus state intact.
    ///
    /// Defaults to `false` so terminals that don't implement focus
    /// reporting silently fall back to "always notify" — the safe
    /// failure mode (missed suppression is annoying; missed
    /// notification is the bug we exist to avoid). A terminal that
    /// does support focus reporting will send `FocusGained` on app
    /// startup, lifting this to `true` within the first event tick.
    terminal_focused: bool,
    /// Whether the binary was launched with `--embedded`. Drives the
    /// mouse-capture + bracketed-paste opt-ins at terminal-setup time
    /// and gates the corresponding input routing in the main loop.
    /// Static for the life of the run (no in-app toggle).
    embedded_mode: bool,
    /// Running `[[tools]]` launches surfaced in the sidebar's "Tools"
    /// group. Each entry holds the tmux session name agent-mux
    /// assigned at spawn time; Enter on a tool row re-attaches the
    /// embedded pane to that tmux session. Pruned on attach failure
    /// (no background poller — failure surfaces at the next user
    /// interaction).
    tool_launches: ToolLaunchRegistry,
    /// Persistent user overrides for the sidebar's display title.
    /// Keyed by `(host, session_id)`; loaded at startup from
    /// `~/.cache/agent-mux/session_names.json`. The `r` keybind opens
    /// an inline edit overlay (see [`Self::rename`]) whose result
    /// flows through this store. Overrides take precedence over the
    /// transcript's `aiTitle` / `task.toml` title.
    session_names: SessionNameStore,
    /// Active rename overlay. `Some` when the user pressed `r` on a
    /// session row and is editing the new name; `None` otherwise.
    /// While `Some`, every keystroke routes through
    /// [`Self::route_rename_key`] rather than the normal action
    /// dispatch.
    rename: Option<RenameState>,
}

/// In-progress session-name rename. The target is captured at `r`
/// time so the user can navigate (or even let discovery re-sort the
/// list) without losing the row they meant to rename. The buffer is
/// what they've typed so far; flushing to disk happens on `Enter`.
#[derive(Debug, Clone)]
struct RenameState {
    host: HostId,
    session_id: SessionId,
    buffer: String,
}

/// Owns the embedded pseudoterminal and remembers which session it's
/// for. The `session_id` field is what lets the attach path skip a
/// respawn when the user presses Enter on the same row twice — a
/// common gesture after a prefix-escape and re-attach.
///
/// `last_size` is the (rows, cols) the PTY was last resized to. The
/// draw path compares the current rendered area against this and
/// fires `EmbeddedPty::resize` only on change — sending SIGWINCH on
/// every frame would be 60 wakeups/sec for nothing.
///
/// `last_inner` is the `Rect` of the PTY's content area (inside the
/// block border) from the most recent draw — used by the mouse event
/// handler to translate terminal-absolute click coordinates into
/// PTY-relative ones. `None` until the first frame renders.
struct Embedded {
    pty: EmbeddedPty,
    session_id: SessionId,
    last_size: (u16, u16),
    last_inner: Option<ratatui::layout::Rect>,
    /// Set when this PTY hosts a tool launch (the tool wraps its
    /// command in a detached tmux session via
    /// `spawn_tool_*_embed`). On PTY exit the dashboard uses this to
    /// prune the corresponding `ToolLaunchRegistry` entry so the
    /// sidebar row vanishes.
    tool_tmux_session: Option<String>,
}

/// Lives in `App.creating` while a worktree-create is in flight, so the
/// footer can show "creating worktree for X in Y…".
struct CreatingSession {
    repo_name: String,
    task: String,
}

/// Result from the background create thread. Drained on every tick.
/// `Created` carries both the worktree path and the `HostId` it was
/// created on so the main thread can route `spawn_session` through the
/// right `Arc<dyn Host>` — the path alone doesn't tell us whether to
/// invoke claude locally or via SSH.
enum NewSessionResult {
    Created { host_id: HostId, path: PathBuf },
    Failed(String),
}

/// Result of a background worktree-deletion thread. `Deleted` carries
/// the session id so `drain_deletes` can call `remove_by_id` without
/// re-resolving the selection (which may have moved by the time the
/// background thread reports). `Failed` carries the message verbatim
/// from `WorktreeError::Display` so the user sees git's stderr —
/// most useful when the failure is "uncommitted changes, use --force"
/// and the user needs to re-run with the force toggle.
enum DeleteWorktreeResult {
    Deleted(SessionId),
    Failed(String),
}

/// One remote host's freshly-scanned repo list, returned from the
/// background thread spawned on `Connected`. Drained each tick; the
/// dashboard `reconcile_host`s the registry with the result so the
/// next picker-open shows live remote repos. An empty `repos` is a
/// valid value (host has no workspaces configured, or none of the
/// configured ones contain repos).
struct RepoScanResult {
    host_id: HostId,
    repos: Vec<Repo>,
    /// The workspace folders the scan visited. Echoed back from the
    /// spawn site so [`App::drain_repo_scans`] can surface a sticky
    /// "no repos found in <folders> on <host>" footer hint when
    /// `repos` is empty — the silent failure that bit the 2026-05-19
    /// dogfood session (configured paths simply didn't exist on the
    /// remote box).
    folders_scanned: Vec<PathBuf>,
}

/// Payload from a background SSH-discovery thread. The two-phase shape
/// is load-bearing for UX: `Connected` fires as soon as the SSH
/// `ControlMaster` is up (a few seconds) so cached remote sessions
/// become *attachable* immediately — without it, the user stares at
/// rows they can't enter for the entire duration of the slower
/// discovery phase. `Ready` lands later with fresh discovered sessions
/// and reconciles the catalog against them.
///
/// `Connected` carries the freshly-connected `Arc<dyn Host>` so it gets
/// inserted into `App.hosts` (making attach/spawn-terminal work) and
/// the polling threads can start ticking against current catalog state
/// rather than waiting for a fresh discovery pass that might still
/// have minutes to go on a high-latency proxy.
///
/// `Failed` carries the host's dashboard label and a one-line error so
/// the footer can show why a host is missing from the catalog —
/// covers both connect-time failure (no `Connected` emitted before it)
/// and discovery-time failure (host stays registered, but no fresh
/// sessions overlay the cached entries this run).
enum RemoteDiscoveryResult {
    Connected {
        host_id: HostId,
        host: Arc<dyn Host>,
        transcript_root: PathBuf,
    },
    Ready {
        host_id: HostId,
        sessions: Vec<Session>,
    },
    Failed {
        host_id: HostId,
        error: String,
    },
}

impl App {
    // App::new is a long but linear constructor — every line wires
    // one independent piece of startup state and the function exists
    // to be read top-to-bottom. Further extraction would just hide
    // statements behind helpers without simplifying.
    #[allow(clippy::too_many_lines)]
    fn new(driver: Box<dyn AttachmentDriver>, embedded_mode: bool) -> io::Result<Self> {
        // Config drives both the cache-load (which `[hosts.<name>]`
        // do we have snapshots for) and the SSH-discovery spawn loop,
        // so it's loaded first.
        let config = Config::load().unwrap_or_default();
        let cache_dir = cache::default_dir();

        let local_host: Arc<dyn Host> = Arc::new(LocalHost::new());
        let projects_root = claude_projects_dir();
        let mut catalog = SessionCatalog::new();
        if let Some(root) = projects_root.as_ref()
            && let Ok(sessions) = discover(local_host.as_ref(), root)
        {
            catalog.replace_all(sessions);
        }

        // Seed the catalog with cached remote sessions so the
        // dashboard renders them on first paint, instead of waiting
        // for each `ControlMaster` handshake to complete. Each host's
        // live discovery thread will `reconcile_host` later, dropping
        // entries that no longer exist on the remote and refreshing
        // attention/title on the rest. Caching is best-effort —
        // missing/corrupt files silently yield empty lists.
        if let Some(dir) = cache_dir.as_ref() {
            for name in config.hosts.keys() {
                let host_id = HostId(name.clone());
                for session in cache::read_for_host(dir, &host_id) {
                    catalog.add(session);
                }
            }
        }

        // Only local transcripts go into the `notify` watcher seed —
        // remote attention is driven by per-host polling threads
        // spun up when each live SSH discovery succeeds. A cached
        // remote session sitting in the catalog has no live host
        // yet, so feeding its path here would do nothing useful
        // (no local file at that path) and could confuse the
        // watcher.
        let targets: Vec<_> = catalog
            .sessions()
            .iter()
            .filter(|s| s.host.is_local())
            .map(|s| (s.id.clone(), s.transcript_path.clone()))
            .collect();
        let (watcher, updates) =
            TranscriptWatcher::start(Arc::clone(&local_host), targets, projects_root.as_deref())
                .map_err(io::Error::other)?;

        let mut list_state = ListState::default();
        let initial_rows = build_display_rows(catalog.sessions());
        if let Some(i) = first_session_index(&initial_rows) {
            list_state.select(Some(i));
        }

        let registry = RepoRegistry::from_config(&config);
        let (create_tx, create_rx) = channel();
        let (delete_tx, delete_rx) = channel();
        let (repo_scan_tx, repo_scan_rx) = channel();

        let mut hosts: HashMap<HostId, Arc<dyn Host>> = HashMap::new();
        hosts.insert(local_host.id().clone(), Arc::clone(&local_host));

        let (remote_tx, remote_rx) = channel();
        let pending_hosts = config.hosts.len();
        for (name, host_config) in &config.hosts {
            let host_id = HostId(name.clone());
            let ssh_target = host_config.ssh.clone();
            let transcript_root = host_config.transcript_root.clone();
            let tx = remote_tx.clone();
            let cache_dir_for_thread = cache_dir.clone();
            std::thread::spawn(move || {
                connect_and_discover(
                    host_id,
                    ssh_target,
                    &transcript_root,
                    cache_dir_for_thread,
                    &tx,
                );
            });
        }
        drop(remote_tx); // close channel once every thread Sender clone is gone

        // Local pane-presence polling; remotes get theirs in
        // `drain_remote_discoveries` once each `SshHost` is connected.
        watcher.start_pane_polling_host(Arc::clone(&local_host), REMOTE_POLL_INTERVAL);

        let notifier = build_notifier(&config.notifications);
        let theme = Theme::from_config(&config.theme).map_err(io::Error::other)?;

        Ok(Self {
            catalog,
            list_state,
            home: dirs::home_dir(),
            hosts,
            _hook_watcher: init_hook_watcher(&watcher.event_sender()),
            watcher,
            updates,
            driver,
            status: None,
            config,
            registry,
            modal: None,
            delete_modal: None,
            create_tx,
            create_rx,
            delete_tx,
            delete_rx,
            repo_scan_tx,
            repo_scan_rx,
            creating: None,
            remote_rx,
            pending_hosts,
            connect_errors: Vec::new(),
            empty_scans: Vec::new(),
            in_tmux: std::env::var_os("TMUX").is_some(),
            search: None,
            notifier,
            theme,
            embedded: None,
            focus: Focus::default(),
            terminal_focused: false,
            embedded_mode,
            tool_launches: ToolLaunchRegistry::new(),
            session_names: load_session_names_or_empty(),
            rename: None,
        })
    }

    /// Begin a rename for the currently-selected session row.
    /// No-op when the cursor isn't on a session row (header, tool
    /// row, or empty list). The initial buffer is the current
    /// override if one exists, else empty — letting the user either
    /// tweak their existing rename or start fresh from a session
    /// whose displayed title is the AI/branch fallback.
    fn open_rename(&mut self) {
        let Some(session) = self.selected_session() else {
            return;
        };
        let initial = self
            .session_names
            .get(&session.host, &session.id)
            .unwrap_or("")
            .to_string();
        self.rename = Some(RenameState {
            host: session.host.clone(),
            session_id: session.id.clone(),
            buffer: initial,
        });
        self.status = None;
    }

    /// Route a key while the rename overlay is active. Returns
    /// `true` when the key was consumed (the run loop should skip
    /// the normal action dispatch); `false` only for keys the
    /// overlay doesn't recognise — defensive, the current handler
    /// matches Enter / Esc / Backspace / Char and nothing else.
    fn route_rename_key(&mut self, key: KeyEvent) -> bool {
        let Some(state) = self.rename.as_mut() else {
            return false;
        };
        match key.code {
            KeyCode::Enter => {
                let buf = std::mem::take(&mut state.buffer);
                let host = state.host.clone();
                let id = state.session_id.clone();
                self.rename = None;
                self.session_names.set(&host, &id, buf);
                true
            }
            KeyCode::Esc => {
                self.rename = None;
                true
            }
            KeyCode::Backspace => {
                state.buffer.pop();
                true
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                state.buffer.push(c);
                true
            }
            _ => false,
        }
    }

    /// Display rows for the *current* view — filtered when search is
    /// active with a non-empty query, otherwise the full layout.
    /// Centralised here so every consumer (draw, navigation,
    /// selection resolution) sees the same set of rows; otherwise a
    /// j/k stroke could walk a list shape the user can't actually see.
    fn current_rows(&self) -> Vec<DisplayRow> {
        let session_rows = match self.search.as_ref() {
            Some(s) if !s.query.is_empty() => {
                let q = s.query.to_lowercase();
                let sessions = self.catalog.sessions();
                build_display_rows_filtered(sessions, |i| matches_query(&sessions[i], &q))
            }
            _ => build_display_rows(self.catalog.sessions()),
        };
        // Surface the Tools group above sessions when one or more
        // launches are running. Search filtering doesn't affect this
        // group — tool launches don't carry transcripts and the user
        // expects them to remain visible while narrowing the session
        // list.
        if self.tool_launches.is_empty() {
            return session_rows;
        }
        let mut rows = Vec::with_capacity(session_rows.len() + self.tool_launches.len() + 1);
        rows.push(DisplayRow::ToolsHeader);
        for i in 0..self.tool_launches.len() {
            rows.push(DisplayRow::ToolRow(i));
        }
        rows.extend(session_rows);
        rows
    }

    /// Open the search bar in Editing mode. If a filter is already
    /// active (Active mode), this returns the user to Editing with
    /// the existing query preserved — typing extends what they had.
    fn open_search(&mut self) {
        match self.search.as_mut() {
            Some(s) => s.mode = SearchMode::Editing,
            None => self.search = Some(SearchState::new()),
        }
        self.status = None;
    }

    /// Drop the search filter entirely. The previously-selected
    /// session (if it still exists in the catalog) stays selected in
    /// the unfiltered view; otherwise selection re-seats to the first
    /// session row.
    fn exit_search(&mut self) {
        let prior = self.selected_session().map(|s| s.id.clone());
        self.search = None;
        self.reseat_selection_to(prior.as_ref());
    }

    /// Route a key event to the search bar while it owns the
    /// keyboard. Returns `true` if the key was consumed (Editing
    /// mode), `false` if the caller should continue with normal
    /// dispatch. Mutates selection on every edit so the highlight
    /// follows the live filter.
    fn route_search_editing_key(&mut self, key: KeyEvent) -> bool {
        // Bail out if the search bar isn't holding the keyboard.
        if !matches!(
            self.search.as_ref().map(|s| s.mode),
            Some(SearchMode::Editing)
        ) {
            return false;
        }
        // Capture selection *before* mutating the search state — the
        // helper borrows `self` and would alias the mut-borrow below.
        let prior = self.selected_id_for_reseat();
        let Some(search) = self.search.as_mut() else {
            // Should be unreachable thanks to the Editing-mode check
            // above, but staying defensive avoids a panic if the
            // pattern ever drifts.
            return false;
        };
        let outcome = search.handle_editing_key(key);
        match outcome {
            SearchOutcome::Exit => {
                self.search = None;
                self.reseat_selection_to(prior.as_ref());
            }
            SearchOutcome::Commit | SearchOutcome::Edited => {
                self.reseat_selection_to(prior.as_ref());
            }
        }
        true
    }

    /// Route a key event that lands while search is in Active mode.
    /// Esc clears the filter; `/` returns to Editing with the existing
    /// query. Returns `true` if consumed so the caller knows to skip
    /// normal action dispatch.
    fn route_search_active_key(&mut self, key: KeyEvent) -> bool {
        if !matches!(
            self.search.as_ref().map(|s| s.mode),
            Some(SearchMode::Active)
        ) {
            return false;
        }
        match key.code {
            KeyCode::Esc => {
                self.exit_search();
                true
            }
            KeyCode::Char('/') => {
                if let Some(s) = self.search.as_mut() {
                    s.mode = SearchMode::Editing;
                }
                true
            }
            _ => false,
        }
    }

    /// Capture the currently-selected session id *before* an operation
    /// that may reshape `current_rows`. Used by [`reseat_selection_to`]
    /// to keep selection on the same session across filter changes.
    fn selected_id_for_reseat(&self) -> Option<SessionId> {
        self.selected_session().map(|s| s.id.clone())
    }

    /// After a change that may have reshaped `current_rows`, re-seat
    /// selection: if the previously-selected session is still visible,
    /// move the highlight to its new row index; otherwise fall back
    /// to the first session row (or `None` if the filter is empty).
    fn reseat_selection_to(&mut self, prior: Option<&SessionId>) {
        let rows = self.current_rows();
        let sessions = self.catalog.sessions();
        let new_idx = prior
            .and_then(|id| {
                rows.iter().position(|r| match r {
                    DisplayRow::SessionRow(i) => sessions[*i].id == *id,
                    _ => false,
                })
            })
            .or_else(|| first_session_index(&rows));
        self.list_state.select(new_idx);
    }

    fn drain_remote_discoveries(&mut self) {
        while let Ok(result) = self.remote_rx.try_recv() {
            match result {
                RemoteDiscoveryResult::Connected {
                    host_id,
                    host,
                    transcript_root,
                } => {
                    // Register the host as soon as its `ControlMaster`
                    // is up so cached remote sessions become attachable
                    // *before* discovery finishes. Polling threads
                    // start here too, seeded with whatever sessions
                    // for this host are already in the catalog
                    // (typically the cache snapshot from the previous
                    // run); the first poll tick is self-healing if
                    // the seed disagrees with the live remote state.
                    // `pending_hosts` does NOT decrement here — the
                    // footer's "connecting to N host(s)…" hint stays
                    // up until `Ready` lands so the user can tell
                    // fresh-discovery progress from
                    // attach-readiness.
                    let poll_seed: Vec<_> = self
                        .catalog
                        .sessions()
                        .iter()
                        .filter(|s| s.host == host_id)
                        .map(|s| (s.id.clone(), s.transcript_path.clone(), s.last_activity))
                        .collect();
                    self.hosts.insert(host_id.clone(), Arc::clone(&host));
                    self.watcher.start_polling_host(
                        Arc::clone(&host),
                        transcript_root,
                        poll_seed,
                        REMOTE_POLL_INTERVAL,
                    );
                    // Pane-presence polling on the same cadence.
                    // Separate thread (not coupled to the transcript
                    // poll) so a slow `tmux list-panes` over ssh
                    // can't backpressure the attention pipeline.
                    self.watcher
                        .start_pane_polling_host(Arc::clone(&host), REMOTE_POLL_INTERVAL);
                    // Remote workspace scan: separate background thread
                    // so a slow `find` over the proxy can't delay the
                    // user noticing the host is up. Results land in
                    // `repo_scan_rx` and reconcile into the registry
                    // in the next event-loop drain.
                    if let Some(host_cfg) = self.config.hosts.get(host_id.as_str()) {
                        let folders = host_cfg
                            .effective_workspace_folders(&self.config.workspace_folders)
                            .to_vec();
                        if !folders.is_empty() {
                            let host_for_scan = Arc::clone(&host);
                            let tx = self.repo_scan_tx.clone();
                            let host_id_for_scan = host_id.clone();
                            std::thread::spawn(move || {
                                let repos = scan_host_workspaces(host_for_scan.as_ref(), &folders);
                                let _ = tx.send(RepoScanResult {
                                    host_id: host_id_for_scan,
                                    repos,
                                    folders_scanned: folders,
                                });
                            });
                        }
                    }
                }
                RemoteDiscoveryResult::Ready { host_id, sessions } => {
                    self.pending_hosts = self.pending_hosts.saturating_sub(1);
                    // Capture selection *before* reconcile so we can
                    // re-seat it: a cached row the user already
                    // highlighted should stay highlighted when the
                    // live read overlays the same id; if the entry
                    // disappeared, selection falls back to the first
                    // visible session row.
                    let prior = self.selected_id_for_reseat();
                    self.catalog.reconcile_host(&host_id, sessions);
                    self.reseat_selection_to(prior.as_ref());
                }
                RemoteDiscoveryResult::Failed { host_id, error } => {
                    self.pending_hosts = self.pending_hosts.saturating_sub(1);
                    self.connect_errors.push((host_id, error));
                }
            }
        }
    }

    /// Drain finished remote workspace scans into the registry. Each
    /// host's slice is replaced wholesale via `reconcile_host`, so
    /// re-running a scan (e.g. on a future reconnect) idempotently
    /// refreshes that host without disturbing the others. An empty
    /// `repos` field flags `empty_scans` so the footer can surface
    /// the silent-empty case (configured paths that don't exist or
    /// contain no repos on the remote); a non-empty result clears any
    /// prior warning for the same host.
    fn drain_repo_scans(&mut self) {
        while let Ok(result) = self.repo_scan_rx.try_recv() {
            self.empty_scans.retain(|(h, _)| h != &result.host_id);
            if result.repos.is_empty() {
                self.empty_scans
                    .push((result.host_id.clone(), result.folders_scanned));
            }
            self.registry.reconcile_host(&result.host_id, result.repos);
        }
    }

    fn open_new_session(&mut self) {
        self.open_picker(ModalOpenMode::Worktree);
    }

    /// Open the picker in no-worktree mode (bound to `N`). Same picker
    /// UI as [`open_new_session`]; the only difference is the modal
    /// constructor — `new_no_worktree` short-circuits Filling and emits
    /// `SubmitNoWorktree` so claude launches in the repo root.
    fn open_new_session_no_worktree(&mut self) {
        self.open_picker(ModalOpenMode::NoWorktree);
    }

    fn open_picker(&mut self, mode: ModalOpenMode) {
        self.registry
            .refresh_if_stale(&self.config, REPO_REFRESH_TTL);
        if self.registry.is_empty() {
            self.status = Some(
                "no repos found. add workspace_folders to ~/.config/agent-mux/config.toml"
                    .to_string(),
            );
            return;
        }
        // Hosts with an `Arc<dyn Host>` currently registered in
        // `App.hosts` are eligible for selection; others render
        // dimmed in the picker until their `Connected` event lands.
        let ready_hosts: HashSet<HostId> = self.hosts.keys().cloned().collect();
        // Seed the picker from the cursor's current session so `n`/`N`
        // from a focused row pre-positions over the same repo (or at
        // least the same host). Worktree-backed sessions match against
        // their `parent_repo`; everything else falls back to
        // `project_dir`, which equals the repo root for non-worktree
        // checkouts.
        let seed = self.selected_session().map(|s| NewSessionSeed {
            host: s.host.clone(),
            repo_path: Some(
                s.parent_repo
                    .clone()
                    .unwrap_or_else(|| s.project_dir.clone()),
            ),
        });
        let repos = self.registry.repos().to_vec();
        let modal = match mode {
            ModalOpenMode::Worktree => NewSessionModal::new(repos, ready_hosts, seed),
            ModalOpenMode::NoWorktree => NewSessionModal::new_no_worktree(repos, ready_hosts, seed),
        };
        self.modal = Some(modal);
        self.status = None;
    }

    fn handle_modal_key(&mut self, key: KeyEvent) {
        let Some(mut modal) = self.modal.take() else {
            return;
        };
        match modal.handle_key(key) {
            KeyOutcome::Handled => self.modal = Some(modal),
            KeyOutcome::Cancel => {}
            KeyOutcome::Submit {
                repo,
                task,
                base_branch,
            } => self.start_creating(repo, task, base_branch),
            KeyOutcome::SubmitNoWorktree { repo } => self.start_no_worktree_session(repo),
        }
    }

    /// Skip `git worktree add` and spawn claude directly in the picked
    /// repo's root. Reuses the create-channel pipeline so `drain_creates`
    /// handles the spawn the same way it does for worktree-backed
    /// sessions — the only difference is that no background work runs
    /// (no `self.creating` indicator), and the synthetic `Created`
    /// carries the repo root path rather than a freshly-`git worktree
    /// add`ed path.
    fn start_no_worktree_session(&mut self, repo: Repo) {
        if !self.hosts.contains_key(&repo.host) {
            self.status = Some(format!(
                "host {} not connected yet — wait and try again",
                repo.host.as_str()
            ));
            return;
        }
        let _ = self.create_tx.send(NewSessionResult::Created {
            host_id: repo.host,
            path: repo.path,
        });
        self.status = None;
    }

    /// Dispatch a worktree creation on a background thread. The "switching
    /// never blocks on I/O" discipline applies to *any* user action, not
    /// just session-switching — `git worktree add` can take seconds on a
    /// large repo, and that must not stall the dashboard.
    fn start_creating(&mut self, repo: Repo, task: String, base_branch: String) {
        // Worktree creation now routes through `Host::run` so the same
        // code path serves local and remote — the trait dispatches.
        // The host must be registered (i.e. for SSH targets, the
        // `Connected` event must have fired); the picker greys out
        // unregistered hosts to prevent us reaching here without one,
        // but we double-check as a defensive measure.
        let Some(host) = self.hosts.get(&repo.host).cloned() else {
            self.status = Some(format!(
                "host {} not connected yet — wait and try again",
                repo.host.as_str()
            ));
            return;
        };
        self.creating = Some(CreatingSession {
            repo_name: repo.name.clone(),
            task: task.clone(),
        });
        self.status = None;
        let tx = self.create_tx.clone();
        let host_id = repo.host.clone();
        std::thread::spawn(move || {
            let outcome =
                match WorktreeManager.create(host.as_ref(), &repo.path, &base_branch, &task) {
                    Ok(path) => NewSessionResult::Created { host_id, path },
                    Err(e) => NewSessionResult::Failed(format!("create worktree: {e}")),
                };
            let _ = tx.send(outcome);
        });
    }

    /// Drain any finished creates. Returns a `SuspendCommand` if a successful
    /// create's `spawn_session` needs to hand the terminal off (outside-tmux
    /// case). Stops at the first such command — the remaining results stay
    /// queued for the next tick, since the caller can only suspend once.
    fn drain_creates(&mut self) -> Option<SuspendCommand> {
        while let Ok(result) = self.create_rx.try_recv() {
            self.creating = None;
            match result {
                NewSessionResult::Created { host_id, path } => {
                    // `spawn_session` needs the host to know whether to
                    // run claude locally or wrap it in `ssh -t target
                    // tmux new-session …`. The host should still be in
                    // `App.hosts` (we held an Arc during the create
                    // thread); the lookup-miss path is defensive.
                    let Some(host) = self.hosts.get(&host_id).cloned() else {
                        self.status = Some(format!(
                            "worktree created at {} but host {} dropped — spawn aborted",
                            path.display(),
                            host_id.as_str()
                        ));
                        continue;
                    };
                    match self.driver.spawn_session(&path, host.as_ref()) {
                        Ok(AttachOutcome::Done) => {
                            self.status =
                                Some(format!("started new session in {}", path.display()));
                        }
                        Ok(AttachOutcome::SuspendAndRun(cmd)) => {
                            self.status = None;
                            return Some(cmd);
                        }
                        Ok(AttachOutcome::EmbedPty(spec)) => {
                            // `PtyDriver::spawn_session` creates a
                            // detached tmux session and asks us to
                            // attach the embedded pane to it. We don't
                            // yet have a `SessionId` (the transcript
                            // hasn't been written), so synthesise one
                            // from the worktree path — it's unique per
                            // spawn and unlikely to collide with a
                            // real Claude conversation id. Discovery
                            // will surface the real session later;
                            // pressing Enter on that row then routes
                            // through `find_pane_local`, finds the
                            // same tmux session by cwd, and re-attaches
                            // (one PTY respawn against the same tmux
                            // session — content is preserved
                            // server-side).
                            let synthetic_id =
                                SessionId(format!("agent-mux-spawn:{}", path.display()));
                            self.install_embedded(&spec, synthetic_id);
                        }
                        Err(e) => {
                            self.status = Some(format!(
                                "worktree created at {} but spawn failed: {e}",
                                path.display()
                            ));
                        }
                    }
                }
                NewSessionResult::Failed(msg) => {
                    self.status = Some(msg);
                }
            }
        }
        None
    }

    /// Open the delete-worktree confirmation modal for the currently
    /// selected session. No-op (with a status message) if the selection
    /// isn't a worktree-backed session, the modal can't open. Also
    /// declines if either the new-session modal or another delete
    /// modal is already open — keeps the at-most-one-modal invariant.
    fn open_delete_worktree(&mut self) {
        if self.modal.is_some() || self.delete_modal.is_some() {
            return;
        }
        let Some(session) = self.selected_session().cloned() else {
            return;
        };
        if session.parent_repo.is_none() {
            // Sessions started outside a worktree (a plain checkout,
            // or `claude` against an arbitrary cwd) aren't deletable
            // through this path — there's no parent repo to run
            // `git worktree remove` against. Surface that rather than
            // opening a modal whose Enter would always fail.
            self.status = Some(format!(
                "{} is not a worktree — nothing to delete",
                session.project_dir.display()
            ));
            return;
        }
        let label = session.title.clone().unwrap_or_else(|| {
            let id_str = session.id.0.as_str();
            id_str
                .get(id_str.len().saturating_sub(6)..)
                .unwrap_or(id_str)
                .to_string()
        });
        // `for_session` only returns `None` if `parent_repo` is missing,
        // which we just ruled out — but defensively fall through to
        // status rather than unwrapping.
        match DeleteWorktreeModal::for_session(&session, label) {
            Some(modal) => {
                self.delete_modal = Some(modal);
                self.status = None;
            }
            None => {
                self.status =
                    Some("could not open delete modal — session is not a worktree".to_string());
            }
        }
    }

    fn handle_delete_modal_key(&mut self, key: KeyEvent) {
        let Some(mut modal) = self.delete_modal.take() else {
            return;
        };
        match modal.handle_key(key) {
            DeleteWorktreeKeyOutcome::Handled => self.delete_modal = Some(modal),
            DeleteWorktreeKeyOutcome::Cancel => {}
            DeleteWorktreeKeyOutcome::Submit {
                session_id,
                host_id,
                parent_repo,
                worktree_path,
                force,
            } => self.start_deleting(session_id, &host_id, parent_repo, worktree_path, force),
        }
    }

    /// Dispatch the actual `git worktree remove` on a background
    /// thread. Mirrors `start_creating`: the UI thread must never
    /// block on a multi-second SSH round-trip, and a delete on a
    /// large worktree can be measurably slow even locally.
    fn start_deleting(
        &mut self,
        session_id: SessionId,
        host_id: &HostId,
        parent_repo: PathBuf,
        worktree_path: PathBuf,
        force: bool,
    ) {
        let Some(host) = self.hosts.get(host_id).cloned() else {
            self.status = Some(format!(
                "host {} not connected — cannot delete worktree",
                host_id.as_str()
            ));
            return;
        };
        self.status = Some(format!("deleting worktree at {}…", worktree_path.display()));
        let tx = self.delete_tx.clone();
        std::thread::spawn(move || {
            let outcome =
                match WorktreeManager.remove(host.as_ref(), &parent_repo, &worktree_path, force) {
                    Ok(()) => DeleteWorktreeResult::Deleted(session_id),
                    Err(e) => DeleteWorktreeResult::Failed(format!("delete worktree: {e}")),
                };
            let _ = tx.send(outcome);
        });
    }

    /// Drain any finished deletes. On success, remove the session
    /// from the catalog so the row vanishes immediately, and — if
    /// the embedded PTY was attached to the deleted session — drop
    /// it and return focus to the sidebar (otherwise the user is
    /// staring at a tmux pane whose cwd no longer exists).
    fn drain_deletes(&mut self) {
        while let Ok(result) = self.delete_rx.try_recv() {
            match result {
                DeleteWorktreeResult::Deleted(id) => {
                    let removed = self.catalog.remove_by_id(&id);
                    if let Some(emb) = self.embedded.as_ref()
                        && emb.session_id == id
                    {
                        self.embedded = None;
                        self.focus = Focus::Sidebar;
                    }
                    match removed {
                        Some(s) => {
                            self.status =
                                Some(format!("deleted worktree at {}", s.project_dir.display()));
                        }
                        None => {
                            // Session was already removed from the
                            // catalog by something else (e.g. a
                            // discovery refresh between modal open
                            // and submit). The git side succeeded,
                            // so report that — losing the row
                            // earlier isn't a failure.
                            self.status = Some("deleted worktree".to_string());
                        }
                    }
                }
                DeleteWorktreeResult::Failed(msg) => {
                    self.status = Some(msg);
                }
            }
        }
    }

    fn drain_updates(&mut self) {
        while let Ok(event) = self.updates.try_recv() {
            match event {
                WatcherEvent::Attention(update) => {
                    // Heuristic-derived attention routes through the
                    // hook-aware catalog method: while a session is
                    // hook-pinned (NeedsInput forced by a Notification
                    // hook event), a heuristic update with an mtime
                    // older than the pin is suppressed so the pinned
                    // state survives until the transcript actually
                    // advances. When mtime advances past the pin, the
                    // pin clears and the heuristic is applied
                    // normally.
                    let prev = self.catalog.apply_heuristic_attention(
                        &update.id,
                        update.attention,
                        update.mtime,
                    );
                    if let Some(mtime) = update.mtime {
                        // Keeps the sidebar's "last activity" cell live
                        // across an active conversation; without this it
                        // would freeze at the discovery-time mtime.
                        self.catalog.touch_activity(&update.id, mtime);
                    }
                    if let Some(prev) = prev {
                        self.fire_attention_notification(&update.id, prev, update.attention);
                    }
                }
                WatcherEvent::Hook { id, received_at } => {
                    // A Claude Code Notification hook fired for `id`.
                    // Force NeedsInput regardless of what the
                    // heuristic last derived (the typical case is a
                    // permission prompt during a tool_use that the
                    // heuristic was reporting as Working). Pinning is
                    // handled inside apply_hook_event.
                    let prev = self.catalog.apply_hook_event(&id, received_at);
                    if let Some(prev) = prev {
                        self.fire_attention_notification(
                            &id,
                            prev,
                            agent_mux::session::Attention::NeedsInput,
                        );
                    }
                }
                WatcherEvent::NewTranscript { host, path, mtime } => {
                    self.handle_new_transcript(&host, &path, mtime);
                }
                WatcherEvent::LivePanes {
                    host,
                    cwds,
                    session_names,
                } => {
                    let cwd_set: HashSet<PathBuf> = cwds.into_iter().collect();
                    // Map the tmux-naming convention `agent-mux-<id>`
                    // to opaque `SessionId`s here so the catalog never
                    // deals in tmux strings (per the "tmux specifics
                    // live behind the Attachment Driver" discipline in
                    // ARCHITECTURE.md). The convention itself is owned
                    // by `tmux_resume_argv` in `attachment.rs`; this
                    // call site recognises the inverse mapping.
                    let live_session_ids: HashSet<SessionId> = session_names
                        .into_iter()
                        .filter_map(|name| {
                            name.strip_prefix("agent-mux-")
                                .map(|id| SessionId(id.into()))
                        })
                        .collect();
                    self.catalog
                        .apply_live_panes(&host, &cwd_set, &live_session_ids);
                }
            }
        }
    }

    /// Route an attention transition to the notifier with the session's
    /// display labels resolved. Looked up here rather than in the
    /// notifier so the notifier stays decoupled from the catalog —
    /// it gets exactly the few `&str`/`&Path` it renders into the payload.
    fn fire_attention_notification(&mut self, id: &SessionId, prev: Attention, new: Attention) {
        let Some(session) = self.catalog.sessions().iter().find(|s| s.id == *id) else {
            return;
        };
        let title = session.title.as_deref().unwrap_or_else(|| {
            // Fallback when the session has no resolved title: use the
            // tail of the session id so multiple title-less sessions
            // remain distinguishable in the notification stream.
            let id_str = session.id.0.as_str();
            id_str
                .get(id_str.len().saturating_sub(6)..)
                .unwrap_or(id_str)
        });
        let host = session.host.clone();
        let project = session.project_dir.clone();
        let actively_viewed = compute_actively_viewed(
            self.focus,
            self.terminal_focused,
            self.embedded.as_ref().map(|e| &e.session_id),
            id,
        );
        self.notifier.on_attention_update(
            &Transition {
                id,
                prev,
                new,
                title,
                host: &host,
                project: &project,
                actively_viewed,
            },
            SystemTime::now(),
        );
    }

    /// React to a watcher-emitted "previously-unknown transcript appeared"
    /// event. The file may be only partially written on the first event,
    /// in which case `build_session` returns `Ok(None)` (no usable cwd
    /// yet) and we silently drop it — the next event re-fires
    /// `NewTranscript` and we retry until the file has enough content to
    /// build a session from.
    fn handle_new_transcript(&mut self, host_id: &HostId, path: &Path, mtime: SystemTime) {
        let Some(host) = self.hosts.get(host_id).cloned() else {
            return;
        };
        let Ok(Some(session)) = build_session(host.as_ref(), path, mtime) else {
            return;
        };
        let id = session.id.clone();
        let transcript_path = session.transcript_path.clone();
        if !self.catalog.add(session) {
            return;
        }
        if self.list_state.selected().is_none() {
            let rows = self.current_rows();
            self.list_state.select(first_session_index(&rows));
        }
        if let Err(e) = self
            .watcher
            .track_new_transcript(host_id, id, transcript_path)
        {
            self.status = Some(format!("watch new transcript: {e}"));
        }
    }

    fn attach_selected(&mut self) -> Option<SuspendCommand> {
        let (result, session_id) = {
            let session = self.selected_session()?;
            let host = self.hosts.get(&session.host)?.clone();
            let id = session.id.clone();
            (self.driver.attach(session, host.as_ref()), id)
        };
        match result {
            Ok(AttachOutcome::Done) => {
                self.status = None;
                None
            }
            Ok(AttachOutcome::SuspendAndRun(cmd)) => {
                self.status = None;
                Some(cmd)
            }
            Ok(AttachOutcome::EmbedPty(spec)) => {
                self.install_embedded(&spec, session_id);
                None
            }
            Err(e) => {
                self.status = Some(format!("attach: {e}"));
                None
            }
        }
    }

    /// Spawn (or refocus) the embedded PTY for the given session.
    /// Common gesture: prefix-escape, navigate, press Enter on the
    /// same session — we shouldn't kill and respawn the PTY just to
    /// return focus, because that loses the child's state. The
    /// `session_id` comparison gates that fast path.
    ///
    /// On a different session, the existing PTY is dropped first (its
    /// `Drop` closes the pty → child gets SIGHUP → reader thread reaps
    /// it), then a fresh PTY is spawned. Spawn failures surface as a
    /// dashboard status; the focus stays on the sidebar so the user
    /// isn't stranded in a terminal that doesn't exist.
    fn install_embedded(&mut self, spec: &EmbedSpec, session_id: SessionId) {
        if let Some(existing) = &self.embedded
            && existing.session_id == session_id
        {
            self.focus = Focus::Terminal {
                leader_armed: false,
            };
            self.status = None;
            return;
        }
        // Drop the previous PTY before spawning a new one. Done in two
        // steps so the old `EmbeddedPty`'s `Drop` runs *before* we
        // allocate a fresh pty (avoids briefly holding two ptys for
        // overlapping sessions).
        self.embedded = None;
        match EmbeddedPty::spawn(
            &spec.argv,
            spec.cwd.as_deref(),
            DEFAULT_PTY_ROWS,
            DEFAULT_PTY_COLS,
        ) {
            Ok(pty) => {
                self.embedded = Some(Embedded {
                    pty,
                    session_id,
                    last_size: (DEFAULT_PTY_ROWS, DEFAULT_PTY_COLS),
                    last_inner: None,
                    tool_tmux_session: spec.tmux_session.clone(),
                });
                self.focus = Focus::Terminal {
                    leader_armed: false,
                };
                self.status = None;
            }
            Err(e) => {
                self.status = Some(format!("embedded attach failed: {e}"));
            }
        }
    }

    /// Route a key event while [`Focus::Terminal`] holds the keyboard.
    /// See the free-standing [`leader_chord_transition`] for the
    /// state machine; this function only carries out the chosen
    /// transition's side effect (focus mutation and/or PTY write).
    fn handle_terminal_key(&mut self, key: &KeyEvent) {
        match leader_chord_transition(self.focus, key) {
            LeaderChordTransition::EscapeToSidebar => {
                self.focus = Focus::Sidebar;
            }
            LeaderChordTransition::ForwardBothToPty => {
                let leader_event = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
                let mut bytes = encode_key_for_pty(&leader_event);
                bytes.extend_from_slice(&encode_key_for_pty(key));
                self.write_to_embedded(&bytes);
                self.focus = Focus::Terminal {
                    leader_armed: false,
                };
            }
            LeaderChordTransition::ArmLeader => {
                self.focus = Focus::Terminal { leader_armed: true };
            }
            LeaderChordTransition::EncodeAndForward => {
                let bytes = encode_key_for_pty(key);
                if !bytes.is_empty() {
                    self.write_to_embedded(&bytes);
                }
            }
        }
    }

    /// Best-effort write to the active embedded PTY. A write failure
    /// surfaces in the dashboard status but does not transition focus —
    /// the PTY may still be readable, and an over-eager teardown would
    /// strand the user with a stuck terminal.
    fn write_to_embedded(&mut self, bytes: &[u8]) {
        let Some(embedded) = self.embedded.as_mut() else {
            return;
        };
        if let Err(e) = embedded.pty.write_input(bytes) {
            self.status = Some(format!("pty write: {e}"));
        }
    }

    /// Translate a terminal-absolute mouse event into PTY-relative
    /// coordinates (1-based) and forward it as an SGR mouse report.
    /// Drops the event when:
    /// - Shift is held. Per the iTerm2 / kitty / wezterm / Alacritty
    ///   convention, Shift+mouse bypasses the application's mouse
    ///   capture and lets the host terminal do native selection.
    ///   Most of those emulators handle the bypass at the OS level
    ///   (we never see the event), but a few forward it anyway; this
    ///   guard makes sure agent-mux never swallows a Shift+drag that
    ///   was meant for native selection. Filed in TODO 2026-05-20.
    /// - The embedded PTY isn't focused (mouse outside the terminal
    ///   pane is meaningless for the running child).
    /// - The click landed outside the PTY's content area (sidebar,
    ///   borders, header, footer).
    /// - The event kind isn't supported by `encode_mouse_event` (e.g.
    ///   horizontal scroll).
    fn handle_terminal_mouse(&mut self, ev: crossterm::event::MouseEvent) {
        if !should_capture_mouse(ev) {
            return;
        }
        if !matches!(self.focus, Focus::Terminal { .. }) {
            return;
        }
        let Some(embedded) = self.embedded.as_ref() else {
            return;
        };
        let Some(inner) = embedded.last_inner else {
            return;
        };
        // crossterm reports 0-based terminal coordinates; SGR mouse
        // expects 1-based PTY-relative. Bounds check first so a click
        // in the sidebar doesn't get encoded as a negative coord.
        let col = ev.column;
        let row = ev.row;
        if col < inner.x
            || row < inner.y
            || col >= inner.x + inner.width
            || row >= inner.y + inner.height
        {
            return;
        }
        let pty_col = col - inner.x + 1;
        let pty_row = row - inner.y + 1;
        if let Some(bytes) = encode_mouse_event(&ev, pty_col, pty_row) {
            self.write_to_embedded(&bytes);
        }
    }

    /// Forward a bracketed-paste payload to the embedded PTY, wrapped
    /// in the `\e[200~ … \e[201~` markers the child opted into. No-op
    /// in Sidebar focus — pasting into the dashboard list is
    /// undefined and silently ignored is better than corrupting the
    /// search query.
    fn handle_terminal_paste(&mut self, text: &str) {
        if !matches!(self.focus, Focus::Terminal { .. }) {
            return;
        }
        let bytes = encode_paste(text);
        self.write_to_embedded(&bytes);
    }

    /// Drain pending events from the embedded PTY's reader thread.
    /// `Output` events are pure redraw hints (the parser is already
    /// updated); `Exited` means the child terminated and the PTY
    /// should be dropped, returning focus to the sidebar.
    ///
    /// We deliberately do not surface the exit status — the common
    /// case is "user detached from tmux," which is a normal end of
    /// session and shouldn't read as an error in the dashboard.
    fn drain_pty_events(&mut self) {
        let Some(embedded) = self.embedded.as_ref() else {
            return;
        };
        let mut exited = false;
        while let Some(ev) = embedded.pty.poll_event() {
            if matches!(ev, PtyEvent::Exited(_)) {
                exited = true;
                break;
            }
        }
        if exited {
            // Capture the tool's tmux session name before dropping
            // the Embedded — pruning the registry afterwards is what
            // makes the sidebar row vanish.
            let tool_session = embedded.tool_tmux_session.clone();
            self.embedded = None;
            self.focus = Focus::Sidebar;
            if let Some(tmux_session) = tool_session {
                self.forget_tool_launch(&tmux_session);
            }
        }
    }

    fn spawn_terminal_selected(&mut self) -> Option<SuspendCommand> {
        let result = {
            let session = self.selected_session()?;
            let host = self.hosts.get(&session.host)?.clone();
            let cwd = session.project_dir.clone();
            let id = session.id.clone();
            (self.driver.spawn_terminal(session, host.as_ref()), cwd, id)
        };
        match result {
            (Ok(AttachOutcome::Done), cwd, _) => {
                self.status = Some(format!("opened terminal in {}", cwd.display()));
                None
            }
            (Ok(AttachOutcome::SuspendAndRun(cmd)), _, _) => {
                self.status = None;
                Some(cmd)
            }
            (Ok(AttachOutcome::EmbedPty(spec)), _, id) => {
                // Embed the shell in the dashboard pane. Synthetic
                // SessionId prefixed `agent-mux-terminal:` so re-press
                // refocuses (same id), but Enter on the source session
                // row swaps cleanly back to the claude attach (different
                // id → install_embedded drops + respawns).
                let synthetic_id = SessionId(format!("agent-mux-terminal:{}", id.0));
                self.install_embedded(&spec, synthetic_id);
                None
            }
            (Err(e), _, _) => {
                self.status = Some(format!("terminal: {e}"));
                None
            }
        }
    }

    /// Fire a user-configured `[[tools]]` keybind against the selected
    /// session. Resolves `{cwd}` / `{host}` placeholders against the
    /// session, then routes through the attachment driver's
    /// `spawn_tool` (same dispatch family as `t: terminal`).
    /// Returns a `SuspendCommand` for the outside-tmux path; `None`
    /// when nothing to suspend (`Done`, `EmbedPty`, errors).
    fn launch_tool(&mut self, idx: usize) -> Option<SuspendCommand> {
        let tool = self.config.tools.get(idx)?.clone();
        let (outcome, label, cwd, session_id, host_id) = {
            let session = self.selected_session()?;
            let host = self.hosts.get(&session.host)?.clone();
            let cwd = session.project_dir.clone();
            let id = session.id.clone();
            let host_id = session.host.clone();
            let host_str = session.host.as_str().to_string();
            let cmd = tool.substitute(&cwd, &host_str);
            (
                self.driver.spawn_tool(session, host.as_ref(), &cmd),
                tool.name.clone().unwrap_or_else(|| {
                    // Fall back to the program name (first token) when
                    // the user didn't set a `name`. Distinguishes
                    // launches in the status line without forcing
                    // every entry to carry an explicit label.
                    tool.command
                        .first()
                        .cloned()
                        .unwrap_or_else(|| format!("tool {idx}"))
                }),
                cwd,
                id,
                host_id,
            )
        };
        match outcome {
            Ok(AttachOutcome::Done) => {
                self.status = Some(format!("launched {label} in {}", cwd.display()));
                None
            }
            Ok(AttachOutcome::SuspendAndRun(cmd)) => {
                self.status = None;
                Some(cmd)
            }
            Ok(AttachOutcome::EmbedPty(spec)) => {
                // Register the launch in the tools registry so the
                // dashboard's "Tools" sidebar group can re-attach
                // after the user swaps focus away. `tmux_session` is
                // populated by `PtyDriver::spawn_tool_*_embed` (the
                // tool runs in a detached tmux session so its state
                // survives a PTY swap).
                if let Some(tmux_session) = spec.tmux_session.clone() {
                    self.tool_launches.push(ToolLaunch {
                        name: label.clone(),
                        host: host_id,
                        tmux_session,
                        project_dir: cwd.clone(),
                        launched_at: SystemTime::now(),
                    });
                }
                // Embed the tool in the dashboard pane. Synthetic
                // SessionId includes the tool index so the same
                // session can host distinct tools without collision —
                // pressing `g` then `v` swaps between lazygit and
                // nvim cleanly, while pressing `g` twice refocuses
                // the existing lazygit.
                let synthetic_id = SessionId(format!("agent-mux-tool:{idx}:{}", session_id.0));
                self.install_embedded(&spec, synthetic_id);
                None
            }
            Err(e) => {
                self.status = Some(format!("tool {label}: {e}"));
                None
            }
        }
    }

    /// Re-attach the embedded pane to the tool launch at
    /// `tool_index`. Builds the `EmbedSpec` from the registry entry
    /// directly — the same shape `spawn_tool_*_embed` would have
    /// produced at original launch time — and feeds it to
    /// `install_embedded`. On spawn failure the launch is pruned from
    /// the registry (its tmux session has been killed; the row should
    /// vanish from the sidebar).
    fn attach_tool(&mut self, tool_index: usize) -> Option<SuspendCommand> {
        let launch = self.tool_launches.launches().get(tool_index)?.clone();
        let host = self.hosts.get(&launch.host)?.clone();
        let attach: Vec<String> = vec![
            "tmux".into(),
            "attach".into(),
            "-t".into(),
            launch.tmux_session.clone(),
        ];
        let argv = if launch.host.is_local() {
            // Local: argv runs directly via portable-pty.
            attach
        } else {
            // Remote: wrap in ssh -tt; same shape spawn_tool_remote
            // would have produced.
            let refs: Vec<&str> = attach.iter().map(String::as_str).collect();
            host.ssh_argv(true, &refs)?
        };
        let project = launch
            .project_dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?");
        let spec = EmbedSpec {
            argv,
            cwd: None,
            label: format!("⚒ {} · {} · [{}]", launch.name, project, launch.host),
            tmux_session: Some(launch.tmux_session.clone()),
        };
        let synthetic_id = SessionId(format!("agent-mux-tool-attach:{}", launch.tmux_session));
        self.install_embedded(&spec, synthetic_id);
        None
    }

    /// Drop a tool launch by its tmux session name. Called from the
    /// embedded PTY's `Exited` handler when the tool process dies on
    /// its own (user typed `exit` in lazygit, ran the command to
    /// completion, etc.).
    fn forget_tool_launch(&mut self, tmux_session: &str) {
        if let Some(idx) = self.tool_launches.position_by_tmux_session(tmux_session) {
            self.tool_launches.remove(idx);
        }
    }

    fn next(&mut self) {
        let rows = self.current_rows();
        if let Some(i) = next_session_index(self.list_state.selected(), &rows) {
            self.list_state.select(Some(i));
        }
    }

    fn prev(&mut self) {
        let rows = self.current_rows();
        if let Some(i) = prev_session_index(self.list_state.selected(), &rows) {
            self.list_state.select(Some(i));
        }
    }

    fn next_project(&mut self) {
        let rows = self.current_rows();
        if let Some(i) = next_project_index(self.list_state.selected(), &rows) {
            self.list_state.select(Some(i));
        }
    }

    fn prev_project(&mut self) {
        let rows = self.current_rows();
        if let Some(i) = prev_project_index(self.list_state.selected(), &rows) {
            self.list_state.select(Some(i));
        }
    }

    fn next_host(&mut self) {
        let rows = self.current_rows();
        if let Some(i) = next_host_index(self.list_state.selected(), &rows) {
            self.list_state.select(Some(i));
        }
    }

    fn prev_host(&mut self) {
        let rows = self.current_rows();
        if let Some(i) = prev_host_index(self.list_state.selected(), &rows) {
            self.list_state.select(Some(i));
        }
    }

    /// Resolve the currently-selected list row to a session. Returns
    /// `None` if no row is selected, the selected row is a header (the
    /// navigation helpers shouldn't allow this but the lookup stays
    /// defensive), or the underlying session index has gone out of
    /// range (defensive against a catalog mutation racing a keypress).
    fn selected_session(&self) -> Option<&Session> {
        let idx = self.list_state.selected()?;
        let rows = self.current_rows();
        let DisplayRow::SessionRow(session_idx) = rows.get(idx)? else {
            return None;
        };
        self.catalog.sessions().get(*session_idx)
    }

    /// Returns the tool-launch index when the selected row is a
    /// `DisplayRow::ToolRow`. Used by the Enter dispatcher to route
    /// to `attach_tool` instead of `attach_selected`.
    fn selected_tool_index(&self) -> Option<usize> {
        let idx = self.list_state.selected()?;
        let rows = self.current_rows();
        if let Some(DisplayRow::ToolRow(i)) = rows.get(idx) {
            Some(*i)
        } else {
            None
        }
    }
}

fn run(terminal: &mut Tui, app: &mut App) -> io::Result<()> {
    loop {
        terminal.draw(|frame| draw(frame, app))?;
        // Tick faster while the embedded terminal owns the keyboard —
        // see `TICK_TERMINAL` for the latency reasoning.
        let tick = if matches!(app.focus, Focus::Terminal { .. }) {
            TICK_TERMINAL
        } else {
            TICK
        };
        let has_event = event::poll(tick)?;
        app.drain_updates();
        app.drain_remote_discoveries();
        app.drain_repo_scans();
        app.drain_pty_events();
        if let Some(cmd) = app.drain_creates()
            && let Some(err) = suspend_and_run(terminal, &cmd, app.embedded_mode)?
        {
            app.status = Some(err);
        }
        app.drain_deletes();
        if has_event {
            let raw = event::read()?;
            match raw {
                Event::Mouse(mev) => {
                    app.handle_terminal_mouse(mev);
                    continue;
                }
                Event::Paste(text) => {
                    app.handle_terminal_paste(&text);
                    continue;
                }
                // Resize falls through — the next draw's resize cascade
                // picks up the new terminal size automatically.
                Event::Resize(_, _) => continue,
                // DEC 1004 focus events drive the notifier's
                // `actively_viewed` predicate so an alt-tab away from
                // the terminal lifts the suppression even when the
                // in-app `Focus::Terminal` state is still set.
                Event::FocusGained => {
                    app.terminal_focused = true;
                    continue;
                }
                Event::FocusLost => {
                    app.terminal_focused = false;
                    // Re-evaluate sessions whose `NeedsInput` entry was
                    // suppressed because they were actively viewed at
                    // transition time. The user has just alt-tabbed
                    // away from the terminal, so any belated toast we
                    // stashed should fire now.
                    app.notifier.on_terminal_focus_lost(SystemTime::now());
                    continue;
                }
                Event::Key(_) => {}
            }
            let Event::Key(key) = raw else { continue };
            // Embedded-terminal focus owns the keyboard. Ctrl-C goes
            // to the running child (the standard "interrupt" gesture);
            // the only way out is the leader chord (`Ctrl-a Esc`).
            if matches!(app.focus, Focus::Terminal { .. }) {
                app.handle_terminal_key(&key);
                continue;
            }
            // Ctrl-C quits unconditionally in sidebar focus, even when
            // the modal is open or the search bar has focus. Everything
            // else routes to those if they're up.
            let ctrl_c =
                key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
            if ctrl_c {
                return Ok(());
            }
            if app.modal.is_some() {
                app.handle_modal_key(key);
                continue;
            }
            if app.delete_modal.is_some() {
                app.handle_delete_modal_key(key);
                continue;
            }
            // Rename overlay owns the keyboard while open — characters
            // append to the buffer, Backspace pops, Enter commits, Esc
            // cancels. Comes before the search routes for the same
            // reason: a buffer that contains `q` should not fire Quit.
            if app.route_rename_key(key) {
                continue;
            }
            // Editing-mode search owns the keyboard — composing the
            // filter must not also fire Action::Quit on `q`.
            if app.route_search_editing_key(key) {
                continue;
            }
            // Active-mode search only intercepts Esc (clear) and `/`
            // (re-edit); everything else falls through to action dispatch
            // so j/k/Enter/n/t continue to work against the filtered list.
            if app.route_search_active_key(key) {
                continue;
            }
            match dispatch_action(app, action_for(key, &app.config.tools)) {
                ActionOutcome::Quit => return Ok(()),
                ActionOutcome::Continue => {}
                ActionOutcome::Suspend(cmd) => {
                    if let Some(err) = suspend_and_run(terminal, &cmd, app.embedded_mode)? {
                        app.status = Some(err);
                    }
                }
            }
        }
    }
}

/// Off-thread: open a `ControlMaster` to `ssh_target`, run discovery
/// against `transcript_root`, and pre-compute initial attention per
/// session against the same connection. Returns a payload the main
/// thread can apply directly to the catalog. Done off the UI thread so
/// the dashboard appears immediately on local sessions while remote
/// hosts (which can take seconds to handshake) stream in as they're
/// ready — the "session switching never blocks on I/O" discipline
/// applies to startup latency too.
fn connect_and_discover(
    host_id: HostId,
    ssh_target: String,
    transcript_root: &Path,
    cache_dir: Option<PathBuf>,
    tx: &Sender<RemoteDiscoveryResult>,
) {
    let ssh = match SshHost::connect(host_id.clone(), ssh_target) {
        Ok(h) => h,
        Err(e) => {
            let _ = tx.send(RemoteDiscoveryResult::Failed {
                host_id,
                error: e.to_string(),
            });
            return;
        }
    };
    let host: Arc<dyn Host> = Arc::new(ssh);
    // Emit Connected before running discovery so the main thread can
    // register the host (cached remote sessions become attachable) and
    // start the polling threads — both gates that previously had to
    // wait for the full discovery pass. If the receiver dropped (the
    // dashboard exited) we bail; otherwise proceed with discovery.
    if tx
        .send(RemoteDiscoveryResult::Connected {
            host_id: host_id.clone(),
            host: Arc::clone(&host),
            transcript_root: transcript_root.to_path_buf(),
        })
        .is_err()
    {
        return;
    }
    let sessions = match discover(host.as_ref(), transcript_root) {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(RemoteDiscoveryResult::Failed {
                host_id,
                error: format!("discovery: {e}"),
            });
            return;
        }
    };
    // No second pass for attention: `discover` now derives initial
    // attention from the same bulk-fetched transcript content it used
    // for cwd/title extraction, so the row paints with a real state
    // (not Unknown) without a per-session `tail -c` round-trip.
    // Snapshot to disk so the next startup paints this host's
    // sessions before its `ControlMaster` handshake completes.
    // Best-effort: an unwritable cache directory must not fail the
    // discovery, since the cache is strictly an optimisation.
    if let Some(dir) = cache_dir {
        let _ = cache::write_for_host(&dir, &host_id, &sessions);
    }
    let _ = tx.send(RemoteDiscoveryResult::Ready { host_id, sessions });
}

/// Outcome of a sidebar-focus key dispatch. Lets `run` keep its event
/// loop linear instead of nesting `Option<SuspendCommand>` and
/// early-return inside a `match` arm.
enum ActionOutcome {
    Quit,
    Continue,
    Suspend(SuspendCommand),
}

/// Apply a sidebar `Action` to the app and report back what the main
/// loop should do. Pulled out of `run` to keep that function under the
/// line cap; pure dispatch with no terminal handoff (the caller
/// invokes `suspend_and_run` for the `Suspend` variant).
fn dispatch_action(app: &mut App, action: Option<Action>) -> ActionOutcome {
    let Some(action) = action else {
        return ActionOutcome::Continue;
    };
    match action {
        Action::Quit => ActionOutcome::Quit,
        Action::Next => {
            app.next();
            ActionOutcome::Continue
        }
        Action::Prev => {
            app.prev();
            ActionOutcome::Continue
        }
        Action::NextProject => {
            app.next_project();
            ActionOutcome::Continue
        }
        Action::PrevProject => {
            app.prev_project();
            ActionOutcome::Continue
        }
        Action::NextHost => {
            app.next_host();
            ActionOutcome::Continue
        }
        Action::PrevHost => {
            app.prev_host();
            ActionOutcome::Continue
        }
        Action::Attach => {
            // Enter on a tool row re-attaches the embedded pane to
            // that tool's tmux session instead of routing through the
            // session-attach path (the catalog has no Session for a
            // tool launch).
            if let Some(tool_idx) = app.selected_tool_index() {
                match app.attach_tool(tool_idx) {
                    Some(cmd) => ActionOutcome::Suspend(cmd),
                    None => ActionOutcome::Continue,
                }
            } else {
                match app.attach_selected() {
                    Some(cmd) => ActionOutcome::Suspend(cmd),
                    None => ActionOutcome::Continue,
                }
            }
        }
        Action::SpawnTerminal => match app.spawn_terminal_selected() {
            Some(cmd) => ActionOutcome::Suspend(cmd),
            None => ActionOutcome::Continue,
        },
        Action::NewSession => {
            app.open_new_session();
            ActionOutcome::Continue
        }
        Action::NewSessionNoWorktree => {
            app.open_new_session_no_worktree();
            ActionOutcome::Continue
        }
        Action::OpenSearch => {
            app.open_search();
            ActionOutcome::Continue
        }
        Action::DeleteWorktree => {
            app.open_delete_worktree();
            ActionOutcome::Continue
        }
        Action::RenameSession => {
            app.open_rename();
            ActionOutcome::Continue
        }
        Action::LaunchTool(idx) => match app.launch_tool(idx) {
            Some(cmd) => ActionOutcome::Suspend(cmd),
            None => ActionOutcome::Continue,
        },
    }
}

/// Which constructor `open_picker` hands to the modal. `Worktree` runs
/// the standard pick → fill task/branch → `git worktree add` → spawn
/// flow; `NoWorktree` skips the fill stage and spawns claude in the
/// repo root.
#[derive(Debug, Clone, Copy)]
enum ModalOpenMode {
    Worktree,
    NoWorktree,
}

enum Action {
    Quit,
    Next,
    Prev,
    NextProject,
    PrevProject,
    NextHost,
    PrevHost,
    Attach,
    SpawnTerminal,
    NewSession,
    /// `N` — pick a repo, spawn claude in its root, no worktree.
    NewSessionNoWorktree,
    OpenSearch,
    DeleteWorktree,
    /// `r` — open the inline rename overlay for the selected session.
    RenameSession,
    /// User-configured `[[tools]]` keybind. The index is into
    /// `App.config.tools` — `dispatch_action` reads the binding back
    /// out of the same vec at fire time so a stale index can't
    /// outlive a config reload (when reload-on-edit ships).
    LaunchTool(usize),
}

fn action_for(key: KeyEvent, tools: &[ToolBinding]) -> Option<Action> {
    // Ctrl-j / Ctrl-k jump host. Handled first so they don't fall through
    // to the lowercase j/k single-session navigation below. Ctrl-C is
    // already intercepted upstream so it never reaches this function.
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('j') => Some(Action::NextHost),
            KeyCode::Char('k') => Some(Action::PrevHost),
            _ => None,
        };
    }
    match key.code {
        KeyCode::Char('q') => Some(Action::Quit),
        KeyCode::Down | KeyCode::Char('j') => Some(Action::Next),
        KeyCode::Up | KeyCode::Char('k') => Some(Action::Prev),
        // Shift-j / Shift-k jump project. Crossterm reports these as the
        // uppercase glyph rather than 'j' + SHIFT, hence the literal 'J' / 'K'.
        KeyCode::Char('J') => Some(Action::NextProject),
        KeyCode::Char('K') => Some(Action::PrevProject),
        KeyCode::Enter => Some(Action::Attach),
        KeyCode::Char('t') => Some(Action::SpawnTerminal),
        KeyCode::Char('n') => Some(Action::NewSession),
        KeyCode::Char('N') => Some(Action::NewSessionNoWorktree),
        KeyCode::Char('/') => Some(Action::OpenSearch),
        KeyCode::Char('d') => Some(Action::DeleteWorktree),
        KeyCode::Char('r') => Some(Action::RenameSession),
        // Tool keybinds dispatch after built-ins. Config-load
        // validation rejected any tool whose `key` shadows a built-in
        // (RESERVED_KEY_CHARS in config.rs), so the order here is
        // belt-and-braces: even a buggy validator can't make a tool
        // override `q` or `j`.
        KeyCode::Char(c) => tools
            .iter()
            .position(|t| t.key == c)
            .map(Action::LaunchTool),
        _ => None,
    }
}

/// Border style for a pane based on whether it currently owns the
/// keyboard. Focused → cyan + BOLD so the cue is visible at a glance
/// and also survives terminals with poor colour discrimination via the
/// weight fallback. Unfocused → DIM so the rival pane recedes without
/// disappearing.
///
/// Colour is hardcoded rather than themed for now; a `[theme.focus_border]`
/// extension is filed in TODO under "extend [theme] schema beyond
/// foreground colours".
fn focus_border_style(focused: bool) -> Style {
    if focused {
        Style::new()
            .fg(ratatui::style::Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().add_modifier(Modifier::DIM)
    }
}

/// Pure decision: should this mouse event be captured and forwarded
/// into the embedded PTY at all? Returns `false` when `Shift` is held,
/// matching the iTerm2 / kitty / wezterm / Alacritty convention that
/// Shift+mouse bypasses application capture so the host terminal can
/// do native selection. Most of those emulators already implement the
/// bypass at the OS level (we never see the event), but a few forward
/// it anyway — this guarantees we never swallow a Shift+drag that was
/// meant for native selection.
#[must_use]
fn should_capture_mouse(ev: crossterm::event::MouseEvent) -> bool {
    !ev.modifiers.contains(KeyModifiers::SHIFT)
}

/// Pure decision: what `App::handle_terminal_key` should do for `key`
/// given `focus`. Extracted so the leader-chord state machine is
/// unit-testable without standing up a full `App` (which owns a real
/// PTY).
///
/// State machine:
/// - Armed (`Focus::Terminal { leader_armed: true }`) + Esc → leave
///   the embedded pane, return to sidebar. Modifier bits on Esc are
///   *ignored* — under Kitty Keyboard Protocol or extended event
///   modes Esc can arrive with stray modifier bits, and a strict
///   `is_empty()` check there used to silently leak subsequent keys
///   (j/k/Enter) into the inner PTY when the user thought they'd
///   escaped (the 2026-05-20 "stray newline on session swap" bug).
/// - Armed + anything else → forward `Ctrl-a` + key to the PTY (tmux
///   passthrough), disarm.
/// - Unarmed + leader chord (`Ctrl-a`) → arm.
/// - Unarmed + anything else → encode and forward.
#[must_use]
fn leader_chord_transition(focus: Focus, key: &KeyEvent) -> LeaderChordTransition {
    if matches!(focus, Focus::Terminal { leader_armed: true }) {
        if key.code == KeyCode::Esc {
            return LeaderChordTransition::EscapeToSidebar;
        }
        return LeaderChordTransition::ForwardBothToPty;
    }
    if is_pty_leader(key) {
        return LeaderChordTransition::ArmLeader;
    }
    LeaderChordTransition::EncodeAndForward
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeaderChordTransition {
    EscapeToSidebar,
    ForwardBothToPty,
    ArmLeader,
    EncodeAndForward,
}

/// Build the sidebar's `ListItem`s from the current row layout. Lifted
/// out of `draw` so the host/project/session/tools match doesn't bloat
/// the top-level draw function past the lint cap. Takes individual
/// fields (rather than `&App`) so the returned items don't tie up a
/// borrow of the whole `App`, which would conflict with the
/// subsequent `&mut app.list_state` render call.
fn build_sidebar_items(
    rows: &[DisplayRow],
    sessions: &[Session],
    home: Option<&Path>,
    theme: &Theme,
    tool_launches: &[ToolLaunch],
    session_names: &SessionNameStore,
) -> Vec<ListItem<'static>> {
    rows.iter()
        .map(|row| match row {
            DisplayRow::HostHeader(host) => ListItem::new(format_host_header(host)),
            DisplayRow::ProjectHeader(path) => ListItem::new(format_project_header(path, home)),
            DisplayRow::SessionRow(i) => {
                let s = &sessions[*i];
                let name_override = session_names.get(&s.host, &s.id);
                ListItem::new(format_session_row(s, theme, name_override))
            }
            DisplayRow::ToolsHeader => ListItem::new(format_tools_header()),
            DisplayRow::ToolRow(i) => ListItem::new(format_tool_row(&tool_launches[*i])),
        })
        .collect()
}

/// Style for the sidebar's outer border. In embed mode the sidebar
/// competes with the terminal pane for keystrokes, so the border picks
/// up a focus cue; outside embed mode there's nothing to disambiguate
/// against and the border stays plain.
fn sidebar_border_style(app: &App) -> Style {
    if app.embedded.is_some() {
        focus_border_style(matches!(app.focus, Focus::Sidebar))
    } else {
        Style::new()
    }
}

/// Spawn the Claude Code hook-marker watcher into the dashboard's
/// event channel. `None` returns mean we fall back to heuristic-only
/// attention with a one-line eprintln explaining why — startup must
/// not fail because the hook ingress couldn't initialise (the
/// dashboard works without it; the hook is a *richer* signal, not
/// the only signal). Lifted out of `App::new` to keep that
/// constructor under the `too_many_lines` cap.
#[must_use]
fn init_hook_watcher(
    event_tx: &std::sync::mpsc::Sender<WatcherEvent>,
) -> Option<notify::RecommendedWatcher> {
    // Watch <local-transcripts-root>/.agent-mux-hooks/. The hook
    // subcommand derives this same path from the payload's
    // `transcript_path` so local and remote producers + consumers
    // share one path convention.
    let Some(root) = claude_projects_dir() else {
        eprintln!(
            "agent-mux: hook watcher disabled (no Claude Code transcripts dir resolved); \
             heuristic-only attention path stays in effect"
        );
        return None;
    };
    let dir = agent_mux::hook_ingest::hook_dir_for_transcripts_root(&root);
    match agent_mux::hook_ingest::spawn_hook_watcher(&dir, event_tx) {
        Ok(w) => Some(w),
        Err(e) => {
            eprintln!(
                "agent-mux: hook watcher disabled ({} on {}); \
                 heuristic-only attention path stays in effect",
                e,
                dir.display()
            );
            None
        }
    }
}

/// Construct the M4 notifier from the resolved `[notifications]`
/// config. Picks a platform-aware dispatcher via
/// [`pick_dispatcher`] and logs the chosen backend label to stderr
/// before ratatui takes over — the 2026-05-20 dogfood signal was
/// "silent failure defeats the entire attention-flap loop", so the
/// pick lands in shell scrollback rather than being invisible.
#[must_use]
fn build_notifier(cfg: &config::NotificationsConfig) -> Notifier {
    let (dispatcher, backend_label) = pick_dispatcher(cfg.backend);
    eprintln!("agent-mux: notifications backend = {backend_label}");
    Notifier::new(dispatcher, cfg.clone())
}

/// Load the persistent session-name override store from the default
/// cache path, degrading silently to an empty store when no cache
/// path resolves. Lifted out of `App::new` so the field initialiser
/// stays a one-liner.
#[must_use]
fn load_session_names_or_empty() -> SessionNameStore {
    default_store_path()
        .map(SessionNameStore::load_or_empty)
        .unwrap_or_default()
}

/// Render the search/rename overlay bar at `layout[2]` when either
/// is active. Returns the index in `layout` where the footer should
/// land (3 when a bar was rendered, 2 otherwise). Lifted out of
/// `draw` so the overlay's mutually-exclusive arms don't bloat the
/// top-level function past the lint cap.
fn render_overlay_bar(
    frame: &mut ratatui::Frame<'_>,
    app: &App,
    layout: &[ratatui::layout::Rect],
    visible_sessions: usize,
) -> usize {
    let bar_text = if let Some(rename) = app.rename.as_ref() {
        Some(format!(
            "rename: {}_  ⏎ save · esc cancel  · empty + ⏎ clears override",
            rename.buffer,
        ))
    } else {
        app.search
            .as_ref()
            .map(|s| compose_search_bar(s, visible_sessions))
    };
    if let Some(text) = bar_text {
        let bar = Paragraph::new(Line::from(Span::styled(
            text,
            Style::new().add_modifier(Modifier::BOLD),
        )));
        frame.render_widget(bar, layout[2]);
        3
    } else {
        2
    }
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &mut App) {
    // When search or rename is active we add a dedicated 1-line bar
    // between the list and the footer. Keeping the footer separately
    // means the regular keybind line stays visible — search and
    // rename add context, they don't blot out the navigation hints.
    let needs_overlay_bar = app.search.is_some() || app.rename.is_some();
    let constraints: Vec<Constraint> = if needs_overlay_bar {
        vec![
            Constraint::Length(1), // header
            Constraint::Min(0),    // list
            Constraint::Length(1), // overlay bar (search or rename)
            Constraint::Length(1), // footer
        ]
    } else {
        vec![
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ]
    };
    let layout = Layout::vertical(constraints).split(frame.area());

    let header = Paragraph::new(Line::from(Span::styled(
        " agent-mux ",
        Style::new().add_modifier(Modifier::BOLD),
    )));
    frame.render_widget(header, layout[0]);

    let rows = app.current_rows();
    let visible_sessions = rows
        .iter()
        .filter(|r| matches!(r, DisplayRow::SessionRow(_)))
        .count();
    let title = if app.search.is_some() {
        format!(" sessions ({visible_sessions}/{}) ", app.catalog.len())
    } else {
        format!(" sessions ({}) ", app.catalog.len())
    };
    let items = build_sidebar_items(
        &rows,
        app.catalog.sessions(),
        app.home.as_deref(),
        &app.theme,
        app.tool_launches.launches(),
        &app.session_names,
    );
    let sidebar_block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(sidebar_border_style(app));
    let list = List::new(items)
        .block(sidebar_block)
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▌ ");

    // Two layout modes for the main area:
    // 1. Embedded PTY active → compact sidebar + embedded terminal.
    // 2. Default → list takes the full main area.
    if app.embedded.is_some() {
        draw_embedded(frame, app, list, layout[1]);
    } else {
        frame.render_stateful_widget(list, layout[1], &mut app.list_state);
    }

    let footer_idx = render_overlay_bar(frame, app, &layout, visible_sessions);

    let footer_text = compose_footer(
        app.creating.as_ref(),
        app.status.as_deref(),
        app.catalog.is_empty(),
        app.in_tmux,
        app.pending_hosts,
        &app.connect_errors,
        &app.empty_scans,
        app.focus,
        &app.config.tools,
    );
    let footer = Paragraph::new(Line::from(footer_text));
    frame.render_widget(footer, layout[footer_idx]);

    draw_modal_overlay(frame, app);
}

/// Overlay any open modal on top of the dashboard. At most one modal
/// is up at a time (open-time guards in `open_new_session` /
/// `open_delete_worktree` enforce that), so the order here is purely
/// defensive — render new-session first so a stuck-both state stays
/// visible rather than silently masking one. Extracted to keep `draw`
/// under the 100-line clippy budget.
fn draw_modal_overlay(frame: &mut ratatui::Frame<'_>, app: &mut App) {
    if let Some(modal) = app.modal.as_mut() {
        modal.draw(frame);
    }
    if let Some(modal) = app.delete_modal.as_ref() {
        modal.draw(frame);
    }
}

/// Render the search bar's contents. Mode-aware: Editing mode shows
/// a fake cursor block to signal "I'm taking your keystrokes"; Active
/// mode shows a hint reminding the user how to re-edit or exit. Match
/// count goes in both so the user knows whether their query narrowed
/// the list to zero hits before pressing Enter.
fn compose_search_bar(search: &SearchState, visible: usize) -> String {
    let matches = if visible == 1 {
        "1 match".to_string()
    } else {
        format!("{visible} matches")
    };
    match search.mode {
        SearchMode::Editing => {
            format!(
                " /{}█  ({matches})  ·  ⏎ apply  ·  esc cancel ",
                search.query
            )
        }
        SearchMode::Active => {
            format!(" /{}  ({matches})  ·  / edit  ·  esc clear ", search.query)
        }
    }
}

/// Pure footer composition so the keybind/return hint logic is unit-testable
/// without standing up a ratatui frame. Precedence: in-flight create >
/// transient status > sticky connect-failure line > sticky empty-scan
/// line > empty catalog > keybind line. The keybind line gets a
/// trailing "· connecting to N host(s)…" suffix while remote SSH
/// discovery is still pending. The return hint is mode-aware because
/// attach takes two different code paths (see `App.in_tmux`).
///
/// Connect failures sit *below* transient status so a fresh action's
/// feedback isn't drowned out, but stay visible (until the next
/// transient status, then re-surface) so they're not silently lost
/// the way a single overwriting `status` field would lose them.
/// Empty-scan hints sit one tier below connect failures — a host that
/// connected but found no repos is less severe than one that didn't
/// connect at all, but it's the silent-failure mode dogfooding hit
/// 2026-05-19 (configured paths didn't exist on the remote box) so
/// it earns a sticky line.
// Pure formatting helper: small inputs read straight off the App to
// render one ratatui-paragraph string. Bundling into a struct would
// buy nothing here — every caller would build the same struct inline
// at the same call site — so we silence the lint.
#[allow(clippy::too_many_arguments)]
fn compose_footer(
    creating: Option<&CreatingSession>,
    status: Option<&str>,
    catalog_empty: bool,
    in_tmux: bool,
    pending_hosts: usize,
    connect_errors: &[(HostId, String)],
    empty_scans: &[(HostId, Vec<PathBuf>)],
    focus: Focus,
    tools: &[ToolBinding],
) -> String {
    // Terminal focus narrows the footer to a single hint — the
    // embedded child owns every other key, so listing the sidebar
    // binds would mislead the user about what works right now.
    // Status / creating still win because they're transient signals
    // the user needs even mid-session (e.g. a worktree create finished
    // while the user was inside the terminal).
    if let Some(c) = creating {
        return format!(" creating worktree for {:?} in {}… ", c.task, c.repo_name);
    }
    if let Some(s) = status {
        return format!(" {s} ");
    }
    if matches!(focus, Focus::Terminal { .. }) {
        return " ⌃a esc: back to sidebar ".to_string();
    }
    if !connect_errors.is_empty() {
        let names: Vec<String> = connect_errors.iter().map(|(h, _)| h.to_string()).collect();
        return format!(" connect failed: {} ", names.join(", "));
    }
    if !empty_scans.is_empty() {
        return format_empty_scans_hint(empty_scans);
    }
    if catalog_empty {
        if pending_hosts > 0 {
            return format!(" connecting to {pending_hosts} host(s)… · n/N: new · q: quit ");
        }
        return " no sessions discovered · n/N: new · q: quit ".to_string();
    }
    let return_hint = if in_tmux { "prefix+s" } else { "prefix+d" };
    let suffix = if pending_hosts > 0 {
        format!("  ·  connecting to {pending_hosts} host(s)…")
    } else {
        String::new()
    };
    // Tool hints slot in between the built-ins and `q: quit` so the
    // quit hint stays last — most-frequently-needed-at-the-edge.
    // Empty tools list elides cleanly (no leading separator).
    let tool_hints = if tools.is_empty() {
        String::new()
    } else {
        let entries: Vec<String> = tools
            .iter()
            .map(|t| format!("{}: {}", t.key, footer_tool_label(t)))
            .collect();
        format!(" · {}", entries.join(" · "))
    };
    format!(
        " j/k: move · J/K: project · ⌃j/⌃k: host · ⏎: attach · t: terminal · n: new (N: no worktree) · d: delete{tool_hints} · q: quit  ·  return: {return_hint}{suffix} "
    )
}

/// Resolve a tool's footer label: explicit `name` wins, else the
/// program name (first command token), else a generic `tool` so a
/// future malformed binding doesn't leave the line dangling.
/// Same fallback chain as the launch-status line in `launch_tool`.
/// Format the sticky empty-scan hint. Single-host gets a specific
/// "host: no repos in <folder>" line so the user can see exactly what
/// path was searched and fix the config; multi-host aggregates to
/// "no repos found: host1, host2" because the per-host folders would
/// truncate the line beyond usefulness. First-folder-only for the
/// single-host case keeps the line bounded; the user can `agent-mux
/// config` for the full list if they configured more.
fn format_empty_scans_hint(empty_scans: &[(HostId, Vec<PathBuf>)]) -> String {
    debug_assert!(!empty_scans.is_empty(), "caller guards on non-empty");
    if empty_scans.len() == 1 {
        let (host, folders) = &empty_scans[0];
        let folder = folders.first().map_or_else(
            || "workspace_folders".to_string(),
            |p| p.display().to_string(),
        );
        return format!(" {host}: no repos in {folder} ");
    }
    let names: Vec<String> = empty_scans.iter().map(|(h, _)| h.to_string()).collect();
    format!(" no repos found: {} ", names.join(", "))
}

fn footer_tool_label(t: &ToolBinding) -> String {
    if let Some(n) = t.name.as_deref()
        && !n.is_empty()
    {
        return n.to_string();
    }
    t.command
        .first()
        .cloned()
        .unwrap_or_else(|| "tool".to_string())
}

/// Embedded-PTY layout: compact sidebar on the left, terminal on the
/// right with a focus-aware border. Extracted from `draw` to keep that
/// function under the line cap; takes the prepared sidebar `list`
/// widget and the rect to fill.
fn draw_embedded(
    frame: &mut ratatui::Frame<'_>,
    app: &mut App,
    list: List<'_>,
    area: ratatui::layout::Rect,
) {
    let split =
        Layout::horizontal([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(0)]).split(area);
    frame.render_stateful_widget(list, split[0], &mut app.list_state);

    let term_focus = matches!(app.focus, Focus::Terminal { .. });
    // Resolve the title via the immutable catalog borrow *before*
    // taking the mutable borrow of `embedded` below. Cloning the
    // SessionId here is cheap (a String) and side-steps borrow-checker
    // friction between the catalog lookup and the resize mutation.
    let Some(session_id) = app.embedded.as_ref().map(|e| e.session_id.clone()) else {
        return;
    };
    let label = terminal_block_label(app, &session_id);

    let Some(embedded) = app.embedded.as_mut() else {
        return;
    };
    let term_block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {label} "))
        .border_style(focus_border_style(term_focus));
    let inner = term_block.inner(split[1]);

    // Resize cascade: best-effort on each frame, only firing when the
    // area genuinely changed (see `should_resize_pty` for the
    // predicate). A `master.resize` ioctl failure isn't worth
    // surfacing — the child keeps running at the old size and the
    // next frame will retry.
    let new_size = (inner.height, inner.width);
    if should_resize_pty(embedded.last_size, new_size)
        && embedded.pty.resize(new_size.0, new_size.1).is_ok()
    {
        embedded.last_size = new_size;
    }

    embedded.last_inner = Some(inner);
    frame.render_widget(term_block, split[1]);
    embedded.pty.render(frame, inner);
}

/// Whether the user is actively engaged with the session whose attention
/// just transitioned — three `ANDed` conditions:
///
/// 1. `terminal_focused`: agent-mux's terminal window owns OS-level
///    focus. Without this an alt-tab to a browser would leave the
///    in-app `Focus::Terminal` state intact and the user would miss
///    real attention transitions while the terminal sat unfocused.
/// 2. In-app focus is on the terminal (not the sidebar). Sidebar focus
///    means the user is browsing the dashboard — the row update is
///    visually obvious, but they're not watching the pane content.
///    Kept strict for now per user-stated preference for the safe
///    failure mode (more notifications, not fewer).
/// 3. The embedded PTY hosts the transitioning session. With a
///    *different* session embedded the user can't see the new
///    `NeedsInput`'s content; they need the notification.
///
/// Lifted out of the call site so the regression that produced this
/// function (a first-iteration predicate missing condition 1) has a
/// pure-function unit test instead of relying on live-event-loop
/// dogfooding to catch it.
#[must_use]
fn compute_actively_viewed(
    focus: Focus,
    terminal_focused: bool,
    embedded_session_id: Option<&SessionId>,
    transition_session_id: &SessionId,
) -> bool {
    terminal_focused
        && matches!(focus, Focus::Terminal { .. })
        && embedded_session_id == Some(transition_session_id)
}

/// Whether the embedded PTY should be resized to `proposed`. False for
/// zero-sized areas (vt100 rejects them, and there's nothing meaningful
/// to render anyway) and for no-change ticks (the common case — 60-Hz
/// re-renders without an actual terminal resize must not fire 60
/// SIGWINCHes/sec at the child).
#[must_use]
fn should_resize_pty(current: (u16, u16), proposed: (u16, u16)) -> bool {
    proposed.0 > 0 && proposed.1 > 0 && current != proposed
}

/// Title rendered in the embedded terminal's block. Falls back to the
/// last 6 chars of the session id (with a "…" prefix) for title-less
/// sessions so multi-session worktrees still distinguish at a glance.
fn terminal_block_label(app: &App, session_id: &SessionId) -> String {
    // Look up the session by id rather than relying on selection,
    // because the user may have navigated to a different row while
    // the embedded PTY is still attached to the previously-selected
    // session.
    let session = app.catalog.sessions().iter().find(|s| &s.id == session_id);
    if let Some(title) = session.and_then(|s| s.title.as_deref()) {
        return title.to_string();
    }
    let id = &session_id.0;
    let suffix: String = id.chars().rev().take(6).collect();
    let suffix: String = suffix.chars().rev().collect();
    format!("…{suffix}")
}

/// Header line for the sidebar's "Tools" group. Same bold treatment
/// as host headers — the group is structurally a sibling of "local",
/// "alpenglow", etc.
fn format_tools_header() -> Line<'static> {
    Line::from(Span::styled(
        "── tools ──".to_string(),
        Style::new().add_modifier(Modifier::BOLD),
    ))
}

/// Row for one running tool launch. Indented one level under the
/// Tools header (matching project rows under host headers). Shows
/// tool name + project basename + host label so multiple launches
/// of the same tool against different projects don't look identical
/// (2026-05-21 dogfood: `⚒ lazygit · ⚒ lazygit` with no distinction
/// when both ran in different repos).
fn format_tool_row(launch: &ToolLaunch) -> Line<'static> {
    let age = humanize_elapsed(launch.launched_at);
    let dim = Style::new().add_modifier(Modifier::DIM);
    let project = launch
        .project_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string();
    Line::from(vec![
        Span::raw("  "),
        Span::styled("⚒ ", dim),
        Span::raw(launch.name.clone()),
        Span::raw("  "),
        Span::styled(project, dim),
        Span::raw("  "),
        Span::styled(format!("[{}]", launch.host), dim),
        Span::raw("  "),
        Span::styled(age, dim),
    ])
}

fn format_host_header(host: &HostId) -> Line<'static> {
    // `── label ──` — a strong visual cue without consuming a full
    // horizontal rule's worth of pixels. The List widget gives us the
    // line width but not at format-line construction time; padding to
    // the frame width would require switching to a custom widget. The
    // short header reads cleanly even on narrow terminals.
    Line::from(Span::styled(
        format!("── {host} ──"),
        Style::new().add_modifier(Modifier::BOLD),
    ))
}

fn format_project_header(project: &Path, home: Option<&Path>) -> Line<'static> {
    // Indented one level under the host header; dimmed so it reads as
    // group context rather than a focal row. Home-shortened so the
    // ~ prefix doesn't waste horizontal space.
    Line::from(Span::styled(
        format!("  {}", display_path(project, home)),
        Style::new().add_modifier(Modifier::DIM),
    ))
}

fn format_session_row(
    session: &Session,
    theme: &Theme,
    name_override: Option<&str>,
) -> Line<'static> {
    let attention = effective_attention(session);
    let glyph = attention_glyph(attention);
    let age = humanize_elapsed(session.last_activity);
    let dim = Style::new().add_modifier(Modifier::DIM);
    let glyph_style = apply_fg(Style::new(), attention_color(attention, theme));
    // Dim the entire title when the pane poller has confirmed no
    // live tmux pane matches this session — Enter is going to be a
    // multi-second auto-resume rather than a fast switch, and the
    // dimming pre-mentally-models that cost. `None` (poller hasn't
    // reported for this host yet) renders at normal weight so a
    // remote whose first pane poll is still in flight doesn't flash
    // dim then bright.
    let title_dim = matches!(session.has_live_pane, Some(false));
    let title_style = if title_dim { dim } else { Style::new() };

    // Indented two levels under the project header. Project text is
    // no longer per-row (the header carries it); for title-less
    // sessions, fall back to a short session-id suffix so two
    // unnamed sessions in the same project remain distinguishable.
    let mut spans = vec![
        Span::raw("    "),
        Span::styled(glyph, glyph_style),
        Span::raw(" "),
    ];
    // User overrides take precedence over both `aiTitle` and the
    // task-toml-derived title (the 2026-05-21 rename feature). A
    // newly-arriving AI title does *not* clobber the override — once
    // the user named something, they meant it.
    if let Some(name) = name_override {
        spans.push(Span::styled(name.to_string(), title_style));
    } else if let Some(title) = &session.title {
        spans.push(Span::styled(title.clone(), title_style));
    } else {
        let id = &session.id.0;
        let suffix = if id.len() > 6 {
            &id[id.len() - 6..]
        } else {
            id.as_str()
        };
        // Title-less rows are already dim; the live-pane signal would
        // be invisible on top, so keep the existing styling.
        spans.push(Span::styled(format!("({suffix})"), dim));
    }
    spans.push(Span::raw("  "));
    spans.push(Span::styled(age, dim));
    Line::from(spans)
}

/// Theme lookup for the attention-state glyph colour. Returns `None`
/// (terminal default) for any state the user hasn't explicitly themed —
/// the default config only colours `NeedsInput`/`Working`/etc. when the
/// user opts in, so an empty `[theme]` section preserves the pre-M5
/// uncoloured look.
fn attention_color(a: Attention, theme: &Theme) -> Option<ratatui::style::Color> {
    match a {
        Attention::NeedsInput => theme.needs_input,
        Attention::Working => theme.working,
        Attention::Idle => theme.idle,
        Attention::Unknown => theme.unknown,
    }
}

fn effective_attention(session: &Session) -> Attention {
    if let Ok(elapsed) = session.last_activity.elapsed()
        && elapsed > IDLE_THRESHOLD
    {
        return Attention::Idle;
    }
    session.attention
}

fn attention_glyph(a: Attention) -> &'static str {
    match a {
        Attention::NeedsInput => "●",
        Attention::Working => "◐",
        Attention::Idle => "○",
        Attention::Unknown => "·",
    }
}

fn display_path(path: &Path, home: Option<&Path>) -> String {
    if let Some(h) = home
        && let Ok(suffix) = path.strip_prefix(h)
    {
        return format!("~/{}", suffix.display());
    }
    path.display().to_string()
}

fn humanize_elapsed(t: SystemTime) -> String {
    let Ok(elapsed) = t.elapsed() else {
        return "future".to_string();
    };
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_connect_errors() -> Vec<(HostId, String)> {
        Vec::new()
    }

    fn no_empty_scans() -> Vec<(HostId, Vec<PathBuf>)> {
        Vec::new()
    }

    fn tool(key: char, command: &[&str]) -> ToolBinding {
        ToolBinding {
            key,
            name: None,
            command: command.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn plain_key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn should_capture_mouse_drops_shift_modified_events() {
        // Regression: dragging with Shift held used to be swallowed by
        // mouse capture, suppressing host-terminal native selection.
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let ev = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 10,
            row: 5,
            modifiers: KeyModifiers::SHIFT,
        };
        assert!(!should_capture_mouse(ev));
    }

    #[test]
    fn should_capture_mouse_accepts_unmodified_events() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::ScrollUp,
            MouseEventKind::ScrollDown,
        ] {
            let ev = MouseEvent {
                kind,
                column: 10,
                row: 5,
                modifiers: KeyModifiers::NONE,
            };
            assert!(
                should_capture_mouse(ev),
                "unmodified {kind:?} should capture"
            );
        }
    }

    #[test]
    fn should_capture_mouse_passes_ctrl_and_alt_events_through_to_pty() {
        // Only Shift triggers the bypass — Ctrl- and Alt-mouse are
        // legitimate input shapes that the inner child may interpret.
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        for mods in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            let ev = MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 10,
                row: 5,
                modifiers: mods,
            };
            assert!(
                should_capture_mouse(ev),
                "modifier {mods:?} should not bypass"
            );
        }
    }

    #[test]
    fn leader_chord_armed_esc_returns_to_sidebar() {
        let focus = Focus::Terminal { leader_armed: true };
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(
            leader_chord_transition(focus, &key),
            LeaderChordTransition::EscapeToSidebar,
        );
    }

    #[test]
    fn leader_chord_armed_esc_with_stray_modifier_still_returns_to_sidebar() {
        // Regression for the 2026-05-20 "stray newline on session
        // swap" bug. Under extended keyboard protocols Esc can arrive
        // with stray modifier bits; the transition must still fire,
        // otherwise focus stays in Terminal and the user's subsequent
        // navigation + Enter keys leak into the inner PTY.
        for mods in [
            KeyModifiers::SHIFT,
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::SHIFT | KeyModifiers::CONTROL,
        ] {
            let focus = Focus::Terminal { leader_armed: true };
            let key = KeyEvent::new(KeyCode::Esc, mods);
            assert_eq!(
                leader_chord_transition(focus, &key),
                LeaderChordTransition::EscapeToSidebar,
                "Esc with modifiers {mods:?} should escape",
            );
        }
    }

    #[test]
    fn leader_chord_armed_non_esc_forwards_both_to_pty() {
        let focus = Focus::Terminal { leader_armed: true };
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);
        assert_eq!(
            leader_chord_transition(focus, &key),
            LeaderChordTransition::ForwardBothToPty,
        );
    }

    #[test]
    fn leader_chord_unarmed_leader_arms() {
        let focus = Focus::Terminal {
            leader_armed: false,
        };
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(
            leader_chord_transition(focus, &key),
            LeaderChordTransition::ArmLeader,
        );
    }

    #[test]
    fn leader_chord_unarmed_non_leader_encodes_and_forwards() {
        let focus = Focus::Terminal {
            leader_armed: false,
        };
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            leader_chord_transition(focus, &key),
            LeaderChordTransition::EncodeAndForward,
        );
    }

    fn idle_session(elapsed: Duration, attention: Attention) -> Session {
        Session {
            id: SessionId("audit".into()),
            host: HostId::local(),
            project_dir: PathBuf::from("/proj"),
            transcript_path: PathBuf::from("/t/x.jsonl"),
            last_activity: SystemTime::now()
                .checked_sub(elapsed)
                .unwrap_or(SystemTime::UNIX_EPOCH),
            attention,
            title: None,
            parent_repo: None,
            has_live_pane: None,
            hook_pinned: None,
        }
    }

    #[test]
    fn effective_attention_preserves_active_state_under_idle_threshold() {
        // A session with recent activity keeps its current attention
        // state regardless of value — the idle promotion only kicks
        // in at the 1h boundary.
        for attn in [
            Attention::NeedsInput,
            Attention::Working,
            Attention::Idle,
            Attention::Unknown,
        ] {
            let s = idle_session(Duration::from_secs(60), attn);
            assert_eq!(
                effective_attention(&s),
                attn,
                "recent activity should preserve {attn:?}",
            );
        }
    }

    #[test]
    fn effective_attention_promotes_to_idle_past_threshold() {
        // Past IDLE_THRESHOLD any non-Idle state collapses to Idle —
        // the row reads as "this hasn't moved in a while". Pins the
        // documented 1h boundary in `main.rs:IDLE_THRESHOLD`.
        let beyond = IDLE_THRESHOLD + Duration::from_secs(60);
        for attn in [Attention::NeedsInput, Attention::Working] {
            let s = idle_session(beyond, attn);
            assert_eq!(effective_attention(&s), Attention::Idle);
        }
    }

    #[test]
    fn effective_attention_just_under_threshold_keeps_active_state() {
        // The threshold is strict greater-than; activity within the
        // 1h window stays active. Tested an absolute second under so
        // the elapsed-since-`now()` clock drift in the harness doesn't
        // race the boundary.
        let just_under = IDLE_THRESHOLD
            .checked_sub(Duration::from_secs(1))
            .expect("IDLE_THRESHOLD > 1s");
        let s = idle_session(just_under, Attention::Working);
        assert_eq!(effective_attention(&s), Attention::Working);
    }

    #[test]
    fn focus_border_style_focused_is_bold_cyan() {
        let s = focus_border_style(true);
        assert_eq!(s.fg, Some(ratatui::style::Color::Cyan));
        assert!(s.add_modifier.contains(Modifier::BOLD));
        assert!(!s.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn focus_border_style_unfocused_is_dim_no_colour() {
        let s = focus_border_style(false);
        assert_eq!(s.fg, None);
        assert!(s.add_modifier.contains(Modifier::DIM));
        assert!(!s.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn action_for_dispatches_tool_keybinds_after_builtins() {
        // `g` is not a built-in key; with a tool bound to `g`, action_for
        // returns LaunchTool with the correct index.
        let tools = vec![tool('g', &["lazygit"]), tool('v', &["nvim"])];
        assert!(matches!(
            action_for(plain_key('g'), &tools),
            Some(Action::LaunchTool(0))
        ));
        assert!(matches!(
            action_for(plain_key('v'), &tools),
            Some(Action::LaunchTool(1))
        ));
    }

    #[test]
    fn action_for_built_in_keys_take_priority_over_tool_keys() {
        // Defensive: even if a (buggy) validator let a tool slip
        // through with `key = 'q'`, the built-in match arms come first
        // and quit still wins. This is the second line of defence
        // behind config-load validation.
        let tools = vec![tool('q', &["never-fires"])];
        assert!(matches!(
            action_for(plain_key('q'), &tools),
            Some(Action::Quit)
        ));
    }

    #[test]
    fn action_for_returns_none_for_unbound_key() {
        let tools = vec![tool('g', &["lazygit"])];
        assert!(action_for(plain_key('x'), &tools).is_none());
    }

    #[test]
    fn action_for_dispatches_capital_n_to_new_session_no_worktree() {
        // `n` opens the worktree-creating flow; `N` opens the same
        // picker in no-worktree mode (raised by dogfooding 2026-05-19).
        // Crossterm reports shifted letters as the uppercase glyph, so
        // we match on the literal 'N' rather than 'n' + SHIFT.
        let tools: Vec<ToolBinding> = vec![];
        assert!(matches!(
            action_for(plain_key('n'), &tools),
            Some(Action::NewSession)
        ));
        assert!(matches!(
            action_for(plain_key('N'), &tools),
            Some(Action::NewSessionNoWorktree)
        ));
    }

    #[test]
    fn footer_keybind_line_advertises_no_worktree_variant() {
        // The user needs to know `N` exists — without a hint they'd
        // never discover it. Inline next to `n: new` so the relationship
        // between the two is visible at a glance.
        let s = compose_footer(
            None,
            None,
            false,
            true,
            0,
            &no_connect_errors(),
            &no_empty_scans(),
            Focus::Sidebar,
            &[],
        );
        assert!(s.contains("N: no worktree"), "got: {s}");
    }

    #[test]
    fn footer_keybind_line_shows_in_tmux_return_hint() {
        let s = compose_footer(
            None,
            None,
            false,
            true,
            0,
            &no_connect_errors(),
            &no_empty_scans(),
            Focus::Sidebar,
            &[],
        );
        assert!(s.contains("return: prefix+s"), "got: {s}");
        assert!(!s.contains("prefix+d"), "got: {s}");
    }

    #[test]
    fn footer_keybind_line_lists_configured_tools_with_name_fallback() {
        // Tools surface inline so the user can spot their custom binds
        // without checking config. Label precedence: explicit `name`
        // wins, else `command[0]`. Empty tools list adds nothing.
        let tools = vec![
            tool('g', &["lazygit"]),
            ToolBinding {
                key: 'v',
                name: Some("edit".to_string()),
                command: vec!["nvim".into(), ".".into()],
            },
        ];
        let s = compose_footer(
            None,
            None,
            false,
            true,
            0,
            &no_connect_errors(),
            &no_empty_scans(),
            Focus::Sidebar,
            &tools,
        );
        assert!(s.contains("g: lazygit"), "got: {s}");
        assert!(s.contains("v: edit"), "got: {s}");
        // Order is "...d: delete · <tools> · q: quit": tool hints land
        // between the worktree-delete affordance and the trailing quit.
        let d_pos = s.find("d: delete").expect("delete hint present");
        let q_pos = s.find("q: quit").expect("quit hint present");
        let g_pos = s.find("g: lazygit").expect("tool g present");
        assert!(
            d_pos < g_pos && g_pos < q_pos,
            "tools must sit between delete and quit: {s}"
        );
    }

    #[test]
    fn footer_keybind_line_omits_tool_separator_when_no_tools_configured() {
        // No tools means the footer reads "…d: delete · q: quit…" with
        // no orphan ` · ` between them — guarding against an empty
        // join leaving a dangling separator.
        let s = compose_footer(
            None,
            None,
            false,
            true,
            0,
            &no_connect_errors(),
            &no_empty_scans(),
            Focus::Sidebar,
            &[],
        );
        assert!(s.contains("d: delete · q: quit"), "got: {s}");
    }

    #[test]
    fn footer_keybind_line_advertises_delete_action() {
        // Pin that `d: delete` lands in the keybind line — a regression
        // here would leave the action discoverable only by reading the
        // source (it doesn't have a separate visible affordance the way
        // `n: new` does via the modal it opens).
        let s = compose_footer(
            None,
            None,
            false,
            true,
            0,
            &no_connect_errors(),
            &no_empty_scans(),
            Focus::Sidebar,
            &[],
        );
        assert!(s.contains("d: delete"), "got: {s}");
    }

    #[test]
    fn footer_keybind_line_advertises_group_jumps() {
        let s = compose_footer(
            None,
            None,
            false,
            true,
            0,
            &no_connect_errors(),
            &no_empty_scans(),
            Focus::Sidebar,
            &[],
        );
        assert!(s.contains("J/K: project"), "got: {s}");
        assert!(s.contains("⌃j/⌃k: host"), "got: {s}");
    }

    #[test]
    fn footer_keybind_line_shows_outside_tmux_return_hint() {
        let s = compose_footer(
            None,
            None,
            false,
            false,
            0,
            &no_connect_errors(),
            &no_empty_scans(),
            Focus::Sidebar,
            &[],
        );
        assert!(s.contains("return: prefix+d"), "got: {s}");
        assert!(!s.contains("prefix+s"), "got: {s}");
    }

    #[test]
    fn footer_empty_catalog_omits_return_hint() {
        let s = compose_footer(
            None,
            None,
            true,
            true,
            0,
            &no_connect_errors(),
            &no_empty_scans(),
            Focus::Sidebar,
            &[],
        );
        assert!(!s.contains("return:"), "got: {s}");
        assert!(s.contains("no sessions"), "got: {s}");
    }

    #[test]
    fn footer_status_takes_precedence_over_keybinds() {
        let s = compose_footer(
            None,
            Some("attach: boom"),
            false,
            true,
            0,
            &no_connect_errors(),
            &no_empty_scans(),
            Focus::Sidebar,
            &[],
        );
        assert!(s.contains("attach: boom"), "got: {s}");
        assert!(!s.contains("return:"), "got: {s}");
    }

    #[test]
    fn footer_creating_takes_precedence_over_status() {
        let creating = CreatingSession {
            repo_name: "agent-mux".into(),
            task: "refactor".into(),
        };
        let s = compose_footer(
            Some(&creating),
            Some("ignored"),
            false,
            true,
            0,
            &no_connect_errors(),
            &no_empty_scans(),
            Focus::Sidebar,
            &[],
        );
        assert!(s.contains("creating worktree"), "got: {s}");
        assert!(s.contains("agent-mux"), "got: {s}");
        assert!(!s.contains("ignored"), "got: {s}");
    }

    #[test]
    fn footer_keybind_line_appends_connecting_suffix_when_hosts_pending() {
        let s = compose_footer(
            None,
            None,
            false,
            true,
            2,
            &no_connect_errors(),
            &no_empty_scans(),
            Focus::Sidebar,
            &[],
        );
        assert!(s.contains("return: prefix+s"), "got: {s}");
        assert!(s.contains("connecting to 2 host(s)"), "got: {s}");
    }

    #[test]
    fn footer_empty_catalog_swaps_no_sessions_for_connecting_when_hosts_pending() {
        // First impression matters: when the catalog is empty *and*
        // remote discovery is still in flight, "no sessions discovered"
        // would mis-imply we're done.
        let s = compose_footer(
            None,
            None,
            true,
            true,
            1,
            &no_connect_errors(),
            &no_empty_scans(),
            Focus::Sidebar,
            &[],
        );
        assert!(s.contains("connecting to 1 host(s)"), "got: {s}");
        assert!(!s.contains("no sessions"), "got: {s}");
    }

    #[test]
    fn footer_connecting_suffix_disappears_once_all_hosts_have_reported() {
        let s = compose_footer(
            None,
            None,
            false,
            true,
            0,
            &no_connect_errors(),
            &no_empty_scans(),
            Focus::Sidebar,
            &[],
        );
        assert!(!s.contains("connecting to"), "got: {s}");
    }

    #[test]
    fn footer_renders_single_empty_scan_with_host_and_first_folder() {
        // The dogfood-triggered hint: when a remote scan returns
        // zero repos, name the host and the first folder it searched
        // so the user can immediately see what to fix. Bounded length
        // — only the first folder is shown even if multiple were
        // configured, to keep the line readable.
        let empty = vec![(
            HostId("alpenglow".into()),
            vec![PathBuf::from("/home/gizmo/workspace")],
        )];
        let s = compose_footer(
            None,
            None,
            false,
            true,
            0,
            &no_connect_errors(),
            &empty,
            Focus::Sidebar,
            &[],
        );
        assert!(s.contains("alpenglow:"), "got: {s}");
        assert!(s.contains("/home/gizmo/workspace"), "got: {s}");
    }

    #[test]
    fn footer_aggregates_multiple_empty_scans_into_host_list() {
        // Multiple hosts with empty scans: aggregate to "no repos
        // found: a, b" rather than concatenating per-host folder
        // info, which would truncate beyond usefulness. User can
        // `agent-mux config` for the per-host folders.
        let empty = vec![
            (
                HostId("alpenglow".into()),
                vec![PathBuf::from("/srv/alpenglow/workspace")],
            ),
            (
                HostId("beta".into()),
                vec![PathBuf::from("/srv/beta/workspace")],
            ),
        ];
        let s = compose_footer(
            None,
            None,
            false,
            true,
            0,
            &no_connect_errors(),
            &empty,
            Focus::Sidebar,
            &[],
        );
        assert!(s.contains("no repos found:"), "got: {s}");
        assert!(s.contains("alpenglow"), "got: {s}");
        assert!(s.contains("beta"), "got: {s}");
    }

    #[test]
    fn footer_connect_errors_take_precedence_over_empty_scans() {
        // A host that failed to even connect is a more severe signal
        // than one that connected but found no repos. Connect errors
        // win the slot when both are present; user fixes the connect
        // failure first, sees the empty-scan once that clears.
        let errors = vec![(HostId("alpenglow".into()), "ssh exit 255".to_string())];
        let empty = vec![(
            HostId("beta".into()),
            vec![PathBuf::from("/srv/beta/workspace")],
        )];
        let s = compose_footer(
            None,
            None,
            false,
            true,
            0,
            &errors,
            &empty,
            Focus::Sidebar,
            &[],
        );
        assert!(s.contains("connect failed"), "got: {s}");
        assert!(!s.contains("no repos"), "got: {s}");
    }

    #[test]
    fn footer_renders_connect_errors_as_sticky_line_when_no_status() {
        let errors = vec![(HostId("alpenglow".into()), "ssh exit 255".to_string())];
        let s = compose_footer(
            None,
            None,
            false,
            true,
            0,
            &errors,
            &no_empty_scans(),
            Focus::Sidebar,
            &[],
        );
        assert!(s.contains("connect failed: alpenglow"), "got: {s}");
        assert!(!s.contains("return:"), "got: {s}");
    }

    #[test]
    fn footer_lists_all_failed_host_names_comma_separated() {
        // The fix the review surfaced: a second failure must not
        // silently overwrite the first. Host names get rendered;
        // the full error text is retained off-screen for future log
        // surfacing.
        let errors = vec![
            (HostId("alpenglow".into()), "first error".to_string()),
            (HostId("gpu-1".into()), "second error".to_string()),
        ];
        let s = compose_footer(
            None,
            None,
            false,
            true,
            0,
            &errors,
            &no_empty_scans(),
            Focus::Sidebar,
            &[],
        );
        assert!(s.contains("alpenglow"), "got: {s}");
        assert!(s.contains("gpu-1"), "got: {s}");
    }

    #[test]
    fn footer_transient_status_takes_precedence_over_connect_errors() {
        // A fresh action's feedback must not be drowned out by the
        // sticky connect-failure line.
        let errors = vec![(HostId("alpenglow".into()), "ssh exit 255".to_string())];
        let s = compose_footer(
            None,
            Some("opened terminal in /x"),
            false,
            true,
            0,
            &errors,
            &no_empty_scans(),
            Focus::Sidebar,
            &[],
        );
        assert!(s.contains("opened terminal"), "got: {s}");
        assert!(!s.contains("connect failed"), "got: {s}");
    }

    // ------- compose_search_bar -------

    #[test]
    fn search_bar_editing_mode_renders_query_with_cursor_and_apply_hint() {
        let mut s = SearchState::new();
        s.query = "refactor".into();
        let bar = compose_search_bar(&s, 3);
        assert!(bar.contains("/refactor█"), "got: {bar}");
        assert!(bar.contains("3 matches"), "got: {bar}");
        assert!(bar.contains("⏎ apply"), "got: {bar}");
        assert!(bar.contains("esc cancel"), "got: {bar}");
    }

    #[test]
    fn search_bar_active_mode_renders_query_without_cursor_and_clear_hint() {
        let mut s = SearchState::new();
        s.query = "refactor".into();
        s.mode = SearchMode::Active;
        let bar = compose_search_bar(&s, 3);
        assert!(bar.contains("/refactor "), "got: {bar}");
        assert!(
            !bar.contains('█'),
            "active mode should not show cursor: {bar}"
        );
        assert!(bar.contains("/ edit"), "got: {bar}");
        assert!(bar.contains("esc clear"), "got: {bar}");
    }

    #[test]
    fn search_bar_uses_singular_match_label_for_one_result() {
        let s = SearchState::new();
        let bar = compose_search_bar(&s, 1);
        assert!(bar.contains("1 match)"), "got: {bar}");
        // Plural form should not appear when count is 1.
        assert!(!bar.contains("1 matches"), "got: {bar}");
    }

    #[test]
    fn search_bar_uses_plural_match_label_for_zero_or_many() {
        let s = SearchState::new();
        assert!(compose_search_bar(&s, 0).contains("0 matches"));
        assert!(compose_search_bar(&s, 12).contains("12 matches"));
    }

    #[test]
    fn search_bar_empty_query_still_renders_cursor() {
        // The user has just pressed `/`; the bar must signal that the
        // keyboard is now theirs (cursor block) even before they type.
        let s = SearchState::new();
        let bar = compose_search_bar(&s, 5);
        assert!(bar.contains("/█"), "got: {bar}");
    }

    // ---- focus-aware footer ----

    #[test]
    fn footer_terminal_focus_shows_only_back_to_sidebar_hint() {
        // When the embedded terminal owns the keyboard, the j/k/⏎/etc
        // bindings the sidebar advertises don't work — listing them
        // would mislead the user. Footer collapses to the one hint
        // that does.
        let s = compose_footer(
            None,
            None,
            false,
            true,
            0,
            &no_connect_errors(),
            &no_empty_scans(),
            Focus::Terminal {
                leader_armed: false,
            },
            &[],
        );
        assert!(s.contains("back to sidebar"), "got: {s}");
        assert!(!s.contains("j/k"), "got: {s}");
        assert!(!s.contains("return:"), "got: {s}");
    }

    #[test]
    fn footer_terminal_focus_still_surfaces_transient_status() {
        // A creating-worktree status or a fresh error still takes
        // precedence in Terminal focus — the user needs to see
        // those signals even mid-session.
        let s = compose_footer(
            None,
            Some("attach: boom"),
            false,
            true,
            0,
            &no_connect_errors(),
            &no_empty_scans(),
            Focus::Terminal {
                leader_armed: false,
            },
            &[],
        );
        assert!(s.contains("attach: boom"), "got: {s}");
        assert!(!s.contains("back to sidebar"), "got: {s}");
    }

    // ---- should_resize_pty ----

    #[test]
    fn should_resize_pty_returns_false_for_unchanged_size() {
        // The hot-path case: 60 fps of unchanged geometry should not
        // trigger 60 SIGWINCHes/sec at the child.
        assert!(!should_resize_pty((24, 80), (24, 80)));
    }

    #[test]
    fn should_resize_pty_returns_true_when_dimensions_change() {
        assert!(should_resize_pty((24, 80), (40, 120)));
        assert!(should_resize_pty((24, 80), (24, 100)));
        assert!(should_resize_pty((24, 80), (30, 80)));
    }

    #[test]
    fn should_resize_pty_returns_false_for_zero_sized_proposed() {
        // Terminal shrunk past the borders: there's no meaningful
        // grid to render and vt100 rejects zero dimensions anyway.
        assert!(!should_resize_pty((24, 80), (0, 80)));
        assert!(!should_resize_pty((24, 80), (24, 0)));
        assert!(!should_resize_pty((24, 80), (0, 0)));
    }

    // ---- compute_actively_viewed ----

    fn sid(s: &str) -> SessionId {
        SessionId(s.to_string())
    }

    #[test]
    fn compute_actively_viewed_true_only_when_all_three_conditions_hold() {
        let id = sid("a");
        assert!(compute_actively_viewed(
            Focus::Terminal {
                leader_armed: false
            },
            true,
            Some(&id),
            &id,
        ));
    }

    #[test]
    fn compute_actively_viewed_false_when_terminal_not_os_focused() {
        // The regression that motivated extracting this helper: an
        // alt-tab away from agent-mux leaves the in-app `Focus::Terminal`
        // state intact, but the user isn't watching the pane any more
        // so the notification must fire.
        let id = sid("a");
        assert!(!compute_actively_viewed(
            Focus::Terminal {
                leader_armed: false
            },
            false,
            Some(&id),
            &id,
        ));
    }

    #[test]
    fn compute_actively_viewed_false_when_in_app_focus_is_sidebar() {
        let id = sid("a");
        assert!(!compute_actively_viewed(
            Focus::Sidebar,
            true,
            Some(&id),
            &id,
        ));
    }

    #[test]
    fn compute_actively_viewed_false_when_no_session_is_embedded() {
        let id = sid("a");
        assert!(!compute_actively_viewed(
            Focus::Terminal {
                leader_armed: false
            },
            true,
            None,
            &id,
        ));
    }

    #[test]
    fn compute_actively_viewed_false_when_embedded_session_differs_from_transition() {
        let embedded = sid("a");
        let other = sid("b");
        assert!(!compute_actively_viewed(
            Focus::Terminal {
                leader_armed: false
            },
            true,
            Some(&embedded),
            &other,
        ));
    }

    #[test]
    fn compute_actively_viewed_ignores_leader_armed_state() {
        // Mid-leader-chord is still "actively engaged"; the user is in
        // the middle of typing a sequence, not browsing the dashboard.
        let id = sid("a");
        assert!(compute_actively_viewed(
            Focus::Terminal { leader_armed: true },
            true,
            Some(&id),
            &id,
        ));
    }
}
