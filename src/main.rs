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

use agent_mux::attachment::{AttachOutcome, AttachmentDriver, SuspendCommand, TmuxDriver};
use agent_mux::cache;
use agent_mux::catalog::SessionCatalog;
use agent_mux::cli;
use agent_mux::config::{Config, Theme};
use agent_mux::dashboard::{
    DisplayRow, PreviewEntry, SearchMode, SearchOutcome, SearchState, apply_fg, build_display_rows,
    build_display_rows_filtered, compose_preview_pane_lines, first_session_index, matches_query,
    next_host_index, next_project_index, next_session_index, prev_host_index, prev_project_index,
    prev_session_index,
};
use agent_mux::discovery::{build_session, claude_projects_dir, discover};
use agent_mux::host::{Host, LocalHost, SshHost};
use agent_mux::new_session_modal::{KeyOutcome, NewSessionModal};
use agent_mux::notifications::{LibNotifyDispatcher, Notifier, Transition};
use agent_mux::preview::parse_preview;
use agent_mux::repo::{Repo, RepoRegistry};
use agent_mux::session::{Attention, HostId, Session, SessionId};
use agent_mux::watcher::{REMOTE_POLL_INTERVAL, TranscriptWatcher, WatcherEvent, derive_attention};
use agent_mux::worktree::WorktreeManager;

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Sessions older than this are shown as Idle regardless of transcript
/// content. M0 default; will become configurable in M5.
const IDLE_THRESHOLD: Duration = Duration::from_secs(60 * 60);

/// Event-loop tick. Bounds the latency between an attention update arriving
/// in the channel and the dashboard re-rendering it.
const TICK: Duration = Duration::from_millis(100);

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
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut stdout = io::stdout();
    match argv.first().map(String::as_str) {
        None => run_tui(),
        Some("themes") => cli::print_themes(&mut stdout, stdout_is_terminal()),
        Some("config") => cli::print_config(&mut stdout),
        Some("help" | "--help" | "-h") => cli::print_help(&mut stdout),
        Some(other) => {
            let mut stderr = io::stderr();
            writeln!(stderr, "agent-mux: unknown subcommand {other:?}\n")?;
            cli::print_help(&mut stderr)?;
            std::process::exit(2);
        }
    }
}

fn run_tui() -> io::Result<()> {
    let mut app = App::new()?;
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
    driver: TmuxDriver,
    status: Option<String>,
    config: Config,
    registry: RepoRegistry,
    modal: Option<NewSessionModal>,
    create_tx: Sender<NewSessionResult>,
    create_rx: Receiver<NewSessionResult>,
    creating: Option<CreatingSession>,
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
}

/// Lives in `App.creating` while a worktree-create is in flight, so the
/// footer can show "creating worktree for X in Y…".
struct CreatingSession {
    repo_name: String,
    task: String,
}

/// Result from the background create thread. Drained on every tick.
enum NewSessionResult {
    Created(PathBuf),
    Failed(String),
}

/// Payload sent from a preview-fetch thread back to the main loop. The
/// receiver inserts `entry` into `preview_cache` keyed by `session_id`.
/// Errors flow as `Failed` rather than a separate channel so cache state
/// stays unified — every fetch leaves a definitive entry behind.
struct PreviewResult {
    session_id: SessionId,
    entry: PreviewEntry,
}

/// Payload from a background SSH-discovery thread. `Ready` carries the
/// connected `SshHost` (wrapped in `Arc` for trait-object insertion into
/// `App.hosts`), the sessions found on it (each with its initial
/// attention already computed against the remote transcript tail), and
/// the `transcript_root` used for discovery — the polling thread that
/// keeps attention live needs to rescan the same directory.
/// `Failed` carries the host's dashboard label and a one-line error so
/// the footer can show why a host is missing from the catalog.
enum RemoteDiscoveryResult {
    Ready {
        host_id: HostId,
        host: Arc<dyn Host>,
        sessions: Vec<Session>,
        transcript_root: PathBuf,
    },
    Failed {
        host_id: HostId,
        error: String,
    },
}

