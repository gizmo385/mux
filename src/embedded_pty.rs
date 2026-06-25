//! Embedded terminal widget. Owns a pseudoterminal and a vt100 parser;
//! lets ratatui render its current screen into a `Rect`.
//!
//! Phase 1 of the embedded-PTY dashboard work (see `TODO.md`'s
//! "embedded-PTY dashboard / Shape B pivot" entry). This module is
//! self-contained — nothing else in the crate calls into it yet. Later
//! phases plug it into the `AttachmentDriver` and the main event loop.
//!
//! Design at a glance:
//! - Spawn `argv` into a pty sized to `rows`×`cols`. The pty becomes the
//!   child's controlling tty; agent-mux's own tty is untouched.
//! - A background reader thread blocks on the pty's master, feeds bytes
//!   into a shared `vt100::Parser`, and sends a wake-up event on each
//!   read. After EOF it reaps the child and forwards the `ExitStatus`.
//! - The screen lives behind `Arc<RwLock<vt100::Parser>>` so the render
//!   path holds the lock for a microsecond at draw time and the reader
//!   thread holds it for a microsecond per chunk — no bytes ride the
//!   event channel, which exists only to wake the main loop.

use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::{Arc, RwLock, mpsc};
use std::thread;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use portable_pty::{
    ChildKiller, CommandBuilder, ExitStatus, MasterPty, NativePtySystem, PtySize, PtySystem,
};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use tui_term::widget::PseudoTerminal;

/// Re-paint the theme background onto every cell in `area` whose
/// background is still `Color::Reset` — the host-terminal default that
/// tui-term leaves behind for cells the inner program didn't colour
/// (see [`EmbeddedPty::render`]). Cells the program coloured itself have
/// a concrete bg and are skipped, so only the gaps pick up the theme.
fn paint_default_bg(buf: &mut Buffer, area: Rect, bg: Color) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buf[(x, y)];
            if cell.bg == Color::Reset {
                cell.bg = bg;
            }
        }
    }
}

/// One-way signal from the reader thread back to the main loop.
///
/// `Output` carries no payload — it's a redraw hint. The parser's
/// screen has already been updated by the time the event arrives. The
/// channel exists only to wake `event::poll` so the main loop can
/// re-render without waiting for its tick.
///
/// `Exited` carries the child's final status. Phase 3 uses it to drop
/// the embedded PTY and transition focus back to the sidebar; Phase 1
/// only the tests observe it.
#[derive(Debug)]
pub enum PtyEvent {
    Output,
    Exited(ExitStatus),
}

/// One running pseudoterminal.
///
/// Drop semantics: dropping `EmbeddedPty` sends SIGHUP to the child via
/// the `ChildKiller` cloned at spawn time, sleeps briefly so the SIGHUP
/// propagates and the slave fd closes, then drops the master + writer.
///
/// The SIGHUP-first step is load-bearing for one specific case:
/// `portable_pty 0.9`'s `UnixMasterWriter::drop` reads the master's
/// termios `VEOF` byte and writes `\n` followed by that byte (default
/// `^D`) to the master before closing the writer's fd. The intent is
/// to cleanly close stdin for a generic child. But agent-mux hosts
/// `tmux attach` clients in this PTY — those forward stdin bytes to
/// the inner tmux session's shell, which interprets `^D` as EOF and
/// exits. The exiting shell terminates its window, the window's
/// session closes (last window gone), and the entire tmux session is
/// destroyed. Pre-emptively SIGHUP'ing the tmux client so the slave is
/// already closed by the time the writer's destructor's `write_all`
/// runs makes the EOT-write a no-op against a closed pipe.
///
/// The reader thread is still detached — a child that ignores SIGHUP
/// must not stall app shutdown — but the short post-SIGHUP sleep gives
/// well-behaved children (which `tmux attach` is) a window to exit
/// cleanly before we drop the fds out from under them.
pub struct EmbeddedPty {
    parser: Arc<RwLock<vt100::Parser>>,
    /// Held to keep the pty alive (drop closes it) and to service
    /// `resize`. The reader and writer live on handles cloned from the
    /// master at spawn time, so we never need to lock the master itself.
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    events: mpsc::Receiver<PtyEvent>,
    /// Cloned from the child at spawn time so the main thread can
    /// SIGHUP the child on drop without owning the `Child` (which
    /// lives in the reader thread, blocked on `child.wait()`).
    killer: Box<dyn ChildKiller + Send + Sync>,
}

