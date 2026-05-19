use std::collections::{HashMap, HashSet};
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, SystemTime};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use agent_mux::attachment::{
    AttachOutcome, AttachmentDriver, EmbedSpec, PtyDriver, SuspendCommand, TmuxDriver,
};
use agent_mux::cache;
use agent_mux::catalog::SessionCatalog;
use agent_mux::cli;
use agent_mux::config::{self, Config, Theme};
use agent_mux::dashboard::{
    DisplayRow, Focus, PreviewEntry, SearchMode, SearchOutcome, SearchState, apply_fg,
    build_display_rows, build_display_rows_filtered, compose_preview_pane_lines,
    first_session_index, is_pty_leader, matches_query, next_host_index, next_project_index,
    next_session_index, prev_host_index, prev_project_index, prev_session_index,
};
use agent_mux::discovery::{build_session, claude_projects_dir, discover};
use agent_mux::embedded_pty::{EmbeddedPty, PtyEvent, encode_key_for_pty};
use agent_mux::host::{Host, LocalHost, SshHost};
use agent_mux::new_session_modal::{KeyOutcome, NewSessionModal};
use agent_mux::notifications::{LibNotifyDispatcher, Notifier, Transition};
use agent_mux::preview::parse_preview;
use agent_mux::repo::{Repo, RepoRegistry, scan_host_workspaces};
use agent_mux::session::{Attention, HostId, Session, SessionId};
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

/// Bytes of transcript tail to fetch for each preview. Enough for a
/// double-digit number of entries on typical sessions even after the
/// JSONL grows verbose with usage metadata and thinking blocks. M5
/// candidate for `[preview]` config.
const PREVIEW_BYTES: u64 = 64 * 1024;

/// Maximum `PreviewLine`s the parser keeps from the transcript tail.
/// This is the source-entry cap; the renderer then trims composed
/// visual lines to the actual pane height at render time, so the cap
/// only matters as an upper bound on how much we're willing to render
/// on a very tall terminal. 100 covers ~50-row preview panes even when
/// every entry is a single-line tool call, with headroom for longer
/// multi-line assistant replies. M5 candidate for `[preview]` config.
const PREVIEW_LIMIT: usize = 100;

fn main() -> io::Result<()> {
    // `args().skip(1)` drops argv[0] (program path). Anything left is
    // a subcommand. No subcommand → launch the TUI; that's the
    // original behaviour and the common case.
    //
    // `--embedded` is a TUI-only flag intentionally undocumented for
    // M5: it opts the run into the Phase-2 embedded-PTY `AttachmentDriver`
    // arc. With Phase-2 wiring alone it just surfaces a "not yet wired"
    // status on attach — usable for verifying the dispatch seam end-to-end.
    // Phase 3 lands the actual embedded widget; Phase 6 documents the flag
    // (and flips its default).
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    let embedded = argv.iter().any(|s| s == "--embedded");
    argv.retain(|s| s != "--embedded");
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
    let mut app = App::new(driver)?;
    let mut terminal = setup_terminal()?;
    let result = run(&mut terminal, &mut app);
    restore_terminal(&mut terminal)?;
    result
}

/// Whether to emit ANSI escapes from non-TUI subcommands. When stdout
/// is a real terminal we want the colour swatch; piped into a file or
/// pager that doesn't strip escapes we want plain text.
fn stdout_is_terminal() -> bool {
    use std::io::IsTerminal;
    io::stdout().is_terminal()
}

fn enter_screen() -> io::Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    Ok(())
}

fn leave_screen() -> io::Result<()> {
    execute!(io::stdout(), LeaveAlternateScreen)?;
    disable_raw_mode()?;
    Ok(())
}

fn setup_terminal() -> io::Result<Tui> {
    enter_screen()?;
    Terminal::new(CrosstermBackend::new(io::stdout()))
}

fn restore_terminal(terminal: &mut Tui) -> io::Result<()> {
    leave_screen()?;
    terminal.show_cursor()?;
    Ok(())
}

