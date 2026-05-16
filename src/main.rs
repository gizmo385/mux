use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::Receiver;
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
use agent_mux::discovery::{claude_projects_dir, discover_local};
use agent_mux::session::{Attention, Host, Session};
use agent_mux::watcher::{AttentionUpdate, TranscriptWatcher};

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Sessions older than this are shown as Idle regardless of transcript
/// content. M0 default; will become configurable in M4.
const IDLE_THRESHOLD: Duration = Duration::from_secs(60 * 60);

/// Event-loop tick. Bounds the latency between an attention update arriving
/// in the channel and the dashboard re-rendering it.
const TICK: Duration = Duration::from_millis(100);

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
    _watcher: TranscriptWatcher,
    updates: Receiver<AttentionUpdate>,
    driver: TmuxDriver,
    status: Option<String>,
}

impl App {
    fn new() -> io::Result<Self> {
        let mut catalog = SessionCatalog::new();
        if let Some(root) = claude_projects_dir()
            && let Ok(sessions) = discover_local(&root)
        {
            catalog.replace_all(sessions);
        }

        let targets: Vec<_> = catalog
            .sessions()
            .iter()
            .map(|s| (s.id.clone(), s.transcript_path.clone()))
            .collect();
        let (watcher, updates) = TranscriptWatcher::start(targets).map_err(io::Error::other)?;

        let mut list_state = ListState::default();
        if !catalog.is_empty() {
            list_state.select(Some(0));
        }

        Ok(Self {
            catalog,
            list_state,
            home: dirs::home_dir(),
            _watcher: watcher,
            updates,
            driver: TmuxDriver::new(),
            status: None,
        })
    }

    fn drain_updates(&mut self) {
        while let Ok(update) = self.updates.try_recv() {
            self.catalog.update_attention(&update.id, update.attention);
        }
    }

    fn attach_selected(&mut self) -> Option<SuspendCommand> {
        let result = {
            let idx = self.list_state.selected()?;
            let session = self.catalog.sessions().get(idx)?;
            self.driver.attach(session)
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
            let idx = self.list_state.selected()?;
            let session = self.catalog.sessions().get(idx)?;
            let cwd = session.project_dir.clone();
            (self.driver.spawn_terminal(session), cwd)
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
        if self.catalog.is_empty() {
            return;
        }
        let i = self.list_state.selected().unwrap_or(0);
        let next = (i + 1) % self.catalog.len();
        self.list_state.select(Some(next));
    }

    fn prev(&mut self) {
        if self.catalog.is_empty() {
            return;
        }
        let i = self.list_state.selected().unwrap_or(0);
        let prev = if i == 0 {
            self.catalog.len() - 1
        } else {
            i - 1
        };
        self.list_state.select(Some(prev));
    }
}

fn run(terminal: &mut Tui, app: &mut App) -> io::Result<()> {
    loop {
        terminal.draw(|frame| draw(frame, app))?;
        let has_event = event::poll(TICK)?;
        app.drain_updates();
        if has_event && let Event::Key(key) = event::read()? {
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

enum Action {
    Quit,
    Next,
    Prev,
    Attach,
    SpawnTerminal,
}

fn action_for(key: KeyEvent) -> Option<Action> {
    let ctrl_c = key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl_c {
        return Some(Action::Quit);
    }
    match key.code {
        KeyCode::Char('q') => Some(Action::Quit),
        KeyCode::Down | KeyCode::Char('j') => Some(Action::Next),
        KeyCode::Up | KeyCode::Char('k') => Some(Action::Prev),
        KeyCode::Enter => Some(Action::Attach),
        KeyCode::Char('t') => Some(Action::SpawnTerminal),
        _ => None,
    }
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &mut App) {
    let layout = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(frame.area());

    let header = Paragraph::new(Line::from(Span::styled(
        " agent-mux ",
        Style::new().add_modifier(Modifier::BOLD),
    )));
    frame.render_widget(header, layout[0]);

    let title = format!(" sessions ({}) ", app.catalog.len());
    let items: Vec<ListItem<'_>> = app
        .catalog
        .sessions()
        .iter()
        .map(|s| ListItem::new(format_session_row(s, app.home.as_deref())))
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▌ ");
    frame.render_stateful_widget(list, layout[1], &mut app.list_state);

    let footer_text = match (&app.status, app.catalog.is_empty()) {
        (Some(s), _) => format!(" {s} "),
        (None, true) => " no sessions discovered · q: quit ".to_string(),
        (None, false) => " ↑/↓ or j/k: move · ⏎: attach · t: terminal · q: quit ".to_string(),
    };
    let footer = Paragraph::new(Line::from(Span::styled(
        footer_text,
        Style::new().add_modifier(Modifier::DIM),
    )));
    frame.render_widget(footer, layout[2]);
}

fn format_session_row(session: &Session, home: Option<&Path>) -> Line<'static> {
    let glyph = attention_glyph(effective_attention(session));
    let host = host_label(session.host);
    let project = display_path(&session.project_dir, home);
    let age = humanize_elapsed(session.last_activity);

    Line::from(vec![
        Span::raw(glyph),
        Span::raw(" "),
        Span::raw(project),
        Span::raw("  "),
        Span::styled(host, Style::new().add_modifier(Modifier::DIM)),
        Span::raw("  "),
        Span::styled(age, Style::new().add_modifier(Modifier::DIM)),
    ])
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

fn host_label(h: Host) -> &'static str {
    match h {
        Host::Local => "local",
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