impl Drop for EmbeddedPty {
    fn drop(&mut self) {
        // SIGHUP the child first — see the struct doc-comment for the
        // tmux-attach-EOT explanation. Failures here are best-effort:
        // the child may already be dead, or the kill syscall may fail
        // for kernel reasons. Either way we proceed with field drops.
        let _ = self.killer.kill();
        // Brief sleep so SIGHUP can propagate and the child closes its
        // slave fd before portable-pty's `UnixMasterWriter::drop`
        // writes `\n + EOT` to the master. 30ms is empirically enough
        // on a loaded macOS dev box (validated via /tmp/agent-mux-repro
        // 2026-05-29). Bounded sleep — even if SIGHUP delivery is
        // delayed past 30ms the worst case is the original bug
        // returning, not a stuck shutdown.
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
}

impl EmbeddedPty {
    /// Spawn `argv` inside a pseudoterminal sized to `rows`×`cols`. The
    /// child runs with the pty as its controlling tty, detached from
    /// agent-mux's own terminal.
    ///
    /// `cwd` is the child's working directory; `None` inherits the
    /// current process's cwd.
    ///
    /// # Errors
    /// Returns `io::ErrorKind::InvalidInput` for an empty `argv` or
    /// zero-sized grid; other errors propagate from `portable_pty`
    /// (pty allocation, child spawn, handle acquisition).
    pub fn spawn(argv: &[String], cwd: Option<&Path>, rows: u16, cols: u16) -> io::Result<Self> {
        if argv.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "argv is empty"));
        }
        if rows == 0 || cols == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "rows and cols must be > 0",
            ));
        }

        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(map_pty_err)?;

        let mut cmd = CommandBuilder::new(&argv[0]);
        for arg in &argv[1..] {
            cmd.arg(arg);
        }
        if let Some(cwd) = cwd {
            cmd.cwd(cwd);
        }

        let mut child = pair.slave.spawn_command(cmd).map_err(map_pty_err)?;
        // Close the slave end on the parent side. The child's copy
        // remains open via its fd table; this drop only releases our
        // reference. Without it, our reader would never see EOF on the
        // master after the child exits.
        drop(pair.slave);

        // Clone a signaller off the child *before* moving the child
        // into the reader thread. The signaller wraps the child's
        // PID and lets the `EmbeddedPty::drop` SIGHUP the child even
        // while the reader thread holds `child.wait()`.
        let killer = child.clone_killer();

        let mut reader = pair.master.try_clone_reader().map_err(map_pty_err)?;
        let writer = pair.master.take_writer().map_err(map_pty_err)?;
        let master = pair.master;

        let parser = Arc::new(RwLock::new(vt100::Parser::new(rows, cols, 0)));
        let (tx, rx) = mpsc::channel();

        {
            let parser = parser.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if let Ok(mut p) = parser.write() {
                                p.process(&buf[..n]);
                            }
                            if tx.send(PtyEvent::Output).is_err() {
                                // Main loop has dropped its receiver —
                                // EmbeddedPty is going away. Stop early
                                // so we don't waste cycles draining a
                                // pty nobody is rendering.
                                return;
                            }
                        }
                    }
                }
                // The pty closed (master dropped or child finished
                // writing). Reap the child so its `ExitStatus` is
                // available; ignore a failed wait — the EOF signal is
                // enough for the caller to drop the pty.
                if let Ok(status) = child.wait() {
                    let _ = tx.send(PtyEvent::Exited(status));
                }
            });
        }

        Ok(Self {
            parser,
            master,
            writer,
            events: rx,
            killer,
        })
    }

    /// Drain one event from the reader thread, non-blocking. Returns
    /// `None` when the queue is empty (or the reader has exited).
    #[must_use]
    pub fn poll_event(&self) -> Option<PtyEvent> {
        self.events.try_recv().ok()
    }

    /// Forward bytes to the child's stdin via the pty master. Phase 3
    /// calls this from the key handler when the embedded terminal has
    /// focus.
    ///
    /// # Errors
    /// Propagates `io::Error` from the underlying writer. The caller
    /// decides whether a write failure is fatal; typically it isn't —
    /// the next render will reflect whatever state the child is in.
    pub fn write_input(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    /// Resize the pty (delivers SIGWINCH to the child via the kernel)
    /// and the vt100 parser to match. Called when the frame's allocated
    /// area changes.
    ///
    /// # Errors
    /// Returns `io::ErrorKind::InvalidInput` for a zero-sized grid;
    /// other errors propagate from `portable_pty::MasterPty::resize`.
    pub fn resize(&mut self, rows: u16, cols: u16) -> io::Result<()> {
        if rows == 0 || cols == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "rows and cols must be > 0",
            ));
        }
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(map_pty_err)?;
        if let Ok(mut p) = self.parser.write() {
            p.screen_mut().set_size(rows, cols);
        }
        Ok(())
    }

    /// Render the parser's current screen into `area` of `frame`. Pure
    /// ratatui — the only synchronisation is a brief parser read-lock.
    ///
    /// `base_style` is the widget's default style — its background paints
    /// the terminal cells the inner program leaves at terminal-default,
    /// so the embedded pane harmonises with agent-mux's themed frame
    /// (`[theme] background`). Cells the program colours itself keep their
    /// own style; this only fills the gaps. Purely a render-layer effect —
    /// nothing is written to the PTY or tmux.
    ///
    /// `PseudoTerminal::style()` is honoured only as documentation here:
    /// tui-term 0.3.4 never reads the widget style in its render path, and
    /// it maps every vt100 default cell to [`Color::Reset`]
    /// (`Style::reset().bg(Reset)`), so the widget alone leaves default
    /// cells at the host terminal's background. We therefore re-paint the
    /// gap ourselves: after the widget renders, any cell still at `Reset`
    /// background takes the theme background. Cells the program coloured
    /// keep their own bg untouched.
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect, base_style: Style) {
        if let Ok(p) = self.parser.read() {
            let widget = PseudoTerminal::new(p.screen()).style(base_style);
            frame.render_widget(widget, area);
        }
        // tui-term ignores the widget style; fill default-bg cells here so
        // a themed `[theme] background` actually reaches the session pane.
        if let Some(bg) = base_style.bg {
            paint_default_bg(frame.buffer_mut(), area, bg);
        }
    }

    /// Flat text dump of the parser's current screen — cells joined per
    /// row, rows joined by `\n`. Style information is dropped. Useful
    /// for tests asserting "the child wrote X" without standing up a
    /// ratatui frame, and as a debugging aid.
    #[must_use]
    pub fn screen_text(&self) -> String {
        self.parser
            .read()
            .map_or_else(|_| String::new(), |p| p.screen().contents())
    }

    /// Current `(rows, cols)` of the parser's screen. Returns `(0, 0)`
    /// if the parser lock is poisoned (unreachable in normal flow —
    /// vt100's `Parser::process` doesn't panic). Used by the resize
    /// cascade test to verify dimensions actually changed.
    #[must_use]
    pub fn current_size(&self) -> (u16, u16) {
        self.parser.read().map_or((0, 0), |p| p.screen().size())
    }
}