impl App {
    fn new() -> io::Result<Self> {
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
                let result = connect_and_discover(
                    host_id.clone(),
                    ssh_target,
                    transcript_root,
                    cache_dir_for_thread,
                );
                let _ = tx.send(result);
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
            driver: TmuxDriver::new(),
            status: None,
            config,
            registry,
            modal: None,
            create_tx,
            create_rx,
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
            self.pending_hosts = self.pending_hosts.saturating_sub(1);
            match result {
                RemoteDiscoveryResult::Ready {
                    host_id,
                    host,
                    sessions,
                    transcript_root,
                } => {
                    let poll_seed: Vec<_> = sessions
                        .iter()
                        .map(|s| (s.id.clone(), s.transcript_path.clone(), s.last_activity))
                        .collect();
                    self.hosts.insert(host_id.clone(), Arc::clone(&host));
                    // Capture selection *before* reconcile so we can
                    // re-seat it: a cached row the user already
                    // highlighted should stay highlighted when the
                    // live read overlays the same id; if the entry
                    // disappeared, selection falls back to the first
                    // visible session row.
                    let prior = self.selected_id_for_reseat();
                    self.catalog.reconcile_host(&host_id, sessions);
                    self.reseat_selection_to(prior.as_ref());
                    // Live polling: without this, remote attention stays
                    // frozen at the discovery reading.
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
                        .start_pane_polling_host(host, REMOTE_POLL_INTERVAL);
                }
                RemoteDiscoveryResult::Failed { host_id, error } => {
                    self.connect_errors.push((host_id, error));
                }
            }
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
        self.modal = Some(NewSessionModal::new(self.registry.repos().to_vec()));
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
        self.creating = Some(CreatingSession {
            repo_name: repo.name.clone(),
            task: task.clone(),
        });
        self.status = None;
        let tx = self.create_tx.clone();
        std::thread::spawn(move || {
            let outcome = match WorktreeManager.create(&repo.path, &base_branch, &task) {
                Ok(path) => NewSessionResult::Created(path),
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
                NewSessionResult::Created(path) => match self.driver.spawn_session(&path) {
                    Ok(AttachOutcome::Done) => {
                        self.status = Some(format!("started new session in {}", path.display()));
                    }
                    Ok(AttachOutcome::SuspendAndRun(cmd)) => {
                        self.status = None;
                        return Some(cmd);
                    }
                    Err(e) => {
                        self.status = Some(format!(
                            "worktree created at {} but spawn failed: {e}",
                            path.display()
                        ));
                    }
                },
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
        let result = {
            let session = self.selected_session()?;
            let host = self.hosts.get(&session.host)?.clone();
            self.driver.attach(session, host.as_ref())
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
            Err(e) => {
                self.status = Some(format!("attach: {e}"));
                None
            }
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
        let has_event = event::poll(TICK)?;
        app.drain_updates();
        app.drain_remote_discoveries();
        if let Some(cmd) = app.drain_creates()
            && let Some(err) = suspend_and_run(terminal, &cmd)?
        {
            app.status = Some(err);
        }
        if has_event && let Event::Key(key) = event::read()? {
            // Ctrl-C quits unconditionally, even when the modal is open
            // or the search bar has focus. Everything else routes to
            // those if they're up.
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
    transcript_root: PathBuf,
    cache_dir: Option<PathBuf>,
) -> RemoteDiscoveryResult {
    let ssh = match SshHost::connect(host_id.clone(), ssh_target) {
        Ok(h) => h,
        Err(e) => {
            return RemoteDiscoveryResult::Failed {
                host_id,
                error: e.to_string(),
            };
        }
    };
    let host: Arc<dyn Host> = Arc::new(ssh);
    let mut sessions = match discover(host.as_ref(), &transcript_root) {
        Ok(s) => s,
        Err(e) => {
            return RemoteDiscoveryResult::Failed {
                host_id,
                error: format!("discovery: {e}"),
            };
        }
    };
    // Pre-compute attention on the discovery thread so the row shows
    // real state on first render — the polling thread won't tick until
    // its first interval elapses.
    for session in &mut sessions {
        session.attention = derive_attention(host.as_ref(), &session.transcript_path);
    }
    // Snapshot to disk so the next startup paints this host's
    // sessions before its `ControlMaster` handshake completes.
    // Best-effort: an unwritable cache directory must not fail the
    // discovery, since the cache is strictly an optimisation.
    if let Some(dir) = cache_dir {
        let _ = cache::write_for_host(&dir, &host_id, &sessions);
    }
    RemoteDiscoveryResult::Ready {
        host_id,
        host,
        sessions,
        transcript_root,
    }
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

    // When the preview pane is open, split the list area into
    // list (55%) + preview (45%). 55/45 favours the list because
    // that's still the primary navigation surface; the preview is
    // peripheral context, not the main object of attention.
    if app.preview_open {
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
) -> String {
    if let Some(c) = creating {
        return format!(" creating worktree for {:?} in {}… ", c.task, c.repo_name);
    }
    if let Some(s) = status {
        return format!(" {s} ");
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
        let s = compose_footer(None, None, false, true, 0, &no_connect_errors());
        assert!(s.contains("return: prefix+s"), "got: {s}");
        assert!(!s.contains("prefix+d"), "got: {s}");
    }

    #[test]
    fn footer_keybind_line_advertises_preview_toggle() {
        let s = compose_footer(None, None, false, true, 0, &no_connect_errors());
        assert!(s.contains("p: preview"), "got: {s}");
    }

    #[test]
    fn footer_keybind_line_advertises_group_jumps() {
        let s = compose_footer(None, None, false, true, 0, &no_connect_errors());
        assert!(s.contains("J/K: project"), "got: {s}");
        assert!(s.contains("⌃j/⌃k: host"), "got: {s}");
    }

    #[test]
    fn footer_keybind_line_shows_outside_tmux_return_hint() {
        let s = compose_footer(None, None, false, false, 0, &no_connect_errors());
        assert!(s.contains("return: prefix+d"), "got: {s}");
        assert!(!s.contains("prefix+s"), "got: {s}");
    }

    #[test]
    fn footer_empty_catalog_omits_return_hint() {
        let s = compose_footer(None, None, true, true, 0, &no_connect_errors());
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
        );
        assert!(s.contains("creating worktree"), "got: {s}");
        assert!(s.contains("agent-mux"), "got: {s}");
        assert!(!s.contains("ignored"), "got: {s}");
    }

    #[test]
    fn footer_keybind_line_appends_connecting_suffix_when_hosts_pending() {
        let s = compose_footer(None, None, false, true, 2, &no_connect_errors());
        assert!(s.contains("return: prefix+s"), "got: {s}");
        assert!(s.contains("connecting to 2 host(s)"), "got: {s}");
    }

    #[test]
    fn footer_empty_catalog_swaps_no_sessions_for_connecting_when_hosts_pending() {
        // First impression matters: when the catalog is empty *and*
        // remote discovery is still in flight, "no sessions discovered"
        // would mis-imply we're done.
        let s = compose_footer(None, None, true, true, 1, &no_connect_errors());
        assert!(s.contains("connecting to 1 host(s)"), "got: {s}");
        assert!(!s.contains("no sessions"), "got: {s}");
    }

    #[test]
    fn footer_connecting_suffix_disappears_once_all_hosts_have_reported() {
        let s = compose_footer(None, None, false, true, 0, &no_connect_errors());
        assert!(!s.contains("connecting to"), "got: {s}");
    }

    #[test]
    fn footer_renders_connect_errors_as_sticky_line_when_no_status() {
        let errors = vec![(HostId("alpenglow".into()), "ssh exit 255".to_string())];
        let s = compose_footer(None, None, false, true, 0, &errors);
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
        let s = compose_footer(None, None, false, true, 0, &errors);
        assert!(s.contains("alpenglow"), "got: {s}");
        assert!(s.contains("gpu-1"), "got: {s}");
    }

    #[test]
    fn footer_transient_status_takes_precedence_over_connect_errors() {
        // A fresh action's feedback must not be drowned out by the
        // sticky connect-failure line.
        let errors = vec![(HostId("alpenglow".into()), "ssh exit 255".to_string())];
        let s = compose_footer(None, Some("opened terminal in /x"), false, true, 0, &errors);
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
}
