use std::collections::HashMap;
use std::io::{self, Stdout};
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
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use agent_mux::attachment::{AttachOutcome, AttachmentDriver, SuspendCommand, TmuxDriver};
use agent_mux::catalog::SessionCatalog;
use agent_mux::config::Config;
use agent_mux::dashboard::{
    DisplayRow, SearchMode, SearchOutcome, SearchState, build_display_rows,
    build_display_rows_filtered, first_session_index, matches_query, next_session_index,
    prev_session_index,
};
use agent_mux::discovery::{build_session, claude_projects_dir, discover};
use agent_mux::host::{Host, LocalHost, SshHost};
use agent_mux::new_session_modal::{KeyOutcome, NewSessionModal};
use agent_mux::repo::{Repo, RepoRegistry};
use agent_mux::session::{Attention, HostId, Session, SessionId};
use agent_mux::watcher::{REMOTE_POLL_INTERVAL, TranscriptWatcher, WatcherEvent, derive_attention};
use agent_mux::worktree::WorktreeManager;

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Sessions older than this are shown as Idle regardless of transcript
/// content. M0 default; will become configurable in M4.
const IDLE_THRESHOLD: Duration = Duration::from_secs(60 * 60);

/// Event-loop tick. Bounds the latency between an attention update arriving
/// in the channel and the dashboard re-rendering it.
const TICK: Duration = Duration::from_millis(100);

/// How long the Repo Registry's cached scan is allowed to age before the
/// next picker-open re-scans. Short enough that newly-cloned repos appear
/// without a restart; long enough that rapid open/close of the modal
/// doesn't repeat the depth-1 walk on every keystroke.
const REPO_REFRESH_TTL: Duration = Duration::from_secs(30);

fn main() -> io::Result<()> {
    let mut app = App::new()?;
    let mut terminal = setup_terminal()?;
    let result = run(&mut terminal, &mut app);
    restore_terminal(&mut terminal)?;
    result
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
        let local_host: Arc<dyn Host> = Arc::new(LocalHost::new());
        let projects_root = claude_projects_dir();
        let mut catalog = SessionCatalog::new();
        if let Some(root) = projects_root.as_ref()
            && let Ok(sessions) = discover(local_host.as_ref(), root)
        {
            catalog.replace_all(sessions);
        }

        let targets: Vec<_> = catalog
            .sessions()
            .iter()
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

        let config = Config::load().unwrap_or_default();
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
            std::thread::spawn(move || {
                let result = connect_and_discover(host_id.clone(), ssh_target, transcript_root);
                let _ = tx.send(result);
            });
        }
        // Drop the original Sender so the channel closes once every
        // background thread has sent its result and exited. The
        // per-thread clones above are the only live Senders.
        drop(remote_tx);

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
                    self.hosts.insert(host_id, Arc::clone(&host));
                    let first_insert = self.catalog.is_empty();
                    for session in sessions {
                        self.catalog.add(session);
                    }
                    if first_insert && !self.catalog.is_empty() {
                        let rows = self.current_rows();
                        self.list_state.select(first_session_index(&rows));
                    }
                    // Live polling: without this, remote attention stays
                    // frozen at the discovery reading.
                    self.watcher.start_polling_host(
                        host,
                        transcript_root,
                        poll_seed,
                        REMOTE_POLL_INTERVAL,
                    );
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
                    self.catalog.update_attention(&update.id, update.attention);
                }
                WatcherEvent::NewTranscript { host, path, mtime } => {
                    self.handle_new_transcript(&host, &path, mtime);
                }
            }
        }
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
                None => None,
            };
            if let Some(cmd) = pending
                && let Some(err) = suspend_and_run(terminal, &cmd)?
            {
                app.status = Some(err);
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
    transcript_root: PathBuf,
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
    Attach,
    SpawnTerminal,
    NewSession,
    OpenSearch,
}

fn action_for(key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char('q') => Some(Action::Quit),
        KeyCode::Down | KeyCode::Char('j') => Some(Action::Next),
        KeyCode::Up | KeyCode::Char('k') => Some(Action::Prev),
        KeyCode::Enter => Some(Action::Attach),
        KeyCode::Char('t') => Some(Action::SpawnTerminal),
        KeyCode::Char('n') => Some(Action::NewSession),
        KeyCode::Char('/') => Some(Action::OpenSearch),
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
            DisplayRow::SessionRow(i) => ListItem::new(format_session_row(&sessions_slice[*i])),
        })
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▌ ");
    frame.render_stateful_widget(list, layout[1], &mut app.list_state);

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
        " ↑/↓ or j/k: move · ⏎: attach · t: terminal · n: new · q: quit  ·  return: {return_hint}{suffix} "
    )
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

fn format_session_row(session: &Session) -> Line<'static> {
    let glyph = attention_glyph(effective_attention(session));
    let age = humanize_elapsed(session.last_activity);
    let dim = Style::new().add_modifier(Modifier::DIM);

    // Indented two levels under the project header. Project text is
    // no longer per-row (the header carries it); for title-less
    // sessions, fall back to a short session-id suffix so two
    // unnamed sessions in the same project remain distinguishable.
    let mut spans = vec![Span::raw("    "), Span::raw(glyph), Span::raw(" ")];
    if let Some(title) = &session.title {
        spans.push(Span::raw(title.clone()));
    } else {
        let id = &session.id.0;
        let suffix = if id.len() > 6 {
            &id[id.len() - 6..]
        } else {
            id.as_str()
        };
        spans.push(Span::styled(format!("({suffix})"), dim));
    }
    spans.push(Span::raw("  "));
    spans.push(Span::styled(age, dim));
    Line::from(spans)
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