fn map_pty_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(e.to_string())
}

/// Encode a crossterm `KeyEvent` into the byte sequence a tty-attached
/// program would receive. Conservative coverage — printable chars
/// (with Ctrl/Alt modifiers), the common control codes (Enter, Tab,
/// Backspace, Esc), arrow / navigation keys, and F1–F4. Exotic keys
/// (F5+, kitty-keyboard extensions) yield an empty `Vec`; dogfooding
/// will tell us which ones need adding.
///
/// Ctrl + ASCII letter follows the standard "byte XOR 0x40" rule
/// (Ctrl-a → 0x01, Ctrl-c → 0x03, …). Ctrl + non-letter is best-effort:
/// terminals disagree on these and the chord layer above us will
/// usually intercept the interesting ones (Ctrl-space, Ctrl-^).
///
/// Alt + key prepends `0x1B` (the de-facto Esc-prefix convention used
/// by xterm, gnome-terminal, and friends). Alt + an unsupported key
/// returns an empty `Vec` rather than a stray Esc.
#[must_use]
pub fn encode_key_for_pty(key: &KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    let mut payload = Vec::with_capacity(4);
    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let upper = c.to_ascii_uppercase();
                if upper.is_ascii_alphabetic() {
                    // Ctrl-A=0x01 … Ctrl-Z=0x1A
                    payload.push((upper as u8) - 0x40);
                } else {
                    // Non-letter Ctrl: best-effort literal byte. The
                    // interesting cases (Ctrl-[, Ctrl-]) happen to map
                    // correctly under this branch on ASCII input.
                    let mut buf = [0u8; 4];
                    payload.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
            } else {
                let mut buf = [0u8; 4];
                payload.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        KeyCode::Enter => payload.push(0x0D),
        KeyCode::Backspace => payload.push(0x7F),
        KeyCode::Tab => payload.push(0x09),
        KeyCode::BackTab => payload.extend_from_slice(b"\x1b[Z"),
        KeyCode::Esc => payload.push(0x1B),
        KeyCode::Up => payload.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => payload.extend_from_slice(b"\x1b[B"),
        KeyCode::Right => payload.extend_from_slice(b"\x1b[C"),
        KeyCode::Left => payload.extend_from_slice(b"\x1b[D"),
        KeyCode::Home => payload.extend_from_slice(b"\x1b[H"),
        KeyCode::End => payload.extend_from_slice(b"\x1b[F"),
        KeyCode::PageUp => payload.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => payload.extend_from_slice(b"\x1b[6~"),
        KeyCode::Delete => payload.extend_from_slice(b"\x1b[3~"),
        KeyCode::Insert => payload.extend_from_slice(b"\x1b[2~"),
        KeyCode::F(1) => payload.extend_from_slice(b"\x1bOP"),
        KeyCode::F(2) => payload.extend_from_slice(b"\x1bOQ"),
        KeyCode::F(3) => payload.extend_from_slice(b"\x1bOR"),
        KeyCode::F(4) => payload.extend_from_slice(b"\x1bOS"),
        _ => return Vec::new(),
    }

    if alt && !payload.is_empty() {
        let mut out = Vec::with_capacity(payload.len() + 1);
        out.push(0x1B);
        out.extend_from_slice(&payload);
        return out;
    }
    payload
}

