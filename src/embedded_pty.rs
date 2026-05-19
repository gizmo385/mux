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

use portable_pty::{CommandBuilder, ExitStatus, MasterPty, NativePtySystem, PtySize, PtySystem};
use ratatui::Frame;
use ratatui::layout::Rect;
use tui_term::widget::PseudoTerminal;

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
/// Drop semantics: dropping `EmbeddedPty` drops `master`, which closes
/// the pty; the child observes EOF on its stdin and SIGHUP on its
/// controlling tty, exits; the reader thread sees EOF on its read, reaps
/// the child, and returns. We deliberately do not `join` the reader on
/// drop — a child that ignores SIGHUP must not stall app shutdown.
pub struct EmbeddedPty {
    parser: Arc<RwLock<vt100::Parser>>,
    /// Held to keep the pty alive (drop closes it) and to service
    /// `resize`. The reader and writer live on handles cloned from the
    /// master at spawn time, so we never need to lock the master itself.
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    events: mpsc::Receiver<PtyEvent>,
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
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        if let Ok(p) = self.parser.read() {
            let widget = PseudoTerminal::new(p.screen());
            frame.render_widget(widget, area);
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
}

fn map_pty_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(e.to_string())
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
    use std::time::{Duration, Instant};

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
        let argv = vec!["/bin/true".to_string()];
        let pty = EmbeddedPty::spawn(&argv, None, 24, 80).unwrap();
        let status = wait_for_exit(&pty, Duration::from_secs(3)).expect("Exited event");
        assert!(status.success(), "expected zero exit, got {status:?}");
    }

    #[test]
    fn spawn_propagates_nonzero_exit_status() {
        let argv = vec!["/bin/false".to_string()];
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