/// Release the alt-screen and raw mode so a foreground subprocess can use
/// the terminal, run it, then re-enter and force a redraw. Used when the
/// attachment driver hands a command off (e.g. `tmux attach` from outside
/// tmux, or `$SHELL` for an outside-tmux spawn-terminal).
fn suspend_and_run(terminal: &mut Tui, cmd: &SuspendCommand) -> io::Result<Option<String>> {
    leave_screen()?;
    let mut process = Command::new(&cmd.program);
    process.args(&cmd.args);
    if let Some(cwd) = &cmd.cwd {
        process.current_dir(cwd);
    }
    let status = process.status();
    enter_screen()?;
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
    updates: Receiver<WatcherEvent>,
    driver: Box<dyn AttachmentDriver>,
    status: Option<String>,
    config: Config,
    registry: RepoRegistry,
    modal: Option<NewSessionModal>,
    create_tx: Sender<NewSessionResult>,
    create_rx: Receiver<NewSessionResult>,
    creating: Option<CreatingSession>,
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
    /// M3 preview pane. `false` = hidden, `true` = visible (right
    /// split of the list area). Toggled by `p`. Independent from
    /// `preview_cache`: closing the pane keeps cached previews so
    /// re-opening is instant for sessions the user already looked at.
    preview_open: bool,
    /// Per-session preview cache. Insertion semantics double as
    /// in-flight deduplication (see [`PreviewEntry`]): a `Loading`
    /// entry means a fetch thread is already running for that
    /// session, so successive selection changes don't stack
    /// concurrent reads. An attention event for a session invalidates
    /// its cache entry (the transcript advanced, refetch on next tick).
    preview_cache: HashMap<SessionId, PreviewEntry>,
    /// Sender cloned into each preview-fetch thread.
    preview_tx: Sender<PreviewResult>,
    /// Drained each tick — completed previews flow in here from the
    /// fetch threads and land in `preview_cache`.
    preview_rx: Receiver<PreviewResult>,
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
struct Embedded {
    pty: EmbeddedPty,
    session_id: SessionId,
    last_size: (u16, u16),
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

/// One remote host's freshly-scanned repo list, returned from the
/// background thread spawned on `Connected`. Drained each tick; the
/// dashboard `reconcile_host`s the registry with the result so the
/// next picker-open shows live remote repos. An empty `repos` is a
/// valid value (host has no workspaces configured, or none of the
/// configured ones contain repos).
struct RepoScanResult {
    host_id: HostId,
    repos: Vec<Repo>,
}

/// Payload sent from a preview-fetch thread back to the main loop. The
/// receiver inserts `entry` into `preview_cache` keyed by `session_id`.
/// Errors flow as `Failed` rather than a separate channel so cache state
/// stays unified — every fetch leaves a definitive entry behind.
struct PreviewResult {
    session_id: SessionId,
    entry: PreviewEntry,
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
    fn new(driver: Box<dyn AttachmentDriver>) -> io::Result<Self> {
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
        // Drop the original Sender so the channel closes once every
        // background thread has sent its result and exited. The
        // per-thread clones above are the only live Senders.
        drop(remote_tx);

        // Pane-presence polling for the local host. Remote hosts get
        // theirs in `drain_remote_discoveries` once each `SshHost`
        // is connected. The 3s cadence matches the remote-attention
        // poll — fast enough that a pane killed by `prefix+&` reflects
        // in the dashboard within one tick.
        watcher.start_pane_polling_host(Arc::clone(&local_host), REMOTE_POLL_INTERVAL);

        let (preview_tx, preview_rx) = channel();

        // Extract before the struct literal moves `config` — Rust
        // evaluates struct fields in source order, so `notifier:` (last)
        // can't borrow `config` after `config:` (earlier) has taken it.
        let notifier = Notifier::new(Box::new(LibNotifyDispatcher), config.notifications.clone());
        let theme = Theme::from_config(&config.theme).map_err(io::Error::other)?;

        Ok(Self {
            catalog,
            list_state,
            home: dirs::home_dir(),
            hosts,
            watcher,
            updates,
            driver,
            status: None,
            config,
            registry,
            modal: None,
            create_tx,
            create_rx,
            repo_scan_tx,
            repo_scan_rx,
            creating: None,
            remote_rx,
            pending_hosts,
            connect_errors: Vec::new(),
            in_tmux: std::env::var_os("TMUX").is_some(),
            search: None,
            preview_open: false,
            preview_cache: HashMap::new(),
            preview_tx,
            preview_rx,
            notifier,
            theme,
            embedded: None,
            focus: Focus::default(),
        })
    }

    /// Display rows for the *current* view — filtered when search is
    /// active with a non-empty query, otherwise the full layout.
    /// Centralised here so every consumer (draw, navigation,
    /// selection resolution) sees the same set of rows; otherwise a
    /// j/k stroke could walk a list shape the user can't actually see.
    fn current_rows(&self) -> Vec<DisplayRow> {
        match self.search.as_ref() {
            Some(s) if !s.query.is_empty() => {
                let q = s.query.to_lowercase();
                let sessions = self.catalog.sessions();
                build_display_rows_filtered(sessions, |i| matches_query(&sessions[i], &q))
            }
            _ => build_display_rows(self.catalog.sessions()),
        }
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
    /// refreshes that host without disturbing the others.
    fn drain_repo_scans(&mut self) {
        while let Ok(result) = self.repo_scan_rx.try_recv() {
            self.registry.reconcile_host(&result.host_id, result.repos);
        }
    }

    fn open_new_session(&mut self) {
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
        self.modal = Some(NewSessionModal::new(
            self.registry.repos().to_vec(),
            ready_hosts,
        ));
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
        }
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
                        Ok(AttachOutcome::EmbedPty(_)) => {
                            // Today's `PtyDriver::spawn_session` delegates
                            // to `TmuxDriver`, so this arm is defensive —
                            // it stops a future driver impl that *does*
                            // emit `EmbedPty` for new-session creation
                            // from panicking the binary. The worktree is
                            // already on disk; surfacing a clear status
                            // means dogfooders can re-launch agent-mux
                            // without `--embedded` and continue.
                            self.status = Some(format!(
                                "worktree created at {} — embedded session spawn not yet wired (Phase 3)",
                                path.display()
                            ));
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

    fn drain_updates(&mut self) {
        while let Ok(event) = self.updates.try_recv() {
            match event {
                WatcherEvent::Attention(update) => {
                    let prev = self.catalog.update_attention(&update.id, update.attention);
                    // Transcript advanced — drop the cached preview so
                    // the next `ensure_preview_for_selected` refetches.
                    // Stale previews are worse than a brief "loading…"
                    // because the user can't tell from the dashboard
                    // when the transcript moved underneath them.
                    self.preview_cache.remove(&update.id);
                    if let Some(prev) = prev {
                        self.fire_attention_notification(&update.id, prev, update.attention);
                    }
                }
                WatcherEvent::NewTranscript { host, path, mtime } => {
                    self.handle_new_transcript(&host, &path, mtime);
                }
                WatcherEvent::LivePanes { host, cwds } => {
                    let set: HashSet<PathBuf> = cwds.into_iter().collect();
                    self.catalog.apply_live_panes(&host, &set);
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
        self.notifier.on_attention_update(
            &Transition {
                id,
                prev,
                new,
                title,
                host: &host,
                project: &project,
            },
            SystemTime::now(),
        );
    }

    fn toggle_preview(&mut self) {
        self.preview_open = !self.preview_open;
    }

    /// Drain completed preview fetches into the cache. Each entry
    /// supersedes whatever was there (the `Loading` placeholder, or
    /// a stale `Ready`/`Failed` if the cache was invalidated mid-flight).
    fn drain_preview_results(&mut self) {
        while let Ok(result) = self.preview_rx.try_recv() {
            self.preview_cache.insert(result.session_id, result.entry);
        }
    }

    /// If the preview pane is open and the selected session has no
    /// cached entry yet, spawn a background fetch. `Loading` goes in
    /// the cache synchronously so a flurry of selection changes
    /// dispatches at most one fetch per session — see [`PreviewEntry`].
    ///
    /// Silently no-ops when the pane is closed, no session is selected,
    /// or the session's `Host` impl isn't connected yet (the cached
    /// remote case before the SSH handshake completes — the entry
    /// stays absent, and the next tick after the host lands will
    /// dispatch).
    fn ensure_preview_for_selected(&mut self) {
        if !self.preview_open {
            return;
        }
        let Some(session) = self.selected_session() else {
            return;
        };
        let id = session.id.clone();
        if self.preview_cache.contains_key(&id) {
            return;
        }
        let Some(host) = self.hosts.get(&session.host).cloned() else {
            return;
        };
        let path = session.transcript_path.clone();
        self.preview_cache.insert(id.clone(), PreviewEntry::Loading);
        let tx = self.preview_tx.clone();
        std::thread::spawn(move || {
            let entry = match host.read_tail(&path, PREVIEW_BYTES) {
                Ok(text) => PreviewEntry::Ready(parse_preview(&text, PREVIEW_LIMIT)),
                Err(e) => PreviewEntry::Failed(e.to_string()),
            };
            let _ = tx.send(PreviewResult {
                session_id: id,
                entry,
            });
        });
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
    ///
    /// State machine:
    /// - Not armed, leader chord: arm and consume.
    /// - Armed, Esc: return focus to sidebar; PTY stays alive.
    /// - Armed, anything else: forward both the leader bytes and the
    ///   followup bytes to the PTY (tmux-style passthrough), then disarm.
    /// - Not armed, anything else: encode and forward to PTY.
    fn handle_terminal_key(&mut self, key: &KeyEvent) {
        if matches!(self.focus, Focus::Terminal { leader_armed: true }) {
            if key.code == KeyCode::Esc && key.modifiers.is_empty() {
                self.focus = Focus::Sidebar;
                return;
            }
            let leader_event = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
            let mut bytes = encode_key_for_pty(&leader_event);
            bytes.extend_from_slice(&encode_key_for_pty(key));
            self.write_to_embedded(&bytes);
            self.focus = Focus::Terminal {
                leader_armed: false,
            };
            return;
        }
        if is_pty_leader(key) {
            self.focus = Focus::Terminal { leader_armed: true };
            return;
        }
        let bytes = encode_key_for_pty(key);
        if !bytes.is_empty() {
            self.write_to_embedded(&bytes);
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
            self.embedded = None;
            self.focus = Focus::Sidebar;
        }
    }

    fn spawn_terminal_selected(&mut self) -> Option<SuspendCommand> {
        let result = {
            let session = self.selected_session()?;
            let host = self.hosts.get(&session.host)?.clone();
            let cwd = session.project_dir.clone();
            (self.driver.spawn_terminal(session, host.as_ref()), cwd)
        };
        match result {
            (Ok(AttachOutcome::Done), cwd) => {
                self.status = Some(format!("opened terminal in {}", cwd.display()));
                None
            }
            (Ok(AttachOutcome::SuspendAndRun(cmd)), _) => {
                self.status = None;
                Some(cmd)
            }
            (Ok(AttachOutcome::EmbedPty(_)), _) => {
                // Defensive — `PtyDriver::spawn_terminal` delegates to
                // `TmuxDriver`, so this arm shouldn't fire today.
                self.status = Some("embedded terminal not yet wired (Phase 3)".to_string());
                None
            }
            (Err(e), _) => {
                self.status = Some(format!("terminal: {e}"));
                None
            }
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
            && let Some(err) = suspend_and_run(terminal, &cmd)?
        {
            app.status = Some(err);
        }
        if has_event && let Event::Key(key) = event::read()? {
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
            let pending = match action_for(key) {
                Some(Action::Quit) => return Ok(()),
                Some(Action::Next) => {
                    app.next();
                    None
                }
                Some(Action::Prev) => {
                    app.prev();
                    None
                }
                Some(Action::NextProject) => {
                    app.next_project();
                    None
                }
                Some(Action::PrevProject) => {
                    app.prev_project();
                    None
                }
                Some(Action::NextHost) => {
                    app.next_host();
                    None
                }
                Some(Action::PrevHost) => {
                    app.prev_host();
                    None
                }
                Some(Action::Attach) => app.attach_selected(),
                Some(Action::SpawnTerminal) => app.spawn_terminal_selected(),
                Some(Action::NewSession) => {
                    app.open_new_session();
                    None
                }
                Some(Action::OpenSearch) => {
                    app.open_search();
                    None
                }
                Some(Action::TogglePreview) => {
                    app.toggle_preview();
                    None
                }
                None => None,
            };
            if let Some(cmd) = pending
                && let Some(err) = suspend_and_run(terminal, &cmd)?
            {
                app.status = Some(err);
            }
        }
        // Run on every loop iteration — covers all causes of "selected
        // session changed *or* host arrived *or* cache was invalidated"
        // without each call site needing to remember to dispatch.
        app.drain_preview_results();
        app.ensure_preview_for_selected();
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
    OpenSearch,
    TogglePreview,
}

fn action_for(key: KeyEvent) -> Option<Action> {
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
        KeyCode::Char('/') => Some(Action::OpenSearch),
        KeyCode::Char('p') => Some(Action::TogglePreview),
        _ => None,
    }
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &mut App) {
    // When search is active we add a dedicated 1-line bar between
    // the list and the footer. Keeping the footer separately means
    // the regular keybind line stays visible — search adds context,
    // it doesn't blot out the navigation hints.
    let constraints: Vec<Constraint> = if app.search.is_some() {
        vec![
            Constraint::Length(1), // header
            Constraint::Min(0),    // list
            Constraint::Length(1), // search bar
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
    let sessions_slice = app.catalog.sessions();
    let items: Vec<ListItem<'_>> = rows
        .iter()
        .map(|row| match row {
            DisplayRow::HostHeader(host) => ListItem::new(format_host_header(host)),
            DisplayRow::ProjectHeader(path) => {
                ListItem::new(format_project_header(path, app.home.as_deref()))
            }
            DisplayRow::SessionRow(i) => {
                ListItem::new(format_session_row(&sessions_slice[*i], &app.theme))
            }
        })
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▌ ");

    // Three layout modes for the main area (in priority order):
    // 1. Embedded PTY active → compact sidebar + embedded terminal
    //    (preview pane hidden — the terminal *is* the preview).
    // 2. Preview pane open → 55/45 list + inline preview.
    // 3. Default → list takes the full main area.
    if app.embedded.is_some() {
        draw_embedded(frame, app, list, layout[1]);
    } else if app.preview_open {
        let split = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(layout[1]);
        frame.render_stateful_widget(list, split[0], &mut app.list_state);

        let (entry, preview_title) = match app.selected_session() {
            Some(s) => (app.preview_cache.get(&s.id).cloned(), preview_pane_title(s)),
            None => (None, " preview ".to_string()),
        };
        // Inner height = pane height minus the top + bottom border.
        // This is the line budget the composer trims to so the newest
        // entry always sits at the bottom of the visible area.
        let max_preview_lines = usize::from(split[1].height.saturating_sub(2));
        let body = compose_preview_pane_lines(
            entry.as_ref(),
            app.list_state.selected().is_some(),
            &app.theme,
            max_preview_lines,
        );
        let preview = Paragraph::new(body)
            .block(Block::default().borders(Borders::ALL).title(preview_title))
            .wrap(Wrap { trim: true });
        frame.render_widget(preview, split[1]);
    } else {
        frame.render_stateful_widget(list, layout[1], &mut app.list_state);
    }

    let footer_idx = if let Some(search) = app.search.as_ref() {
        let bar_text = compose_search_bar(search, visible_sessions);
        let bar = Paragraph::new(Line::from(Span::styled(
            bar_text,
            // The bar is bold (not dim) — when the search owns the
            // keyboard, it's the foreground UI; dimming it would read
            // as inactive.
            Style::new().add_modifier(Modifier::BOLD),
        )));
        frame.render_widget(bar, layout[2]);
        3
    } else {
        2
    };

    let footer_text = compose_footer(
        app.creating.as_ref(),
        app.status.as_deref(),
        app.catalog.is_empty(),
        app.in_tmux,
        app.pending_hosts,
        &app.connect_errors,
        app.focus,
    );
    let footer = Paragraph::new(Line::from(Span::styled(
        footer_text,
        Style::new().add_modifier(Modifier::DIM),
    )));
    frame.render_widget(footer, layout[footer_idx]);

    if let Some(modal) = app.modal.as_mut() {
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
/// transient status > sticky connect-failure line > empty catalog >
/// keybind line. The keybind line gets a trailing "· connecting to N
/// host(s)…" suffix while remote SSH discovery is still pending. The
/// return hint is mode-aware because attach takes two different code
/// paths (see `App.in_tmux`).
///
/// Connect failures sit *below* transient status so a fresh action's
/// feedback isn't drowned out, but stay visible (until the next
/// transient status, then re-surface) so they're not silently lost
/// the way a single overwriting `status` field would lose them.
fn compose_footer(
    creating: Option<&CreatingSession>,
    status: Option<&str>,
    catalog_empty: bool,
    in_tmux: bool,
    pending_hosts: usize,
    connect_errors: &[(HostId, String)],
    focus: Focus,
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
    if catalog_empty {
        if pending_hosts > 0 {
            return format!(" connecting to {pending_hosts} host(s)… · n: new · q: quit ");
        }
        return " no sessions discovered · n: new · q: quit ".to_string();
    }
    let return_hint = if in_tmux { "prefix+s" } else { "prefix+d" };
    let suffix = if pending_hosts > 0 {
        format!("  ·  connecting to {pending_hosts} host(s)…")
    } else {
        String::new()
    };
    format!(
        " j/k: move · J/K: project · ⌃j/⌃k: host · ⏎: attach · t: terminal · p: preview · n: new · q: quit  ·  return: {return_hint}{suffix} "
    )
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
        .border_style(if term_focus {
            Style::new().add_modifier(Modifier::BOLD)
        } else {
            Style::new().add_modifier(Modifier::DIM)
        });
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

    frame.render_widget(term_block, split[1]);
    embedded.pty.render(frame, inner);
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

/// Title rendered in the embedded terminal's block. Mirrors
/// `preview_pane_title`'s fallback (last 6 of session id, "…" prefix)
/// for title-less sessions so the same anchor reads consistently
/// whether you're looking at the inline preview or the embedded pane.
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

/// Title for the preview pane's bordered block. Lifts the session's
/// title (or its id-suffix fallback) so the pane labels what the user
/// is looking at — without this the right pane and the list both
/// show the same content with no contextual anchor.
fn preview_pane_title(session: &Session) -> String {
    let label = session.title.as_deref().map_or_else(
        || {
            let suffix: String = session.id.0.chars().rev().take(6).collect();
            let suffix: String = suffix.chars().rev().collect();
            format!("…{suffix}")
        },
        str::to_string,
    );
    format!(" preview: {label} ")
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

fn format_session_row(session: &Session, theme: &Theme) -> Line<'static> {
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
    if let Some(title) = &session.title {
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

    #[test]
    fn footer_keybind_line_shows_in_tmux_return_hint() {
        let s = compose_footer(
            None,
            None,
            false,
            true,
            0,
            &no_connect_errors(),
            Focus::Sidebar,
        );
        assert!(s.contains("return: prefix+s"), "got: {s}");
        assert!(!s.contains("prefix+d"), "got: {s}");
    }

    #[test]
    fn footer_keybind_line_advertises_preview_toggle() {
        let s = compose_footer(
            None,
            None,
            false,
            true,
            0,
            &no_connect_errors(),
            Focus::Sidebar,
        );
        assert!(s.contains("p: preview"), "got: {s}");
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
            Focus::Sidebar,
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
            Focus::Sidebar,
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
            Focus::Sidebar,
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
            Focus::Sidebar,
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
            Focus::Sidebar,
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
            Focus::Sidebar,
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
            Focus::Sidebar,
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
            Focus::Sidebar,
        );
        assert!(!s.contains("connecting to"), "got: {s}");
    }

    #[test]
    fn footer_renders_connect_errors_as_sticky_line_when_no_status() {
        let errors = vec![(HostId("alpenglow".into()), "ssh exit 255".to_string())];
        let s = compose_footer(None, None, false, true, 0, &errors, Focus::Sidebar);
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
        let s = compose_footer(None, None, false, true, 0, &errors, Focus::Sidebar);
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
            Focus::Sidebar,
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
            Focus::Terminal {
                leader_armed: false,
            },
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
            Focus::Terminal {
                leader_armed: false,
            },
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
}