/// Encode a crossterm [`MouseEvent`] into an SGR-mode (xterm 1006)
/// mouse report — `\x1b[<{button};{col};{row}M` (press / scroll /
/// motion) or `…m` (release). SGR is the modern mouse protocol every
/// recent terminal speaks, and what tmux / `claude` parse on stdin.
///
/// `pty_col` and `pty_row` are PTY-relative, 1-based coordinates. The
/// caller is responsible for the terminal-to-PTY translation (subtract
/// the embedded pane's origin, then add 1 to match SGR's base).
///
/// Returns `None` for unhandled kinds (`ScrollLeft` / `ScrollRight`,
/// `Moved` without a held button) — those are rare and would need
/// terminal-specific encoding; emit nothing rather than guessing.
#[must_use]
pub fn encode_mouse_event(ev: &MouseEvent, pty_col: u16, pty_row: u16) -> Option<Vec<u8>> {
    let modifier_bits = mouse_modifier_bits(*ev);
    let (button_code, kind_byte) = match ev.kind {
        MouseEventKind::Down(b) => (button_code(b), b'M'),
        MouseEventKind::Up(b) => (button_code(b), b'm'),
        MouseEventKind::Drag(b) => (button_code(b) + 32, b'M'),
        MouseEventKind::ScrollUp => (64, b'M'),
        MouseEventKind::ScrollDown => (65, b'M'),
        // Moved-without-button + horizontal scroll: deliberate gaps —
        // see the function-level comment.
        MouseEventKind::Moved | MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {
            return None;
        }
    };
    let code = button_code | modifier_bits;
    Some(format!("\x1b[<{code};{pty_col};{pty_row}{}", kind_byte as char).into_bytes())
}

