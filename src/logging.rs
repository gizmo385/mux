//! Process-wide diagnostics log.
//!
//! agent-mux runs a full-screen ratatui UI on the terminal's alternate
//! screen. Anything written to stdout/stderr *while the UI is up* paints
//! straight over the dashboard — and once you're on the alt-screen there's
//! no scrollback to recover it from afterward. Background threads (the
//! per-host SSH pollers) legitimately produce diagnostic chatter —
//! reconnects, backoff, poll failures — that must not corrupt the display
//! yet shouldn't be lost. This module gives them a file to write to instead.
//!
//! Design: a single append target opened once at startup via [`init`], held
//! behind a `OnceLock<Option<Mutex<File>>>`. Best-effort throughout — if the
//! file can't be opened, or [`init`] is never called (non-TUI subcommands),
//! [`log_line`] silently no-ops. Diagnostics must never take the app down.
//!
//! The file is truncated on each [`init`] so it can't grow without bound
//! across runs; the current run's log is what matters for diagnosing a live
//! session. Size-bounded rotation is a possible follow-up (see `TODO.md`).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static LOG: OnceLock<Option<Mutex<File>>> = OnceLock::new();

/// Default log path: `<cache-dir>/agent-mux/agent-mux.log`, alongside the
/// `sessions/` cache directory. `None` when no cache dir resolves on this
/// platform.
#[must_use]
pub fn default_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("agent-mux").join("agent-mux.log"))
}

/// Open (truncating) the diagnostics log at `path`, creating parent
/// directories as needed. Idempotent-ish: only the first call wins (a
/// `OnceLock`), later calls are ignored. Best-effort — a failure to open
/// leaves logging as a silent no-op rather than surfacing an error, because
/// a diagnostics sink that can break startup is worse than no sink.
pub fn init(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .ok();
    let _ = LOG.set(file.map(Mutex::new));
}

/// Append a timestamped line to the diagnostics log. No-op if [`init`] was
/// never called or the file couldn't be opened. Never panics — a poisoned
/// lock or a write error is swallowed (the alternative is taking down a
/// poller thread over a log line).
pub fn log_line(msg: &str) {
    let Some(Some(mutex)) = LOG.get() else {
        return;
    };
    let Ok(mut file) = mutex.lock() else {
        return;
    };
    let _ = write_record(&mut *file, SystemTime::now(), msg);
}

/// Write one `[<epoch-secs>] <msg>\n` record and flush. Split out from
/// [`log_line`] so the record format is unit-testable against an in-memory
/// buffer without standing up the global file sink (whose `OnceLock` can
/// only be initialised once per process).
fn write_record<W: Write>(w: &mut W, now: SystemTime, msg: &str) -> io::Result<()> {
    let secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    writeln!(w, "[{secs}] {msg}")?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn write_record_formats_timestamp_and_message() {
        let mut buf = Vec::new();
        let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        write_record(&mut buf, t, "reconnect to host 'alpenglow' failed: timeout").unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "[1700000000] reconnect to host 'alpenglow' failed: timeout\n"
        );
    }

    #[test]
    fn write_record_handles_pre_epoch_time_without_panicking() {
        // `duration_since(UNIX_EPOCH)` errors for a pre-epoch instant;
        // the record falls back to 0 rather than panicking a poller thread.
        let mut buf = Vec::new();
        let t = UNIX_EPOCH - Duration::from_secs(5);
        write_record(&mut buf, t, "x").unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "[0] x\n");
    }

    #[test]
    fn default_path_sits_beside_the_session_cache() {
        // Only asserts the tail so it's platform-agnostic (the cache root
        // varies); the file must be `.../agent-mux/agent-mux.log`.
        if let Some(p) = default_path() {
            assert!(
                p.ends_with("agent-mux/agent-mux.log"),
                "got {}",
                p.display()
            );
        }
    }

    #[test]
    fn log_line_is_a_noop_when_uninitialised() {
        // In a test process `init` may never have run (or ran for another
        // test); `log_line` must not panic in that state.
        log_line("no sink configured — must not panic");
    }
}