fn button_code(b: MouseButton) -> u32 {
    match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

fn mouse_modifier_bits(ev: MouseEvent) -> u32 {
    let mut bits = 0u32;
    if ev.modifiers.contains(KeyModifiers::SHIFT) {
        bits |= 4;
    }
    if ev.modifiers.contains(KeyModifiers::ALT) {
        bits |= 8;
    }
    if ev.modifiers.contains(KeyModifiers::CONTROL) {
        bits |= 16;
    }
    bits
}

/// Wrap pasted text in bracketed-paste markers (`\e[200~ … \e[201~`),
/// the protocol the embedded child opts into via `DECSET 2004`. Any
/// `\e[201~` sequence inside the paste is stripped first — a paste
/// containing that close-bracket would prematurely terminate the
/// paste mode and inject the rest as raw keystrokes, which is a
/// real security concern (think pasting from a maliciously-crafted
/// URL into a sudo prompt).
#[must_use]
pub fn encode_paste(text: &str) -> Vec<u8> {
    let sanitized = text.replace("\x1b[201~", "");
    let mut out = Vec::with_capacity(sanitized.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(sanitized.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

/// Manual `Debug` because `MasterPty` and the boxed writer don't
/// implement it. We don't need the field contents in a diagnostic — the
/// existence of the struct is enough for `assert_*!`-style failure
/// messages.
impl std::fmt::Debug for EmbeddedPty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddedPty").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::{Duration, Instant};

    #[test]
    fn paint_default_bg_fills_reset_cells_and_spares_coloured_ones() {
        // Reset-bg cells are the host-terminal default tui-term leaves
        // behind; they should take the theme background. A cell the inner
        // program coloured (concrete bg) must be left untouched.
        let area = Rect::new(0, 0, 2, 1);
        let mut buf = Buffer::empty(area);
        buf[(0, 0)].bg = Color::Reset; // default cell
        buf[(1, 0)].bg = Color::Blue; // program-coloured cell

        paint_default_bg(&mut buf, area, Color::Rgb(0x2e, 0x34, 0x40));

        assert_eq!(buf[(0, 0)].bg, Color::Rgb(0x2e, 0x34, 0x40));
        assert_eq!(buf[(1, 0)].bg, Color::Blue);
    }

    #[test]
    fn render_paints_theme_background_onto_session_pane() {
        // End-to-end guard for the tui-term-ignores-.style() bug: a child
        // that writes plain text leaves most cells at terminal-default, and
        // a themed `base_style.bg` must reach them through `render`.
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "printf hi".to_string(),
        ];
        let pty = EmbeddedPty::spawn(&argv, None, 24, 80).unwrap();
        assert!(wait_for_screen(
            &pty,
            |s| s.contains("hi"),
            Duration::from_secs(5)
        ));

        let bg = Color::Rgb(0x2e, 0x34, 0x40);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| pty.render(f, f.area(), Style::new().bg(bg)))
            .unwrap();

        // A blank cell beyond the "hi" the child wrote is terminal-default
        // and must have picked up the theme bg.
        assert_eq!(terminal.backend().buffer()[(40, 12)].bg, bg);
    }

    /// Poll `pred` against the screen text until it returns true or
    /// `timeout` elapses. Drains events between polls so the channel
    /// doesn't back up.
    fn wait_for_screen(pty: &EmbeddedPty, pred: impl Fn(&str) -> bool, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            while pty.poll_event().is_some() {}
            if pred(&pty.screen_text()) {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// Drain events until an `Exited` arrives, or return `None` on
    /// timeout.
    fn wait_for_exit(pty: &EmbeddedPty, timeout: Duration) -> Option<ExitStatus> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            while let Some(ev) = pty.poll_event() {
                if let PtyEvent::Exited(status) = ev {
                    return Some(status);
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        None
    }

    #[test]
    fn spawn_rejects_empty_argv() {
        let err = EmbeddedPty::spawn(&[], None, 24, 80).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn spawn_rejects_zero_dimensions() {
        let argv = vec!["/bin/true".to_string()];
        assert_eq!(
            EmbeddedPty::spawn(&argv, None, 0, 80).unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
        );
        assert_eq!(
            EmbeddedPty::spawn(&argv, None, 24, 0).unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
        );
    }

    #[test]
    fn spawn_runs_command_and_writes_output_into_parser() {
        // `printf hello` finishes immediately. The reader thread feeds
        // its bytes through the vt100 parser before — or just after —
        // the Exited event lands; `wait_for_screen` rides out the
        // scheduling jitter.
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "printf hello".to_string(),
        ];
        let pty = EmbeddedPty::spawn(&argv, None, 24, 80).unwrap();
        assert!(
            wait_for_screen(&pty, |s| s.contains("hello"), Duration::from_secs(3)),
            "screen was {:?}",
            pty.screen_text()
        );
    }

    #[test]
    fn spawn_propagates_zero_exit_status() {
        // `/bin/sh -c 'exit 0'` rather than `/bin/true`: sandboxed test
        // environments routinely lack `/bin/true` (and `/bin/false`),
        // but `/bin/sh` is the same binary the rest of this file's
        // tests already rely on. Same observable behaviour, broader
        // portability.
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "exit 0".to_string(),
        ];
        let pty = EmbeddedPty::spawn(&argv, None, 24, 80).unwrap();
        let status = wait_for_exit(&pty, Duration::from_secs(3)).expect("Exited event");
        assert!(status.success(), "expected zero exit, got {status:?}");
    }

    #[test]
    fn spawn_propagates_nonzero_exit_status() {
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "exit 1".to_string(),
        ];
        let pty = EmbeddedPty::spawn(&argv, None, 24, 80).unwrap();
        let status = wait_for_exit(&pty, Duration::from_secs(3)).expect("Exited event");
        assert!(!status.success(), "expected non-zero exit, got {status:?}");
    }

    #[test]
    fn resize_after_spawn_does_not_panic_and_drop_kills_child() {
        // `sleep 30` outlives the test; the drop at the end is what we
        // expect to terminate it. No panic, no hang — that's the assert.
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 30".to_string(),
        ];
        let mut pty = EmbeddedPty::spawn(&argv, None, 24, 80).unwrap();
        pty.resize(40, 120).unwrap();
        pty.resize(10, 30).unwrap();
    }

    /// Regression: hosting `tmux attach -t <session>` in an
    /// `EmbeddedPty` and dropping it must NOT terminate the underlying
    /// tmux session. The bug (2026-05-29 dogfood, surfaced by the t-in-
    /// tools ship): `portable_pty 0.9`'s `UnixMasterWriter::drop`
    /// writes `\n` + termios `VEOF` (`^D`) to the master before
    /// closing the writer's fd. `tmux attach` faithfully forwards
    /// those bytes to the inner session's shell, which interprets
    /// `^D` as EOF and exits — terminating the window, the session,
    /// and (if it was the last session) the tmux server. The fix:
    /// `EmbeddedPty::drop` SIGHUPs the child first via the
    /// `ChildKiller` cloned at spawn time, so by the time the writer's
    /// destructor runs the slave is already closed and the EOT-write
    /// falls into a broken pipe.
    #[test]
    fn drop_preserves_tmux_session_when_hosting_an_attach_client() {
        // Sandboxed tmux server via `-L <socket>` so we don't touch
        // the user's default server (a `tmux kill-server` here would
        // wipe every session the developer had open). Unique socket
        // name per test run to avoid collisions if the test is invoked
        // concurrently.
        let socket = format!("agent-mux-test-{}", std::process::id());
        let _ = std::process::Command::new("tmux")
            .args(["-L", &socket, "kill-server"])
            .output();

        // Create a detached session running /bin/sh (which exits on
        // EOF, mirroring zsh/bash behaviour for the failure mode the
        // fix targets).
        let out = std::process::Command::new("tmux")
            .args([
                "-L",
                &socket,
                "new-session",
                "-d",
                "-P",
                "-F",
                "#{session_name}",
                "-c",
                "/tmp",
                "/bin/sh",
            ])
            .output()
            .expect("tmux new-session");
        if !out.status.success() {
            // tmux may not be installed in the test environment.
            // Skip rather than fail — every other test in this module
            // is independent of tmux.
            eprintln!("skipping drop_preserves_tmux_session: tmux unavailable");
            return;
        }
        let name = String::from_utf8_lossy(&out.stdout).trim().to_string();

        // Sanity: session is alive immediately after creation.
        let alive_before = std::process::Command::new("tmux")
            .args(["-L", &socket, "has-session", "-t", &name])
            .status()
            .expect("has-session")
            .success();
        assert!(alive_before, "session should be alive after new-session -d");

        // Host `tmux attach -t <name>` in an EmbeddedPty, hold briefly
        // so the attach actually connects (tmux's stdout flush has a
        // measurable lag on macOS), then drop.
        {
            let argv = vec![
                "tmux".to_string(),
                "-L".to_string(),
                socket.clone(),
                "attach".to_string(),
                "-t".to_string(),
                name.clone(),
            ];
            let _pty = EmbeddedPty::spawn(&argv, None, 24, 80).expect("attach");
            thread::sleep(Duration::from_millis(200));
            // _pty drops at end of scope. Drop semantics (SIGHUP + brief
            // settle) should leave the session alive.
        }

        // Give the SIGHUP and slave-close another moment to settle on
        // a loaded CI box, then assert the session is still there.
        thread::sleep(Duration::from_millis(100));
        let alive_after = std::process::Command::new("tmux")
            .args(["-L", &socket, "has-session", "-t", &name])
            .status()
            .expect("has-session")
            .success();

        // Cleanup the sandbox server BEFORE the assert so a failed
        // assertion doesn't leave a sandbox socket behind.
        let _ = std::process::Command::new("tmux")
            .args(["-L", &socket, "kill-server"])
            .output();

        assert!(
            alive_after,
            "tmux session must survive EmbeddedPty drop — \
             portable-pty's UnixMasterWriter::drop sends \\n+EOT to \
             the master, which tmux attach forwards as user input to \
             the inner shell; if EmbeddedPty's drop doesn't SIGHUP \
             the child first the shell exits and the session dies"
        );
    }

    #[test]
    fn resize_updates_parser_screen_size() {
        // Pins the contract that `resize` propagates the new
        // dimensions to the vt100 parser, not just the pty master.
        // Without this, the rendered grid would stay stuck at the
        // spawn size even after the user resized their terminal.
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 30".to_string(),
        ];
        let mut pty = EmbeddedPty::spawn(&argv, None, 24, 80).unwrap();
        assert_eq!(pty.current_size(), (24, 80));
        pty.resize(40, 120).unwrap();
        assert_eq!(pty.current_size(), (40, 120));
        pty.resize(10, 30).unwrap();
        assert_eq!(pty.current_size(), (10, 30));
    }

    #[test]
    fn resize_rejects_zero_dimensions() {
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 30".to_string(),
        ];
        let mut pty = EmbeddedPty::spawn(&argv, None, 24, 80).unwrap();
        assert_eq!(
            pty.resize(0, 80).unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
        );
        assert_eq!(
            pty.resize(24, 0).unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
        );
    }

    // ---- encode_key_for_pty ----

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn key_mod(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn encode_printable_char_passes_through_as_utf8() {
        assert_eq!(encode_key_for_pty(&key(KeyCode::Char('a'))), b"a");
        assert_eq!(encode_key_for_pty(&key(KeyCode::Char('Z'))), b"Z");
        // Multi-byte UTF-8 char survives intact.
        assert_eq!(encode_key_for_pty(&key(KeyCode::Char('é'))), "é".as_bytes());
    }

    #[test]
    fn encode_ctrl_letter_yields_xor_0x40_control_byte() {
        assert_eq!(
            encode_key_for_pty(&key_mod(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            vec![0x01],
        );
        assert_eq!(
            encode_key_for_pty(&key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            vec![0x03],
        );
        assert_eq!(
            encode_key_for_pty(&key_mod(KeyCode::Char('z'), KeyModifiers::CONTROL)),
            vec![0x1A],
        );
    }

    #[test]
    fn encode_alt_letter_prepends_escape_byte() {
        // xterm-style Alt prefix: ESC + key. Same convention every
        // common terminal uses for Meta on a no-meta-key keyboard.
        assert_eq!(
            encode_key_for_pty(&key_mod(KeyCode::Char('x'), KeyModifiers::ALT)),
            vec![0x1B, b'x'],
        );
    }

    #[test]
    fn encode_ctrl_alt_letter_prepends_escape_to_control_byte() {
        // Meta-Ctrl-C used by some apps. Result: ESC followed by the
        // control byte (0x03).
        assert_eq!(
            encode_key_for_pty(&key_mod(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            )),
            vec![0x1B, 0x03],
        );
    }

    #[test]
    fn encode_named_control_keys() {
        assert_eq!(encode_key_for_pty(&key(KeyCode::Enter)), vec![0x0D]);
        assert_eq!(encode_key_for_pty(&key(KeyCode::Backspace)), vec![0x7F]);
        assert_eq!(encode_key_for_pty(&key(KeyCode::Tab)), vec![0x09]);
        assert_eq!(encode_key_for_pty(&key(KeyCode::Esc)), vec![0x1B]);
    }

    #[test]
    fn encode_arrow_keys_to_csi_sequences() {
        assert_eq!(encode_key_for_pty(&key(KeyCode::Up)), b"\x1b[A");
        assert_eq!(encode_key_for_pty(&key(KeyCode::Down)), b"\x1b[B");
        assert_eq!(encode_key_for_pty(&key(KeyCode::Right)), b"\x1b[C");
        assert_eq!(encode_key_for_pty(&key(KeyCode::Left)), b"\x1b[D");
    }

    #[test]
    fn encode_navigation_keys_to_csi_sequences() {
        assert_eq!(encode_key_for_pty(&key(KeyCode::Home)), b"\x1b[H");
        assert_eq!(encode_key_for_pty(&key(KeyCode::End)), b"\x1b[F");
        assert_eq!(encode_key_for_pty(&key(KeyCode::PageUp)), b"\x1b[5~");
        assert_eq!(encode_key_for_pty(&key(KeyCode::PageDown)), b"\x1b[6~");
        assert_eq!(encode_key_for_pty(&key(KeyCode::Delete)), b"\x1b[3~");
        assert_eq!(encode_key_for_pty(&key(KeyCode::Insert)), b"\x1b[2~");
    }

    #[test]
    fn encode_function_keys_f1_through_f4_use_ss3_sequences() {
        // SS3 (\x1bO) prefix is what xterm uses for F1–F4 in normal
        // keypad mode. F5+ uses CSI \x1b[N~ — added when dogfooding
        // asks for them.
        assert_eq!(encode_key_for_pty(&key(KeyCode::F(1))), b"\x1bOP");
        assert_eq!(encode_key_for_pty(&key(KeyCode::F(2))), b"\x1bOQ");
        assert_eq!(encode_key_for_pty(&key(KeyCode::F(3))), b"\x1bOR");
        assert_eq!(encode_key_for_pty(&key(KeyCode::F(4))), b"\x1bOS");
    }

    #[test]
    fn encode_unsupported_key_yields_empty_vec() {
        // F5 is not in our table; the user sees nothing happen rather
        // than a corrupt byte sequence reaching the PTY.
        assert!(encode_key_for_pty(&key(KeyCode::F(5))).is_empty());
    }

    #[test]
    fn encode_backtab_emits_csi_z() {
        // Shift-Tab. Distinct from regular Tab so terminals can move
        // backwards through focusable elements.
        assert_eq!(encode_key_for_pty(&key(KeyCode::BackTab)), b"\x1b[Z");
    }

    // ---- encode_mouse_event ----

    fn mouse(kind: MouseEventKind, modifiers: KeyModifiers) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers,
        }
    }

    #[test]
    fn encode_mouse_left_down_uses_sgr_press_form() {
        let ev = mouse(
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::empty(),
        );
        assert_eq!(encode_mouse_event(&ev, 10, 5).unwrap(), b"\x1b[<0;10;5M",);
    }

    #[test]
    fn encode_mouse_left_up_uses_lowercase_terminator() {
        // SGR distinguishes press (M) from release (m) by terminator
        // case. The terminal program needs both to track drag state.
        let ev = mouse(MouseEventKind::Up(MouseButton::Left), KeyModifiers::empty());
        assert_eq!(encode_mouse_event(&ev, 10, 5).unwrap(), b"\x1b[<0;10;5m",);
    }

    #[test]
    fn encode_mouse_right_down_uses_button_code_2() {
        let ev = mouse(
            MouseEventKind::Down(MouseButton::Right),
            KeyModifiers::empty(),
        );
        assert_eq!(encode_mouse_event(&ev, 1, 1).unwrap(), b"\x1b[<2;1;1M",);
    }

    #[test]
    fn encode_mouse_scroll_up_uses_button_code_64() {
        let ev = mouse(MouseEventKind::ScrollUp, KeyModifiers::empty());
        assert_eq!(encode_mouse_event(&ev, 12, 8).unwrap(), b"\x1b[<64;12;8M",);
    }

    #[test]
    fn encode_mouse_scroll_down_uses_button_code_65() {
        let ev = mouse(MouseEventKind::ScrollDown, KeyModifiers::empty());
        assert_eq!(encode_mouse_event(&ev, 12, 8).unwrap(), b"\x1b[<65;12;8M",);
    }

    #[test]
    fn encode_mouse_drag_adds_motion_bit_32() {
        let ev = mouse(
            MouseEventKind::Drag(MouseButton::Left),
            KeyModifiers::empty(),
        );
        assert_eq!(
            encode_mouse_event(&ev, 5, 5).unwrap(),
            // Left (0) + drag (32) = 32
            b"\x1b[<32;5;5M",
        );
    }

    #[test]
    fn encode_mouse_modifiers_add_to_button_code() {
        // SGR encodes Shift=4, Alt=8, Ctrl=16 on top of the base
        // button code. Ctrl-Shift-LeftClick at (1,1) yields
        // (0 | 4 | 16) = 20.
        let ev = mouse(
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert_eq!(encode_mouse_event(&ev, 1, 1).unwrap(), b"\x1b[<20;1;1M",);
    }

    #[test]
    fn encode_mouse_moved_without_button_returns_none() {
        // Motion-without-button events would need terminal-specific
        // encoding; emit nothing rather than guessing wrong.
        let ev = mouse(MouseEventKind::Moved, KeyModifiers::empty());
        assert!(encode_mouse_event(&ev, 10, 5).is_none());
    }

    // ---- encode_paste ----

    #[test]
    fn encode_paste_wraps_text_in_bracketed_paste_markers() {
        let out = encode_paste("hello world");
        assert_eq!(out, b"\x1b[200~hello world\x1b[201~");
    }

    #[test]
    fn encode_paste_strips_embedded_close_marker_to_prevent_injection() {
        // A paste containing the close-bracket could otherwise
        // terminate the paste prematurely and inject the trailing
        // bytes as raw keystrokes — a real concern when pasting from
        // a hostile source (e.g. a maliciously-crafted snippet).
        let injected = "innocent\x1b[201~rm -rf /";
        let out = encode_paste(injected);
        let expected = b"\x1b[200~innocentrm -rf /\x1b[201~";
        assert_eq!(out, expected);
    }

    #[test]
    fn encode_paste_handles_empty_text() {
        let out = encode_paste("");
        assert_eq!(out, b"\x1b[200~\x1b[201~");
    }

    #[test]
    fn encode_paste_preserves_newlines() {
        // Multi-line pastes are a common case (a snippet of code) and
        // the embedded child needs to receive the newlines verbatim,
        // not translated.
        let out = encode_paste("line1\nline2");
        assert_eq!(out, b"\x1b[200~line1\nline2\x1b[201~");
    }

    #[test]
    fn write_input_reaches_child_via_pty_echo() {
        // PTYs default to cooked mode with ECHO on, so bytes written to
        // the master are echoed back through the kernel's line
        // discipline before `cat` sees them. Writing "hello\n" to the
        // master should produce "hello" in the parser within
        // milliseconds, regardless of whether `cat` has run yet.
        let argv = vec!["/bin/cat".to_string()];
        let mut pty = EmbeddedPty::spawn(&argv, None, 24, 80).unwrap();
        pty.write_input(b"hello\n").unwrap();
        assert!(
            wait_for_screen(&pty, |s| s.contains("hello"), Duration::from_secs(3)),
            "screen was {:?}",
            pty.screen_text()
        );
    }
}
