use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::session::HostId;

/// One entry returned by [`Host::list_transcripts`]: an absolute transcript
/// path and its last-modified time. The mtime lets the polling watcher
/// (M2 next chunk) skip files that haven't changed since the last scan.
#[derive(Debug, Clone)]
pub struct TranscriptStat {
    pub path: PathBuf,
    pub mtime: SystemTime,
}

/// Hides the local-vs-SSH distinction for *read* operations on a host's
/// Claude Code transcripts and worktree metadata. Spawn/attach operations
/// live in the Attachment Driver, not here.
///
/// Trait-object–safe: all methods take `&self` and use only concrete types.
pub trait Host: Send + Sync {
    /// Stable identifier — matches the dashboard label and the
    /// `[hosts.<name>]` config key (or [`HostId::local`] for the local
    /// implicit host).
    fn id(&self) -> &HostId;

    /// Two-level walk under `root` (matches Claude Code's layout:
    /// `root/<project-hash>/<session-id>.jsonl`). Returns absolute paths
    /// and mtimes for every `.jsonl` discovered. A missing `root` is not
    /// an error — returns an empty `Vec` so callers don't have to special-
    /// case first-run.
    ///
    /// # Errors
    /// Propagates any I/O error other than `NotFound` on `root`.
    fn list_transcripts(&self, root: &Path) -> io::Result<Vec<TranscriptStat>>;

    /// Read the whole file as UTF-8. Intended for small files (transcript
    /// metadata extraction, `.agent-mux/task.toml`); use [`Host::read_tail`]
    /// for transcripts where only the tail is needed.
    ///
    /// # Errors
    /// Propagates any I/O or UTF-8 decoding error.
    fn read_to_string(&self, path: &Path) -> io::Result<String>;

    /// Read the last `n_bytes` of a file. Used by attention derivation
    /// against transcripts that can be megabytes long; reading the whole
    /// file every tick would be wasteful, and over SSH would be unusably
    /// slow. Local impl seeks; SSH impl will shell out to `tail -c <n>`.
    ///
    /// Returns a String via [`String::from_utf8_lossy`] so a tail that
    /// happens to start mid-codepoint still produces parseable output for
    /// the line-by-line JSON consumer (the bad line fails to parse and
    /// gets skipped, exactly as it would in the legacy `BufReader::lines`
    /// path).
    ///
    /// # Errors
    /// Propagates any I/O error from opening, stating, or seeking.
    fn read_tail(&self, path: &Path, n_bytes: u64) -> io::Result<String>;

    /// True iff `path` exists and is a directory. Used by discovery's
    /// stale-session filter (skip transcripts whose `cwd` no longer
    /// exists on disk). Errors are folded into `false` because the only
    /// useful question is "can I attach a session rooted here?" and a
    /// failed stat is indistinguishable from "no" for that purpose.
    fn is_dir(&self, path: &Path) -> bool;

    /// Bulk-read `paths` in a single host round-trip. Returns one
    /// per-path [`io::Result`] in input order; a missing file lands in
    /// its slot as `Err(ErrorKind::NotFound)` rather than failing the
    /// whole batch, because discovery's task-metadata reads expect a
    /// `NotFound` for sessions without a `.agent-mux/task.toml` and
    /// need to distinguish that from a transport failure.
    ///
    /// Used by discovery to collapse N sequential `read_to_string`
    /// round-trips into one — over a high-latency SSH proxy that's
    /// the difference between ~30s and ~0.5s on a 20-session host.
    /// Local impl iterates; the trait surface is the same so callers
    /// stay host-agnostic.
    ///
    /// # Errors
    /// Propagates only transport-level failures (the SSH process exits
    /// non-zero, the local I/O subsystem errors on the batch as a
    /// whole). Per-path errors land inside the returned vec.
    fn read_many(&self, paths: &[&Path]) -> io::Result<Vec<io::Result<String>>>;

    /// Bulk-`is_dir` check; one `bool` per input path in order. Same
    /// rationale as [`Host::read_many`]: discovery's stale-cwd filter
    /// hits every unique `project_dir` and the round-trip cost has to
    /// amortize. Local impl iterates over `Path::is_dir`; SSH impl
    /// runs one remote `test -d` loop and parses the Y/N stream.
    ///
    /// # Errors
    /// Propagates transport-level failures only. A path that doesn't
    /// exist (or isn't a directory) lands as `false` in its slot,
    /// matching the single-path [`Host::is_dir`] contract.
    fn is_dir_many(&self, paths: &[&Path]) -> io::Result<Vec<bool>>;

    /// Run an arbitrary command on this host, optionally `cd`-ing
    /// to `cwd` first. Returns the full process [`Output`] (status,
    /// stdout, stderr) so callers can dispatch on exit code without
    /// a one-size-fits-all "success-or-error" baked into the trait
    /// — `git symbolic-ref` exiting 1 is a "fall back to main"
    /// signal, while `git worktree add` exiting 1 should propagate
    /// stderr to the user.
    ///
    /// The seam that lets [`crate::worktree::WorktreeManager`] and
    /// the default-branch resolver run `git` against the right host
    /// without an `if is_local()` branch outside the trait. `LocalHost`
    /// uses `Command::current_dir`; `SshHost` prefixes the shell
    /// command line with `cd <quoted cwd> && ` so the *remote* shell
    /// handles directory changes, keeping the trait surface
    /// host-agnostic.
    ///
    /// # Errors
    /// Returns `io::Error` only for transport-level failures
    /// (couldn't spawn the local process; the SSH `ssh` subprocess
    /// failed to start; the OS killed the wrapper). Non-zero exit
    /// status of the *target* command is **not** an error — inspect
    /// `Output::status` directly.
    fn run(&self, cwd: Option<&Path>, program: &str, args: &[&str]) -> io::Result<Output>;

    /// Write `content` to `path`, overwriting if it exists. The
    /// write-side counterpart to [`Host::read_to_string`]. Intended
    /// for small files (`.agent-mux/task.toml` is the motivating
    /// case — remote worktree creation needs to drop metadata
    /// alongside the worktree directory).
    ///
    /// Not atomic. If the operation is interrupted mid-write, `path`
    /// may end up partially written. Acceptable for the
    /// session-creation use case because a retry creates a fresh
    /// worktree from scratch anyway. An atomic `tmp + rename` shape
    /// can drop in later behind the same trait method when a caller
    /// needs durability guarantees.
    ///
    /// # Errors
    /// Propagates I/O errors from the local FS or the SSH transport.
    fn write_file(&self, path: &Path, content: &str) -> io::Result<()>;

    /// Flat directory listing — every regular file directly inside
    /// `dir` (no recursion), as absolute paths. Used by the hook-marker
    /// ingest path: hooks land under `<transcripts-root>/.agent-mux-hooks/`
    /// and the watcher needs to enumerate them per tick over the same
    /// connection it already uses for transcript polling.
    ///
    /// Missing `dir` is **not** an error — returns an empty `Vec`. The
    /// hook directory only exists after the first hook fires, so a
    /// startup poll against a never-fired-yet host must succeed
    /// cleanly rather than surface a transport failure.
    ///
    /// # Errors
    /// Propagates transport-level failures only.
    fn list_files(&self, dir: &Path) -> io::Result<Vec<PathBuf>>;

    /// Delete a single file at `path`. A missing file is **not** an
    /// error (idempotent — a hook marker that the watcher consumed
    /// can be re-delete-attempted at startup without a spurious
    /// failure). Anything else propagates.
    ///
    /// Used by the hook-marker ingest path to clean up after read so
    /// the directory doesn't grow without bound. Sibling to
    /// [`Host::write_file`] on the I/O surface.
    ///
    /// # Errors
    /// Propagates I/O errors other than `NotFound`.
    fn remove(&self, path: &Path) -> io::Result<()>;

    /// Build the argv that runs `remote_cmd` against this host.
    /// Returns `None` for local hosts (the caller runs `remote_cmd`
    /// directly). For SSH hosts, returns the argv that wraps it in an
    /// `ssh -S <ctrl-socket> [-t] <target>` invocation reusing the
    /// existing `ControlMaster` connection.
    ///
    /// `tty` toggles `-t`, which is required for interactive tmux
    /// attach but wasteful for one-shot capture (`tmux list-panes`).
    ///
    /// Lives on `Host` as informational argv construction — the
    /// `AttachmentDriver` still does the actual spawning. Keeping the
    /// SSH binary/socket/target details inside the trait's impl
    /// preserves the discipline that callers stay host-agnostic
    /// without `is_local()` branches.
    fn ssh_argv(&self, tty: bool, remote_cmd: &[&str]) -> Option<Vec<String>>;

    /// Ensure the host's connection is healthy, re-establishing it if a
    /// cheap liveness probe shows it has died.
    ///
    /// Called proactively by the background pollers (not on the attach
    /// hot path) so the connection is warm *before* the user switches
    /// sessions. The motivating failure: a laptop sleeps overnight, the
    /// SSH `ControlMaster` times out past `ControlPersist` and its TCP
    /// connection drops, and — with no re-establishment — every later
    /// `ssh -S <socket>` silently falls back to a full TCP+TLS+auth
    /// handshake. That turns each poll tick and each session switch into
    /// seconds of blocking I/O, exactly the "session switching never
    /// blocks on I/O" property `ARCHITECTURE.md` makes load-bearing.
    ///
    /// Returns `Ok(true)` when a reconnect actually happened, `Ok(false)`
    /// when the existing connection was already healthy (the common
    /// case, and cheap — one local-socket probe). The default impl is a
    /// no-op for connectionless hosts ([`LocalHost`]).
    ///
    /// # Errors
    /// Propagates the `io::Error` from re-establishing a dead connection
    /// (e.g. the host is genuinely unreachable). Callers should treat an
    /// error as "still disconnected, retry next tick" rather than fatal.
    fn ensure_connected(&self) -> io::Result<bool> {
        Ok(false)
    }
}

/// `Host` implementation for the local machine. Pure `std::fs` calls; no
/// owned state beyond its identity.
#[derive(Debug, Clone)]
pub struct LocalHost {
    id: HostId,
}

impl LocalHost {
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: HostId::local(),
        }
    }
}

impl Default for LocalHost {
    fn default() -> Self {
        Self::new()
    }
}

impl Host for LocalHost {
    fn id(&self) -> &HostId {
        &self.id
    }

    fn list_transcripts(&self, root: &Path) -> io::Result<Vec<TranscriptStat>> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(root) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e),
        };
        for project_dir in entries {
            let project_dir = project_dir?;
            if !project_dir.file_type()?.is_dir() {
                continue;
            }
            for jsonl in fs::read_dir(project_dir.path())? {
                let jsonl = jsonl?;
                let path = jsonl.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let meta = fs::metadata(&path)?;
                let mtime = meta.modified()?;
                out.push(TranscriptStat { path, mtime });
            }
        }
        Ok(out)
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        fs::read_to_string(path)
    }

    fn read_tail(&self, path: &Path, n_bytes: u64) -> io::Result<String> {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        let start = len.saturating_sub(n_bytes);
        file.seek(SeekFrom::Start(start))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn read_many(&self, paths: &[&Path]) -> io::Result<Vec<io::Result<String>>> {
        // Local FS has no per-call round-trip overhead — the batched
        // shape exists for SSH amortization, not because std::fs::read
        // benefits from batching. Iterating preserves per-path error
        // resolution and keeps the local code path obvious.
        Ok(paths.iter().map(fs::read_to_string).collect())
    }

    fn is_dir_many(&self, paths: &[&Path]) -> io::Result<Vec<bool>> {
        Ok(paths.iter().map(|p| p.is_dir()).collect())
    }

    fn run(&self, cwd: Option<&Path>, program: &str, args: &[&str]) -> io::Result<Output> {
        let mut cmd = Command::new(program);
        cmd.args(args);
        if let Some(d) = cwd {
            cmd.current_dir(d);
        }
        // Always strip git-pointer env vars so a process running inside
        // a git hook (or a `cargo test` invocation from one) can't
        // accidentally see GIT_DIR / GIT_WORK_TREE / etc. set by the
        // outer git invocation and silently override `current_dir`.
        // Stripping is harmless for non-git programs — they ignore
        // these names. SSH hosts don't need this because ssh login
        // shells start with a fresh env.
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_COMMON_DIR",
            "GIT_PREFIX",
        ] {
            cmd.env_remove(var);
        }
        cmd.output()
    }

    fn write_file(&self, path: &Path, content: &str) -> io::Result<()> {
        fs::write(path, content)
    }

    fn list_files(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                out.push(entry.path());
            }
        }
        Ok(out)
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn ssh_argv(&self, _tty: bool, _remote_cmd: &[&str]) -> Option<Vec<String>> {
        None
    }
}

/// `Host` implementation that reaches a remote machine over SSH, reusing
/// a single `ControlMaster` connection for every operation so per-call
/// cost is a local socket round-trip rather than a fresh TCP+TLS+auth
/// handshake. The master is opened by [`SshHost::connect`] and torn down
/// by the `Drop` impl; the rest of the codebase sees the same `Host`
/// surface as [`LocalHost`].
///
/// Why shell out to the system `ssh` binary instead of a Rust-native
/// client: the user's `~/.ssh/config` aliases, agent forwarding, jump
/// hosts, keys, and per-host quirks come for free (see
/// `ARCHITECTURE.md` → Tech stack).
pub struct SshHost {
    id: HostId,
    ssh_target: String,
    control_path: PathBuf,
    /// Path to the `ssh` binary. Always `"ssh"` in production; the
    /// `#[cfg(test)]` constructor takes a different value so unit tests
    /// can exercise command construction without contacting a real host.
    ssh_binary: PathBuf,
    /// Guards [`SshHost::ensure_connected`]'s re-establishment. Two
    /// roles: (1) *serialise* the re-spawn so the transcript and pane
    /// pollers sharing this `Arc<SshHost>` can't both delete the socket
    /// and spawn duelling masters; (2) hold the *backoff* state so a
    /// master that can't be re-established (e.g. a proxy that won't
    /// sustain `-fN -M`) isn't re-spawned every poll tick — each doomed
    /// attempt costs a `ConnectTimeout`-bounded ssh, and unthrottled
    /// they pile up into a visible storm of short-lived ssh processes.
    reconnect: Mutex<ReconnectState>,
}

/// Backoff bookkeeping for [`SshHost::ensure_connected`], shared across
/// the pollers via the `reconnect` mutex.
#[derive(Default)]
struct ReconnectState {
    /// Consecutive failed re-spawns; `0` whenever the master is healthy.
    fails: u32,
    /// Earliest instant the next re-spawn may be attempted. `None` =
    /// "attempt now" (healthy, or never failed).
    next_attempt: Option<Instant>,
}

impl ReconnectState {
    fn reset(&mut self) {
        self.fails = 0;
        self.next_attempt = None;
    }
}

/// First backoff window after a failed re-spawn; doubles each further
/// consecutive failure up to [`RECONNECT_BACKOFF_MAX`].
const RECONNECT_BACKOFF_BASE: Duration = Duration::from_secs(6);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_mins(1);

impl SshHost {
    /// Open a `ControlMaster` connection to `ssh_target` and return a
    /// handle that reuses it for every subsequent operation. The master
    /// persists for the lifetime of the returned `SshHost` and is closed
    /// by the drop guard.
    ///
    /// # Errors
    /// Propagates the `io::Error` from spawning `ssh`, or returns
    /// [`io::Error::other`] if the `ssh` process exits non-zero (auth
    /// failure, host unreachable, etc).
    pub fn connect(id: HostId, ssh_target: String) -> io::Result<Self> {
        Self::connect_with_binary(id, ssh_target, PathBuf::from("ssh"))
    }

    fn connect_with_binary(
        id: HostId,
        ssh_target: String,
        ssh_binary: PathBuf,
    ) -> io::Result<Self> {
        let control_path = control_socket_path(&id);
        Self::spawn_master(&ssh_binary, &control_path, &ssh_target)?;
        Ok(Self {
            id,
            ssh_target,
            control_path,
            ssh_binary,
            reconnect: Mutex::new(ReconnectState::default()),
        })
    }

    /// Spawn the backgrounded `ControlMaster` on `control_path` and
    /// verify it is actually listening with `-O check`. Shared by the
    /// initial [`SshHost::connect`] and the [`SshHost::ensure_connected`]
    /// reconnect path so both issue identical flags and get the same
    /// verify-after-spawn guarantee.
    fn spawn_master(ssh_binary: &Path, control_path: &Path, ssh_target: &str) -> io::Result<()> {
        // -fN  = fork into background, run no remote command
        // -M   = master mode
        // -S   = control socket path (clients reuse via the same path)
        // ControlPersist=600 = if our Drop guard never fires (SIGKILL,
        // panic during unwind), the remote-side master self-terminates
        // after ten minutes of idleness rather than lingering forever.
        // ConnectTimeout=5 caps a wedged-host connect at 5s so a single
        // unreachable host can't stall the dashboard's startup discovery
        // (which we run in parallel threads, but each thread still needs
        // a bounded worst case).
        let status = Command::new(ssh_binary)
            .arg("-fN")
            .arg("-M")
            .arg("-S")
            .arg(control_path)
            .arg("-o")
            .arg("ControlPersist=600")
            .arg("-o")
            .arg("ConnectTimeout=5")
            // Compression is set on the ControlMaster so every channel
            // multiplexed through it inherits it. Discovery's bulk
            // transcript read shovels JSON over the wire — for a
            // long-lived remote with megabytes of Claude Code
            // transcripts to fetch, gzip at the SSH layer typically
            // cuts wall-clock 3–5× because the proxy bandwidth is the
            // bottleneck, not the per-round-trip latency. CPU cost
            // is rounding error compared to the savings.
            .arg("-o")
            .arg("Compression=yes")
            .arg(ssh_target)
            .status()?;
        if !status.success() {
            return Err(io::Error::other(format!(
                "ssh ControlMaster setup failed for {ssh_target} (exit {status})"
            )));
        }
        // `-fN` returns 0 the moment the parent forks the backgrounded
        // master — its exit status proves the fork happened, not that
        // the master is actually listening on `control_path`. A
        // ProxyCommand that doesn't survive the fork-to-background
        // (some `coder ssh --stdio` setups, some jumphosts) makes the
        // master die seconds after spawn; without a positive check
        // here, every later `ssh -S <control_path>` would silently
        // fall back to a fresh connection and the user would never see
        // an error — they'd just pay full SSH cost on every poll tick
        // and session switch, violating ARCHITECTURE.md's
        // "session switching never blocks on I/O" property invisibly.
        // `-O check` asks the master directly and exits non-zero when
        // nothing is listening on the socket.
        if !Self::master_check(ssh_binary, control_path, ssh_target) {
            // Best-effort cleanup in case the master partially
            // established. Mirrors Drop's shape; errors swallowed
            // because the connect is failing either way.
            let _ = Command::new(ssh_binary)
                .arg("-S")
                .arg(control_path)
                .arg("-O")
                .arg("exit")
                .arg(ssh_target)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            return Err(io::Error::other(format!(
                "ssh ControlMaster did not establish for {ssh_target} \
                 (master spawned successfully but `-O check` failed). \
                 Common cause: a ProxyCommand that does not sustain a backgrounded master."
            )));
        }
        Ok(())
    }

    /// `ssh -O check`: ask the master on `control_path` whether it is
    /// alive. Exits non-zero (→ `false`) when nothing is listening on
    /// the socket — a never-spawned, timed-out, or TCP-dropped master.
    /// Cheap: a local-socket round-trip, no network handshake.
    fn master_check(ssh_binary: &Path, control_path: &Path, ssh_target: &str) -> bool {
        Command::new(ssh_binary)
            .arg("-S")
            .arg(control_path)
            .arg("-O")
            .arg("check")
            .arg(ssh_target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn ssh_command(&self) -> Command {
        let mut cmd = Command::new(&self.ssh_binary);
        cmd.arg("-S").arg(&self.control_path);
        cmd.arg(&self.ssh_target);
        cmd
    }

    fn exec_script(&self, remote_cmd: &str) -> io::Result<Vec<u8>> {
        let output = self.ssh_command().arg(remote_cmd).output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "ssh `{remote_cmd}` failed on {}: {}",
                self.ssh_target,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(output.stdout)
    }
}

impl Drop for SshHost {
    fn drop(&mut self) {
        // Best-effort tear-down. Errors are swallowed because there is no
        // useful recovery — worst case the remote master times out per
        // ControlPersist above. Stdin/out/err are nulled so a slow exit
        // does not bleed into the user's terminal.
        let _ = Command::new(&self.ssh_binary)
            .arg("-S")
            .arg(&self.control_path)
            .arg("-O")
            .arg("exit")
            .arg(&self.ssh_target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Host for SshHost {
    fn id(&self) -> &HostId {
        &self.id
    }

    fn list_transcripts(&self, root: &Path) -> io::Result<Vec<TranscriptStat>> {
        // `if [ -d ROOT ]` folds the missing-root case into "exit 0,
        // empty stdout" so the trait's "missing root is not an error"
        // contract holds without parsing exit codes. `find -printf` is a
        // GNU extension; macOS hosts need `findutils` from Homebrew. We
        // assume Linux remotes for now — a portability item is filed in
        // TODO if a macOS remote ever surfaces.
        let quoted = shell_quote_path(&root.to_string_lossy());
        let cmd = format!(
            "if [ -d {quoted} ]; then \
                find {quoted} -mindepth 2 -maxdepth 2 -type f -name '*.jsonl' \
                    -printf '%T@ %p\\0' 2>/dev/null; \
             fi"
        );
        let stdout = self.exec_script(&cmd)?;
        parse_find_output(&stdout)
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        let quoted = shell_quote_path(&path.to_string_lossy());
        let stdout = self.exec_script(&format!("cat {quoted}"))?;
        String::from_utf8(stdout).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    fn read_tail(&self, path: &Path, n_bytes: u64) -> io::Result<String> {
        let quoted = shell_quote_path(&path.to_string_lossy());
        let stdout = self.exec_script(&format!("tail -c {n_bytes} {quoted}"))?;
        Ok(String::from_utf8_lossy(&stdout).into_owned())
    }

    fn is_dir(&self, path: &Path) -> bool {
        let quoted = shell_quote_path(&path.to_string_lossy());
        let status = self
            .ssh_command()
            .arg(format!("test -d {quoted}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        matches!(status, Ok(s) if s.success())
    }

    fn read_many(&self, paths: &[&Path]) -> io::Result<Vec<io::Result<String>>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        // Remote loop: emit one length-prefixed record per input path.
        // `OK <bytes>\n<bytes>` for an existing regular file, `MISS\n`
        // for anything that fails the `-f` test. `wc -c < "$p"` (no
        // path arg) avoids printing the filename; `tr -d ' '` strips
        // the leading whitespace BSD `wc` adds. Paths are emitted in
        // input order so the parser doesn't need to associate by name.
        let mut script = String::from("set -e; ");
        for p in paths {
            let quoted = shell_quote_path(&p.to_string_lossy());
            let _ = write!(
                script,
                "if [ -f {quoted} ]; then \
                    sz=$(wc -c < {quoted} | tr -d ' '); \
                    printf 'OK %s\\n' \"$sz\"; \
                    cat {quoted}; \
                else \
                    printf 'MISS\\n'; \
                fi; "
            );
        }
        let stdout = self.exec_script(&script)?;
        parse_read_many_output(&stdout, paths.len())
    }

    fn is_dir_many(&self, paths: &[&Path]) -> io::Result<Vec<bool>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        // One `Y\n` or `N\n` per input path, in order. Cheaper wire
        // format than `read_many` because there's no payload.
        let mut script = String::new();
        for p in paths {
            let quoted = shell_quote_path(&p.to_string_lossy());
            let _ = write!(script, "if [ -d {quoted} ]; then echo Y; else echo N; fi; ");
        }
        let stdout = self.exec_script(&script)?;
        let mut out = Vec::with_capacity(paths.len());
        for line in stdout.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            match line {
                b"Y" => out.push(true),
                b"N" => out.push(false),
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "is_dir_many: unexpected line {:?}",
                            String::from_utf8_lossy(other)
                        ),
                    ));
                }
            }
        }
        if out.len() != paths.len() {
            return Err(io::Error::other(format!(
                "is_dir_many: expected {} results, got {}",
                paths.len(),
                out.len()
            )));
        }
        Ok(out)
    }

    fn run(&self, cwd: Option<&Path>, program: &str, args: &[&str]) -> io::Result<Output> {
        // Build one shell-script string the remote shell evaluates as a
        // single command. The `cd <cwd> && ` prefix puts cwd-handling
        // on the remote side so callers don't branch on host kind —
        // same shape as Command::current_dir on the local side. Each
        // token is quoted via shell_quote_path (not shell_single_quote),
        // matching how cwd above is treated: a leading `~/` in an arg
        // stays outside the quotes so the remote shell expands it to
        // the *remote* user's home. Without this, `Host::run(None,
        // "find", &["~/workspace", …])` ships `'~/workspace'` and the
        // tilde is taken literally — see TODO entry "remote workspace
        // scan returns no repos when workspace_folders paths use ~/".
        // Non-tilde args still get plain single-quoting, so spaces /
        // globs / leading `#` round-trip verbatim.
        let mut script = String::new();
        if let Some(d) = cwd {
            let _ = write!(script, "cd {} && ", shell_quote_path(&d.to_string_lossy()));
        }
        let quoted = std::iter::once(program)
            .chain(args.iter().copied())
            .map(shell_quote_path)
            .collect::<Vec<_>>()
            .join(" ");
        script.push_str(&quoted);
        self.ssh_command().arg(&script).output()
    }

    fn list_files(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        // `if [ -d DIR ]` folds the missing-dir case into "exit 0,
        // empty stdout" — matches the trait contract that a never-
        // fired-yet hook dir is not an error. `find -print0` is the
        // GNU extension we already depend on for `list_transcripts`,
        // so portability assumptions are unchanged.
        let quoted = shell_quote_path(&dir.to_string_lossy());
        let cmd = format!(
            "if [ -d {quoted} ]; then \
                find {quoted} -mindepth 1 -maxdepth 1 -type f -print0 2>/dev/null; \
             fi"
        );
        let stdout = self.exec_script(&cmd)?;
        let mut out = Vec::new();
        for chunk in stdout.split(|&b| b == 0) {
            if chunk.is_empty() {
                continue;
            }
            let path_str = std::str::from_utf8(chunk)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            out.push(PathBuf::from(path_str));
        }
        Ok(out)
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        // `rm -f` swallows the "file doesn't exist" case, matching
        // the trait's idempotent contract — a marker we already
        // ingested and deleted shouldn't fail a retry.
        let quoted = shell_quote_path(&path.to_string_lossy());
        self.exec_script(&format!("rm -f {quoted}"))?;
        Ok(())
    }

    fn write_file(&self, path: &Path, content: &str) -> io::Result<()> {
        // `cat > <path>` consumes stdin and writes verbatim, so the
        // local-side `child.stdin.write_all(content)` is what actually
        // delivers the bytes — no shell-escaping of the content needed,
        // which would be a quoting nightmare for arbitrary TOML / JSON.
        let quoted = shell_quote_path(&path.to_string_lossy());
        let mut cmd = self.ssh_command();
        cmd.arg(format!("cat > {quoted}"));
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        {
            use std::io::Write as _;
            let stdin = child
                .stdin
                .as_mut()
                .expect("stdin piped — Stdio::piped guarantees Some");
            stdin.write_all(content.as_bytes())?;
        }
        // Close stdin by dropping the handle so the remote `cat`
        // sees EOF and exits.
        drop(child.stdin.take());
        let output = child.wait_with_output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "ssh write_file({}) failed on {}: {}",
                path.display(),
                self.ssh_target,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }

    fn ssh_argv(&self, tty: bool, remote_cmd: &[&str]) -> Option<Vec<String>> {
        let mut argv = vec![
            self.ssh_binary.to_string_lossy().into_owned(),
            "-S".into(),
            self.control_path.display().to_string(),
        ];
        if tty {
            argv.push("-t".into());
        }
        argv.push(self.ssh_target.clone());
        // Shell-quote-and-join into a single final argv element. SSH
        // sends every post-target argv joined by space to the remote
        // shell as one command line, where it gets re-tokenized. If
        // we pass each element raw, anything with a space, glob char,
        // or — most subtly — a leading `#` (which sh treats as a
        // comment marker at word-start, e.g. tmux's `-F #{format}`
        // would be eaten on the remote) breaks. Single-quoting each
        // element makes the remote shell pass it through verbatim.
        if !remote_cmd.is_empty() {
            argv.push(shell_join_quoted(remote_cmd));
        }
        Some(argv)
    }

    fn ensure_connected(&self) -> io::Result<bool> {
        // Cheap local-socket probe first (the common per-tick case).
        let alive = Self::master_check(&self.ssh_binary, &self.control_path, &self.ssh_target);
        // Lock guards both the re-spawn (serialised across the two
        // pollers sharing this `Arc<SshHost>`) and the backoff state. A
        // poisoned lock just means a prior holder panicked mid-respawn;
        // recover its state and carry on rather than cascade.
        let mut state = self
            .reconnect
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if alive {
            state.reset();
            return Ok(false);
        }
        // Double-check under the lock: a sibling thread (or our own last
        // attempt) may have rebuilt the master while we were probing /
        // waiting on the lock.
        if Self::master_check(&self.ssh_binary, &self.control_path, &self.ssh_target) {
            state.reset();
            return Ok(false);
        }
        // Backoff gate: if a recent re-spawn failed and we're still
        // inside the wait window, report "down" *without* attempting
        // another doomed `ssh -fN -M`. This is what keeps an
        // unsustainable master from being hammered every poll tick by
        // both pollers — the storm symptom dogfooding surfaced.
        if let Some(next) = state.next_attempt
            && Instant::now() < next
        {
            return Err(io::Error::other(format!(
                "ssh master for {} is down; backing off reconnect",
                self.ssh_target
            )));
        }
        // A dead master can leave a stale socket file behind, and
        // `ssh -fN -M` refuses to create a master when the socket path
        // already exists. Remove it first so the re-spawn lands cleanly.
        // Safe under the lock with the master confirmed dead — no live
        // channel depends on it. NotFound is fine.
        let _ = fs::remove_file(&self.control_path);
        match Self::spawn_master(&self.ssh_binary, &self.control_path, &self.ssh_target) {
            Ok(()) => {
                state.reset();
                Ok(true)
            }
            Err(e) => {
                // Grow the wait: 6s, 12s, 24s, 48s, capped at 60s. Shift
                // is bounded so it can't overflow.
                state.fails = state.fails.saturating_add(1);
                let wait = RECONNECT_BACKOFF_BASE
                    .saturating_mul(1u32 << (state.fails - 1).min(5))
                    .min(RECONNECT_BACKOFF_MAX);
                state.next_attempt = Some(Instant::now() + wait);
                Err(e)
            }
        }
    }
}

/// Shell-quote a path for inclusion in a remote command, preserving
/// tilde expansion. POSIX shells expand a *leading* `~` only when it
/// is unquoted; once we wrap it in single quotes, `'~/foo'` becomes a
/// literal path with a tilde character. For remote-host paths
/// (`transcript_root` defaults to `~/.claude/projects`, meaning the
/// remote user's home), we want the remote shell to do the expansion.
/// Strategy: leave a leading `~/` (or bare `~`) unquoted; single-quote
/// the rest. Paths without a leading tilde fall through to the
/// standard single-quote escape.
fn shell_quote_path(s: &str) -> String {
    if s == "~" {
        return "~".to_string();
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return format!("~/{}", shell_single_quote(rest));
    }
    shell_single_quote(s)
}

/// POSIX single-quote escape: wrap in `'…'`, splitting on any embedded
/// single quote via the `'\''` idiom. Safe for any byte string a path
/// can contain.
pub(crate) fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Shell-quote each element of `parts` and join with spaces, producing
/// a single POSIX-sh-parseable command line. Used in two places:
/// — inside [`SshHost::ssh_argv`] to encode `remote_cmd` as one final
///   argv element (SSH joins post-target args with space, so the
///   remote shell sees one string; quoting protects against word-
///   splitting, glob expansion, and leading-`#` comment-eating).
/// — in the `AttachmentDriver`, to embed an entire ssh argv as the
///   single command-string `tmux new-window` execs via `sh -c`.
pub(crate) fn shell_join_quoted<I, S>(parts: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    parts
        .into_iter()
        .map(|s| shell_single_quote(s.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parse the wire format emitted by `read_many`'s remote loop:
/// alternating `OK <bytes>\n<bytes>` (existing file) or `MISS\n`
/// (`NotFound`), one record per requested path, in input order.
/// Returns one `io::Result<String>` per record so per-path `NotFound`
/// is surfaced without failing the whole batch.
fn parse_read_many_output(bytes: &[u8], expected: usize) -> io::Result<Vec<io::Result<String>>> {
    let mut out: Vec<io::Result<String>> = Vec::with_capacity(expected);
    let mut cursor = 0usize;
    while out.len() < expected {
        let nl_offset = bytes[cursor..]
            .iter()
            .position(|&b| b == b'\n')
            .ok_or_else(|| {
                io::Error::other(format!(
                    "read_many: truncated header after {} of {} records",
                    out.len(),
                    expected
                ))
            })?;
        let header = std::str::from_utf8(&bytes[cursor..cursor + nl_offset])
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        cursor += nl_offset + 1;

        if header == "MISS" {
            out.push(Err(io::Error::from(io::ErrorKind::NotFound)));
        } else if let Some(size_str) = header.strip_prefix("OK ") {
            let size: usize = size_str.parse().map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("read_many: bad size {size_str:?}: {e}"),
                )
            })?;
            let end = cursor
                .checked_add(size)
                .ok_or_else(|| io::Error::other("read_many: size overflow"))?;
            if end > bytes.len() {
                return Err(io::Error::other(format!(
                    "read_many: short read — declared {size} bytes, got {}",
                    bytes.len().saturating_sub(cursor)
                )));
            }
            let content = String::from_utf8(bytes[cursor..end].to_vec())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            cursor = end;
            out.push(Ok(content));
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("read_many: bad header {header:?}"),
            ));
        }
    }
    Ok(out)
}

/// Parse the NUL-delimited output of `find -printf '%T@ %p\0'` into
/// [`TranscriptStat`] entries. NUL termination is what lets paths
/// containing whitespace round-trip correctly.
fn parse_find_output(bytes: &[u8]) -> io::Result<Vec<TranscriptStat>> {
    let mut out = Vec::new();
    for chunk in bytes.split(|&b| b == 0) {
        if chunk.is_empty() {
            continue;
        }
        let s = std::str::from_utf8(chunk)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let (mtime_str, path_str) = s.split_once(' ').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("malformed find chunk: {s:?}"),
            )
        })?;
        let mtime_f: f64 = mtime_str.parse().map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("malformed mtime {mtime_str:?}: {e}"),
            )
        })?;
        let mtime = epoch_seconds_to_systemtime(mtime_f);
        out.push(TranscriptStat {
            path: PathBuf::from(path_str),
            mtime,
        });
    }
    Ok(out)
}

fn epoch_seconds_to_systemtime(epoch_f: f64) -> SystemTime {
    if !epoch_f.is_finite() || epoch_f < 0.0 {
        return UNIX_EPOCH;
    }
    // Range-checked above; the float fits in u64 for any plausible
    // mtime (it'd take ~580 billion years to overflow). Subsecond
    // fract is always in [0, 1) so the *1e9 product fits in u32.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let secs = epoch_f.trunc() as u64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let nanos = (epoch_f.fract() * 1e9) as u32;
    UNIX_EPOCH + Duration::new(secs, nanos)
}

fn control_socket_path(id: &HostId) -> PathBuf {
    let pid = std::process::id();
    std::env::temp_dir().join(format!("agent-mux-ssh-{}-{pid}.sock", id.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir parent");
        }
        fs::write(path, content).expect("write file");
    }

    #[test]
    fn id_returns_local() {
        let host = LocalHost::new();
        assert!(host.id().is_local());
    }

    #[test]
    fn list_transcripts_returns_empty_for_missing_root() {
        let tmp = TempDir::new().expect("tempdir");
        let host = LocalHost::new();
        let stats = host
            .list_transcripts(&tmp.path().join("nope"))
            .expect("missing root is ok");
        assert!(stats.is_empty());
    }

    #[test]
    fn list_transcripts_finds_jsonl_two_levels_deep() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path().join("projects");
        write_file(&root.join("proj-a").join("s1.jsonl"), "{}\n");
        write_file(&root.join("proj-a").join("s2.jsonl"), "{}\n");
        write_file(&root.join("proj-b").join("s3.jsonl"), "{}\n");

        let host = LocalHost::new();
        let mut stats = host.list_transcripts(&root).expect("list");
        stats.sort_by(|a, b| a.path.cmp(&b.path));
        let names: Vec<_> = stats
            .iter()
            .map(|s| s.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["s1.jsonl", "s2.jsonl", "s3.jsonl"]);
        assert!(stats.iter().all(|s| s.mtime <= SystemTime::now()));
    }

    #[test]
    fn list_transcripts_skips_non_jsonl_and_top_level_files() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path().join("projects");
        write_file(&root.join("proj-a").join("s1.jsonl"), "{}\n");
        write_file(&root.join("proj-a").join("notes.txt"), "ignore");
        write_file(&root.join("README.md"), "ignore"); // top-level file: not a project dir

        let host = LocalHost::new();
        let stats = host.list_transcripts(&root).expect("list");
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].path.file_name().unwrap(), "s1.jsonl");
    }

    #[test]
    fn read_to_string_returns_file_contents() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("file.txt");
        fs::write(&path, "hello\n").expect("write");
        let host = LocalHost::new();
        assert_eq!(host.read_to_string(&path).expect("read"), "hello\n");
    }

    #[test]
    fn read_to_string_propagates_missing_file_error() {
        let tmp = TempDir::new().expect("tempdir");
        let host = LocalHost::new();
        let err = host
            .read_to_string(&tmp.path().join("nope"))
            .expect_err("should error");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn read_tail_returns_whole_file_when_smaller_than_n_bytes() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("file.txt");
        fs::write(&path, "abc\n").expect("write");
        let host = LocalHost::new();
        assert_eq!(host.read_tail(&path, 1024).expect("read"), "abc\n");
    }

    #[test]
    fn read_tail_returns_last_n_bytes_when_file_is_larger() {
        use std::fmt::Write as _;
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("file.txt");
        // 90 bytes total, ask for last 10.
        let mut content = String::new();
        for i in 0..10 {
            writeln!(content, "line-{i:03}").expect("write to string");
        }
        assert_eq!(content.len(), 90);
        fs::write(&path, &content).expect("write");
        let host = LocalHost::new();
        let tail = host.read_tail(&path, 10).expect("read");
        assert_eq!(tail.len(), 10);
        assert!(tail.ends_with("line-009\n"));
    }

    #[test]
    fn is_dir_recognises_directories_and_rejects_files() {
        let tmp = TempDir::new().expect("tempdir");
        let host = LocalHost::new();
        assert!(host.is_dir(tmp.path()));

        let file = tmp.path().join("file.txt");
        fs::write(&file, "x").expect("write");
        assert!(!host.is_dir(&file));

        assert!(!host.is_dir(&tmp.path().join("nope")));
    }

    // ---- SshHost helpers ----
    //
    // The full SSH round-trip (connect → run command → drop) is
    // verified by dogfooding against a real host once M2 wiring lands;
    // these tests cover the pure helpers so a regression in escaping or
    // output parsing is caught at `cargo test` time, not at runtime over
    // SSH where the failure mode is "ssh: invalid command" hundreds of
    // lines away from the cause.

    #[test]
    fn shell_single_quote_wraps_plain_string() {
        assert_eq!(super::shell_single_quote("hello"), "'hello'");
    }

    #[test]
    fn shell_single_quote_escapes_embedded_single_quote() {
        // The POSIX idiom: close-quote, backslash-quote, re-open.
        assert_eq!(super::shell_single_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn shell_quote_path_leaves_tilde_unquoted_so_remote_shell_expands_it() {
        // The bug this guards against: wrapping `~/.claude/projects` in
        // single quotes makes the remote shell read it as a literal
        // tilde character, and `test -d '~/.claude/projects'` fails.
        // Strategy: leading `~/` stays outside the quotes; the rest is
        // single-quoted so embedded oddities still escape safely.
        assert_eq!(
            super::shell_quote_path("~/.claude/projects"),
            "~/'.claude/projects'"
        );
    }

    #[test]
    fn shell_quote_path_handles_bare_tilde() {
        assert_eq!(super::shell_quote_path("~"), "~");
    }

    #[test]
    fn shell_quote_path_falls_through_for_absolute_paths() {
        assert_eq!(
            super::shell_quote_path("/services/attic"),
            "'/services/attic'"
        );
    }

    #[test]
    fn shell_quote_path_only_treats_leading_tilde_specially() {
        // `~user` (no slash) and an embedded tilde are not tilde-prefix
        // expansion — they should be fully single-quoted so they reach
        // the remote shell as literal characters.
        assert_eq!(super::shell_quote_path("~chris/x"), "'~chris/x'");
        assert_eq!(super::shell_quote_path("/tmp/~"), "'/tmp/~'");
    }

    #[test]
    fn shell_single_quote_handles_paths_with_spaces() {
        assert_eq!(
            super::shell_single_quote("/tmp/with space/file"),
            "'/tmp/with space/file'"
        );
    }

    #[test]
    fn parse_find_output_yields_empty_for_empty_input() {
        let stats = super::parse_find_output(b"").expect("empty parse");
        assert!(stats.is_empty());
    }

    #[test]
    fn parse_find_output_parses_single_entry() {
        let stats = super::parse_find_output(b"1700000000.5 /root/proj/s1.jsonl\0").expect("parse");
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].path, PathBuf::from("/root/proj/s1.jsonl"));
        let secs = stats[0]
            .mtime
            .duration_since(UNIX_EPOCH)
            .expect("post-epoch")
            .as_secs();
        assert_eq!(secs, 1_700_000_000);
    }

    #[test]
    fn parse_find_output_parses_multiple_entries_and_preserves_paths_with_spaces() {
        // `\x00` (not `\0`) before a digit so the byte literal isn't read
        // as an octal escape.
        let bytes = b"1.0 /a/with space/file.jsonl\x002.0 /b/x.jsonl\0";
        let stats = super::parse_find_output(bytes).expect("parse");
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].path, PathBuf::from("/a/with space/file.jsonl"));
        assert_eq!(stats[1].path, PathBuf::from("/b/x.jsonl"));
    }

    #[test]
    fn parse_find_output_skips_trailing_empty_chunk() {
        // `find -printf '...\0'` ends with a NUL, so split() emits a
        // trailing empty chunk. It must not be parsed as a malformed
        // entry.
        let stats = super::parse_find_output(b"1.0 /x.jsonl\0").expect("parse");
        assert_eq!(stats.len(), 1);
    }

    #[test]
    fn parse_find_output_rejects_chunk_without_space() {
        let err = super::parse_find_output(b"no-space-here\0").expect_err("should reject");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn parse_find_output_rejects_unparseable_mtime() {
        let err = super::parse_find_output(b"not-a-number /x.jsonl\0").expect_err("should reject");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn epoch_seconds_to_systemtime_handles_subsecond_precision() {
        let t = super::epoch_seconds_to_systemtime(1.5);
        let d = t.duration_since(UNIX_EPOCH).expect("post-epoch");
        assert_eq!(d.as_secs(), 1);
        // half a second ± rounding from f64.
        let nanos = d.subsec_nanos();
        assert!((450_000_000..=550_000_000).contains(&nanos), "got {nanos}");
    }

    #[test]
    fn epoch_seconds_to_systemtime_clamps_negative_to_epoch() {
        assert_eq!(super::epoch_seconds_to_systemtime(-1.0), UNIX_EPOCH);
    }

    #[test]
    fn epoch_seconds_to_systemtime_clamps_non_finite_to_epoch() {
        assert_eq!(super::epoch_seconds_to_systemtime(f64::NAN), UNIX_EPOCH);
        assert_eq!(
            super::epoch_seconds_to_systemtime(f64::INFINITY),
            UNIX_EPOCH
        );
    }

    #[test]
    fn local_host_ssh_argv_returns_none() {
        let host = LocalHost::new();
        assert!(host.ssh_argv(true, &["tmux", "ls"]).is_none());
    }

    // ---- shutdown audit: SshHost lifecycle ----
    //
    // SshHost owns a remote `ssh -fNM` master process; its Drop is the
    // best-effort teardown. These tests use a mock `ssh` binary (a
    // shell script that appends its argv to a log file) so we can
    // assert the *exact* commands SshHost issues across its lifetime
    // — both the connect-time master spawn and the Drop-time
    // `-O exit`. Without an end-to-end test like this, a refactor
    // that silently dropped the `-O exit` call would leak remote
    // master processes for ControlPersist's full 10-minute window
    // before they self-terminated.

    /// Test-only wrapper around [`SshHost::connect_with_binary`] that retries
    /// on `ErrorKind::ExecutableFileBusy` (Linux `ETXTBSY`).
    ///
    /// When `cargo test` runs the host tests in parallel, the test binary's
    /// process is shared across threads. While test A is inside its
    /// `fs::write(mock_ssh_A, ...)` call, a sibling test B's
    /// `Command::spawn` may fork the test binary; that fork inherits test
    /// A's still-open write fd. The inherited fd has `FD_CLOEXEC` set, but
    /// `CLOEXEC` only fires on the child's *exec*: between fork and exec
    /// there is a window where test A's exec on `mock_ssh_A` sees the
    /// still-open inherited write fd and returns `ETXTBSY`. This is a
    /// well-known Linux race with no clean fix short of serialising every
    /// fork in the process, so we absorb it here with a short bounded
    /// retry. Production callers go through [`SshHost::connect`], whose
    /// target is the system `ssh` binary — that file is not being rewritten
    /// under us, so the race cannot fire there and the retry would be dead
    /// code. Keeping the retry test-only preserves that invariant.
    #[cfg(unix)]
    fn connect_with_binary_retrying_etxtbsy(
        id: HostId,
        ssh_target: String,
        ssh_binary: PathBuf,
    ) -> io::Result<SshHost> {
        let mut delay = std::time::Duration::from_millis(2);
        for _ in 0..10 {
            match SshHost::connect_with_binary(id.clone(), ssh_target.clone(), ssh_binary.clone()) {
                Err(e) if e.kind() == io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(delay);
                    delay = delay.saturating_mul(2);
                }
                other => return other,
            }
        }
        SshHost::connect_with_binary(id, ssh_target, ssh_binary)
    }

    #[cfg(unix)]
    fn write_executable_mock_ssh(log_path: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        // Bash is available on every supported platform (Linux + macOS
        // per SPEC.md). Each invocation appends `<argv>\n` to the log
        // and exits 0 so the connect succeeds.
        let dir = log_path.parent().expect("log path has parent");
        let mock = dir.join("mock-ssh");
        let script = format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> {}\nexit 0\n",
            log_path.display()
        );
        write_file(&mock, &script);
        let mut perms = fs::metadata(&mock).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&mock, perms).unwrap();
        mock
    }

    #[cfg(unix)]
    fn read_log_lines(log_path: &Path) -> Vec<String> {
        if !log_path.exists() {
            return Vec::new();
        }
        fs::read_to_string(log_path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn ssh_host_lifecycle_invokes_connect_check_drop_with_matching_socket() {
        // Pins four lifecycle contracts in one test. The ETXTBSY
        // race that originally forced the consolidation is now
        // absorbed by `connect_with_binary_retrying_etxtbsy`; the
        // test stays unified because each step builds on the previous
        // (connect → check → drop → inspect the log):
        //
        // 1. Connect uses `-fN -M`, `ControlPersist=600`, and
        //    `ConnectTimeout=5`. Losing any of these breaks the
        //    operational guarantees agent-mux relies on (background
        //    master, idle timeout for the SIGKILL-mid-sleep case,
        //    bounded startup latency for unreachable hosts).
        //
        // 2. Connect runs `ssh -O check` after the `-fN -M` spawn
        //    to confirm the master actually established. Without
        //    this every later `ssh -S <sock>` would silently fall
        //    back to a fresh connection when the master died (e.g.
        //    a ProxyCommand that doesn't survive the fork) — paying
        //    full SSH cost on every poll and violating the
        //    "session switching never blocks on I/O" property.
        //
        // 3. Drop runs `ssh -O exit`. Losing this strands remote
        //    masters for ControlPersist's full 10-minute window
        //    after agent-mux quits.
        //
        // 4. The `-S <path>` argument matches across connect, check,
        //    and teardown. A mismatch makes `-O check` look for the
        //    wrong master (false-positive failures) or `-O exit`
        //    silently no-op against the wrong (or missing) master.
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("ssh-calls.log");
        let mock = write_executable_mock_ssh(&log);

        {
            let _host = connect_with_binary_retrying_etxtbsy(
                HostId("devbox".into()),
                "devbox".into(),
                mock,
            )
            .unwrap();
            // Drop fires at end of scope.
        }

        let lines = read_log_lines(&log);
        assert_eq!(
            lines.len(),
            3,
            "expected connect + check + drop calls, got: {lines:?}"
        );

        // (1) Connect-time invariants.
        let connect = &lines[0];
        assert!(connect.contains("-fN"), "got: {connect}");
        assert!(connect.contains("-M"), "got: {connect}");
        assert!(
            connect.contains("ControlPersist=600"),
            "ControlPersist=600 is the safety net for thread-leak \
             scenarios where Drop never fires; losing it would \
             strand masters forever. got: {connect}"
        );
        assert!(
            connect.contains("ConnectTimeout=5"),
            "ConnectTimeout caps a wedged-host connect — without it \
             a single unreachable host can stall startup. got: {connect}"
        );
        assert!(connect.contains("devbox"), "target missing: {connect}");

        // (2) Post-spawn check.
        let check = &lines[1];
        assert!(
            check.contains("-O") && check.contains("check"),
            "connect must verify the master with `-O check` after \
             the `-fN -M` spawn — `-fN` exits 0 the moment it forks \
             the background master, so without an explicit check a \
             dead master goes undetected and every later operation \
             silently degrades to per-command SSH. got: {check}"
        );
        assert!(
            check.contains("devbox"),
            "check must target the same host. got: {check}"
        );

        // (3) Drop-time invariants.
        let teardown = &lines[2];
        assert!(
            teardown.contains("-O") && teardown.contains("exit"),
            "drop must run `-O exit`. got: {teardown}"
        );
        assert!(
            teardown.contains("devbox"),
            "teardown must target the same host. got: {teardown}"
        );

        // (4) Socket path matches across all three calls.
        let socket_path = control_socket_path(&HostId("devbox".into()));
        let socket_str = socket_path.to_string_lossy().into_owned();
        assert!(
            connect.contains(&socket_str),
            "connect omitted socket path: {connect}"
        );
        assert!(
            check.contains(&socket_str),
            "check omitted socket path: {check}"
        );
        assert!(
            teardown.contains(&socket_str),
            "drop omitted socket path: {teardown}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ssh_host_connect_fails_when_master_does_not_establish_after_spawn() {
        // Regression: pre-2026-05-27 `SshHost::connect` accepted
        // `-fN -M` exit 0 as proof the master was up, but `-fN`
        // returns 0 the moment the parent forks the backgrounded
        // master — it doesn't say whether that backgrounded process
        // then actually opened the control socket. Reproduced with
        // a Coder ProxyCommand: master spawn returned 0, no socket
        // ever appeared, every later `ssh -S <missing-sock>` fell
        // back to a fresh proxy+handshake invisibly.
        //
        // Pin the verify-after-spawn behaviour: when `-O check`
        // exits non-zero, `connect` must surface that as Err.
        // Cleanup `-O exit` runs best-effort — its exit code is
        // not consulted — but we assert it was attempted so a
        // future refactor doesn't quietly drop it.
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("ssh-calls.log");
        let mock = write_executable_mock_ssh_with_broken_master(&log);

        let result =
            connect_with_binary_retrying_etxtbsy(HostId("devbox".into()), "devbox".into(), mock);

        let Err(err) = result else {
            panic!("connect must fail when -O check rejects the master");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("did not establish") && msg.contains("devbox"),
            "error must name the failure mode and the host. got: {msg}"
        );

        let lines = read_log_lines(&log);
        assert_eq!(
            lines.len(),
            3,
            "expected spawn + check + cleanup-exit calls, got: {lines:?}"
        );
        assert!(lines[0].contains("-fN") && lines[0].contains("-M"));
        assert!(
            lines[1].contains("-O") && lines[1].contains("check"),
            "second call must be `-O check`. got: {}",
            lines[1]
        );
        assert!(
            lines[2].contains("-O") && lines[2].contains("exit"),
            "third call must be the best-effort `-O exit` cleanup so \
             a partially-established master isn't leaked when connect \
             fails. got: {}",
            lines[2]
        );
    }

    /// Mock `ssh` that logs argv like [`write_executable_mock_ssh`] but
    /// exits non-zero when invoked with `-O check`, simulating the
    /// ProxyCommand-doesn't-survive-fork failure mode (`-fN -M` returns
    /// 0, but no master is actually listening on the socket). Every
    /// other invocation — including the best-effort `-O exit` cleanup
    /// path — exits 0 so the test only sees `connect` itself fail.
    #[cfg(unix)]
    fn write_executable_mock_ssh_with_broken_master(log_path: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = log_path.parent().expect("log path has parent");
        let mock = dir.join("mock-ssh-broken-master");
        let script = format!(
            "#!/usr/bin/env bash\n\
             printf '%s\\n' \"$*\" >> {}\n\
             prev=\"\"\n\
             for arg in \"$@\"; do\n\
                 if [ \"$prev\" = \"-O\" ] && [ \"$arg\" = \"check\" ]; then\n\
                     exit 1\n\
                 fi\n\
                 prev=\"$arg\"\n\
             done\n\
             exit 0\n",
            log_path.display()
        );
        write_file(&mock, &script);
        let mut perms = fs::metadata(&mock).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&mock, perms).unwrap();
        mock
    }

    /// Mock `ssh` whose `-O check` exits non-zero for the first
    /// `fail_count` invocations and exits 0 thereafter. Lets a test
    /// drive [`SshHost::ensure_connected`] through "probe says dead →
    /// respawn → re-probe says alive" deterministically. The invocation
    /// count lives in `counter_path` (the mock reads/writes it) so it
    /// survives across the separate `ssh` processes `ensure_connected`
    /// spawns. Non-`-O check` calls (the `-fN -M` respawn, the warmup)
    /// are logged and exit 0 without touching the counter.
    #[cfg(unix)]
    fn write_executable_mock_ssh_check_fails_first_n(
        log_path: &Path,
        counter_path: &Path,
        fail_count: u32,
    ) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = log_path.parent().expect("log path has parent");
        let mock = dir.join("mock-ssh-flaky-check");
        let script = format!(
            "#!/usr/bin/env bash\n\
             printf '%s\\n' \"$*\" >> {log}\n\
             prev=\"\"\n\
             for arg in \"$@\"; do\n\
                 if [ \"$prev\" = \"-O\" ] && [ \"$arg\" = \"check\" ]; then\n\
                     n=$(cat {ctr} 2>/dev/null || echo 0)\n\
                     n=$((n+1))\n\
                     echo \"$n\" > {ctr}\n\
                     if [ \"$n\" -le {fail} ]; then exit 1; fi\n\
                     exit 0\n\
                 fi\n\
                 prev=\"$arg\"\n\
             done\n\
             exit 0\n",
            log = log_path.display(),
            ctr = counter_path.display(),
            fail = fail_count,
        );
        write_file(&mock, &script);
        let mut perms = fs::metadata(&mock).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&mock, perms).unwrap();
        mock
    }

    /// Build an [`SshHost`] directly (no `connect`, so no master spawn)
    /// pointed at `ssh_binary`, with `control_path` inside a tempdir so
    /// the reconnect path's `remove_file` can't touch anything real.
    #[cfg(unix)]
    fn ssh_host_with(ssh_binary: PathBuf, control_path: PathBuf) -> SshHost {
        SshHost {
            id: HostId("devbox".into()),
            ssh_target: "devbox".into(),
            control_path,
            ssh_binary,
            reconnect: Mutex::new(ReconnectState::default()),
        }
    }

    /// Exec `mock` once up front, retrying on the ETXTBSY fork/exec race
    /// (see [`connect_with_binary_retrying_etxtbsy`] for the mechanism),
    /// so the `ensure_connected` assertions below run against a "warm"
    /// binary no later exec can find busy. Callers truncate the log
    /// afterward so this warmup invocation doesn't appear in the
    /// recorded argv.
    #[cfg(unix)]
    fn prime_mock_exec(mock: &Path) {
        let mut delay = std::time::Duration::from_millis(2);
        for _ in 0..10 {
            match Command::new(mock).arg("warmup").status() {
                Err(e) if e.kind() == io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(delay);
                    delay = delay.saturating_mul(2);
                }
                _ => return,
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn ssh_host_ensure_connected_is_noop_when_master_is_alive() {
        // The common per-tick case: the master answers `-O check`, so
        // ensure_connected does one cheap probe and reports "nothing to
        // do" without re-spawning. A spurious respawn here would tear
        // down a perfectly good master on every poll.
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("ssh-calls.log");
        let mock = write_executable_mock_ssh(&log); // exits 0 for everything
        prime_mock_exec(&mock);
        fs::write(&log, b"").unwrap();

        let host = ssh_host_with(mock, tmp.path().join("ctrl.sock"));
        let reconnected = host.ensure_connected().expect("probe succeeds");

        assert!(!reconnected, "alive master must not trigger a reconnect");
        let lines = read_log_lines(&log);
        assert_eq!(lines.len(), 1, "expected one `-O check`, got: {lines:?}");
        assert!(
            lines[0].contains("-O") && lines[0].contains("check"),
            "the single call must be the liveness probe. got: {}",
            lines[0]
        );
        assert!(
            !lines.iter().any(|l| l.contains("-fN")),
            "no master respawn when alive. got: {lines:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ssh_host_ensure_connected_respawns_master_after_probe_fails() {
        // The morning-after-sleep case: the master died (probe fails),
        // so ensure_connected clears the stale socket, re-issues the
        // `-fN -M` spawn, and confirms the rebuilt master answers — all
        // off the attach hot path so the next session switch is fast.
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("ssh-calls.log");
        let counter = tmp.path().join("check-count");
        // Fail the first two checks (the fast-path probe + the
        // under-lock double-check), then let the post-respawn check
        // succeed so the rebuilt master verifies.
        let mock = write_executable_mock_ssh_check_fails_first_n(&log, &counter, 2);
        prime_mock_exec(&mock);
        fs::write(&log, b"").unwrap();

        let host = ssh_host_with(mock, tmp.path().join("ctrl.sock"));
        let reconnected = host.ensure_connected().expect("respawn succeeds");

        assert!(reconnected, "dead master must trigger a reconnect");
        let lines = read_log_lines(&log);
        assert!(
            lines.iter().any(|l| l.contains("-fN") && l.contains("-M")),
            "reconnect must re-issue the `-fN -M` master spawn. got: {lines:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ssh_host_ensure_connected_surfaces_error_when_master_cannot_reestablish() {
        // A genuinely-unreachable host: every `-O check` fails, so the
        // respawn's verify also fails. ensure_connected must return Err
        // (which the poller treats as "still down, retry next tick")
        // rather than reporting a phantom success or hanging — and it
        // must have actually attempted the respawn first.
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("ssh-calls.log");
        let mock = write_executable_mock_ssh_with_broken_master(&log);
        prime_mock_exec(&mock);
        fs::write(&log, b"").unwrap();

        let host = ssh_host_with(mock, tmp.path().join("ctrl.sock"));
        let result = host.ensure_connected();

        assert!(
            result.is_err(),
            "a host that never re-establishes must surface Err, got: {result:?}"
        );
        let lines = read_log_lines(&log);
        assert!(
            lines.iter().any(|l| l.contains("-fN") && l.contains("-M")),
            "reconnect must be attempted before giving up. got: {lines:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ssh_host_ensure_connected_backs_off_repeated_respawns() {
        // The storm fix: after a failed re-spawn, an *immediate* second
        // call must NOT re-spawn again — it's inside the backoff window.
        // Without this, an unsustainable master gets a fresh doomed
        // `ssh -fN -M` from every poll tick (and from both pollers).
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("ssh-calls.log");
        let mock = write_executable_mock_ssh_with_broken_master(&log);
        prime_mock_exec(&mock);
        fs::write(&log, b"").unwrap();

        let host = ssh_host_with(mock, tmp.path().join("ctrl.sock"));
        assert!(host.ensure_connected().is_err(), "first attempt fails");
        assert!(
            host.ensure_connected().is_err(),
            "second attempt also reports down"
        );

        let respawns = read_log_lines(&log)
            .iter()
            .filter(|l| l.contains("-fN") && l.contains("-M"))
            .count();
        assert_eq!(
            respawns, 1,
            "the second call within the backoff window must not re-spawn"
        );
    }

    #[test]
    fn local_host_ensure_connected_is_noop() {
        // Local hosts have no connection to heal; the trait default must
        // report "already connected" so the shared poller loop doesn't
        // special-case host kind.
        assert!(!LocalHost::new().ensure_connected().unwrap());
    }

    fn devbox() -> SshHost {
        SshHost {
            id: HostId("devbox".into()),
            ssh_target: "devbox".into(),
            control_path: PathBuf::from("/tmp/sock"),
            ssh_binary: PathBuf::from("ssh"),
            reconnect: Mutex::new(ReconnectState::default()),
        }
    }

    #[test]
    fn ssh_argv_builds_prefix_without_tty_for_capture() {
        // No -t before the target: capturing list-panes output doesn't
        // need a tty allocation and asking for one wastes bytes on the
        // wire. The trailing command lands as one shell-quoted argv
        // element (SSH joins post-target args with space and the remote
        // shell re-tokenizes — quoting protects against word-splitting,
        // glob expansion, and the leading-`#` comment-eating).
        let argv = devbox()
            .ssh_argv(false, &["tmux", "list-panes", "-a"])
            .expect("remote");
        assert_eq!(
            argv,
            vec![
                "ssh",
                "-S",
                "/tmp/sock",
                "devbox",
                "'tmux' 'list-panes' '-a'"
            ]
        );
    }

    #[test]
    fn ssh_argv_inserts_tty_flag_for_interactive_use() {
        // -t goes *before* the target so `ssh -t target cmd` is the
        // shape OpenSSH expects.
        let argv = devbox()
            .ssh_argv(true, &["tmux", "attach", "-t", "main:0.0"])
            .expect("remote");
        assert_eq!(
            argv,
            vec![
                "ssh",
                "-S",
                "/tmp/sock",
                "-t",
                "devbox",
                "'tmux' 'attach' '-t' 'main:0.0'"
            ]
        );
    }

    #[test]
    fn ssh_argv_with_empty_remote_cmd_omits_trailing_command_arg() {
        let argv = devbox().ssh_argv(false, &[]).expect("remote");
        assert_eq!(argv, vec!["ssh", "-S", "/tmp/sock", "devbox"]);
    }

    #[test]
    fn ssh_argv_quotes_format_string_so_remote_shell_does_not_treat_hash_as_comment() {
        // Regression: tmux's `-F` format strings start with `#{...}`.
        // Without quoting, the remote shell sees `tmux ... -F #{...}`
        // and treats everything from `#` to EOL as a comment, so tmux
        // gets an empty `-F` argument and fails. Single-quoting the
        // format string preserves it verbatim through the remote sh
        // tokenizer.
        let argv = devbox()
            .ssh_argv(false, &["tmux", "list-panes", "-F", "#{session_name}"])
            .expect("remote");
        let cmd = argv.last().expect("trailing cmd arg");
        assert!(cmd.contains("'#{session_name}'"), "got: {cmd}");
        // Belt-and-braces: no bare `#` anywhere outside its quotes.
        let unquoted: String = cmd
            .split('\'')
            .step_by(2) // only the bits *between* quotes are unquoted
            .collect();
        assert!(!unquoted.contains('#'), "unquoted # in: {cmd}");
    }

    #[test]
    fn ssh_argv_preserves_multi_word_command_strings_as_single_tmux_arg() {
        // Regression: `tmux new-session ... 'claude --resume <id>'`
        // must land on the remote as five tmux argv elements, with
        // the last being a single quoted string. Without quoting,
        // SSH's space-joining splits "claude --resume <id>" into
        // three tokens and tmux treats `--resume` as a flag of its
        // own.
        let argv = devbox()
            .ssh_argv(
                true,
                &[
                    "tmux",
                    "new-session",
                    "-c",
                    "/work",
                    "claude --resume abc-123",
                ],
            )
            .expect("remote");
        let cmd = argv.last().expect("trailing cmd arg");
        assert!(cmd.contains("'claude --resume abc-123'"), "got: {cmd}");
    }

    // ---- shell quoting primitives ----

    #[test]
    fn shell_join_quoted_quotes_each_element() {
        let got = super::shell_join_quoted(["a", "b c", "d"]);
        assert_eq!(got, "'a' 'b c' 'd'");
    }

    #[test]
    fn shell_join_quoted_escapes_embedded_single_quotes() {
        // POSIX idiom: 'it'\''s' parses back to it's.
        let got = super::shell_join_quoted(["it's"]);
        assert_eq!(got, "'it'\\''s'");
    }

    #[test]
    fn shell_join_quoted_on_empty_input_returns_empty_string() {
        let got = super::shell_join_quoted::<_, &str>([]);
        assert_eq!(got, "");
    }

    #[test]
    fn control_socket_path_includes_host_id_and_pid() {
        let path = super::control_socket_path(&HostId("devbox".into()));
        let name = path.file_name().expect("filename").to_string_lossy();
        let pid = std::process::id().to_string();
        assert!(name.contains("devbox"), "{name}");
        assert!(name.contains(&pid), "{name}");
        assert!(name.ends_with(".sock"), "{name}");
    }

    #[test]
    fn local_read_many_returns_per_path_results_in_order() {
        // Mix of present + missing files: ordering must match input,
        // and the missing file's slot must surface as an Err so
        // discovery's task.toml lookups can dispatch on NotFound vs
        // transport failure.
        let tmp = TempDir::new().expect("tempdir");
        let a = tmp.path().join("a.txt");
        let c = tmp.path().join("c.txt");
        write_file(&a, "alpha");
        write_file(&c, "gamma");
        let missing = tmp.path().join("b.txt");

        let host = LocalHost::new();
        let results = host
            .read_many(&[a.as_path(), missing.as_path(), c.as_path()])
            .expect("batch ok");
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].as_deref().ok(), Some("alpha"));
        assert!(results[1].is_err(), "missing slot should be Err");
        assert_eq!(results[2].as_deref().ok(), Some("gamma"));
    }

    #[test]
    fn local_is_dir_many_returns_per_path_bools_in_order() {
        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path().join("d");
        fs::create_dir(&dir).expect("mkdir");
        let file = tmp.path().join("f.txt");
        write_file(&file, "x");
        let missing = tmp.path().join("nope");

        let host = LocalHost::new();
        let results = host
            .is_dir_many(&[dir.as_path(), file.as_path(), missing.as_path()])
            .expect("batch ok");
        assert_eq!(results, vec![true, false, false]);
    }

    #[test]
    fn parse_read_many_output_decodes_ok_and_miss_records_in_order() {
        // Two existing files (with binary-ish + UTF-8 content) plus
        // one missing file, in mixed order. Parser tracks position by
        // byte count so a file whose content lacks a trailing newline
        // round-trips correctly — the next header starts immediately
        // after the declared size.
        let mut wire: Vec<u8> = Vec::new();
        wire.extend_from_slice(b"OK 5\n");
        wire.extend_from_slice(b"alpha");
        wire.extend_from_slice(b"MISS\n");
        wire.extend_from_slice(b"OK 11\n");
        wire.extend_from_slice("hello world".as_bytes());

        let parsed = super::parse_read_many_output(&wire, 3).expect("parse");
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].as_deref().ok(), Some("alpha"));
        assert_eq!(
            parsed[1].as_ref().err().map(io::Error::kind),
            Some(io::ErrorKind::NotFound),
        );
        assert_eq!(parsed[2].as_deref().ok(), Some("hello world"));
    }

    #[test]
    fn parse_read_many_output_rejects_truncated_content() {
        // Declared 100 bytes but only 5 actual bytes follow: the
        // parser should error rather than silently return a short
        // string. Pin the failure mode so a remote shell that dies
        // mid-cat doesn't masquerade as a valid (partial) result.
        let mut wire: Vec<u8> = Vec::new();
        wire.extend_from_slice(b"OK 100\n");
        wire.extend_from_slice(b"short");

        let err = super::parse_read_many_output(&wire, 1).expect_err("should error");
        assert!(err.to_string().contains("short read"), "got: {err}");
    }

    #[test]
    fn parse_read_many_output_rejects_bad_header() {
        let wire = b"GARBAGE\n";
        let err = super::parse_read_many_output(wire, 1).expect_err("should error");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    // ---- Host::run / Host::write_file ----

    #[test]
    fn local_run_returns_full_output_with_status_and_stdout() {
        // `echo` is on PATH on every supported platform. Pin: the
        // returned `Output` carries the bytes the child wrote, and a
        // `status.success()` matching the exit code. Caller dispatches
        // on status — the trait doesn't fold non-zero into Err.
        let host = LocalHost::new();
        let out = host
            .run(None, "echo", &["hello", "world"])
            .expect("echo runs");
        assert!(out.status.success(), "echo should exit 0");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("hello world"), "got: {stdout:?}");
    }

    #[test]
    fn local_run_honours_cwd() {
        // The cwd parameter must actually scope the spawned process —
        // a future refactor that drops `Command::current_dir` would
        // silently keep working in test if cwd were ignored, so pin it.
        let tmp = TempDir::new().expect("tempdir");
        let host = LocalHost::new();
        let out = host.run(Some(tmp.path()), "pwd", &[]).expect("pwd runs");
        let stdout = String::from_utf8_lossy(&out.stdout);
        // macOS resolves /var/folders via a symlink to /private/var/...,
        // so canonicalise both sides before comparison.
        let want = std::fs::canonicalize(tmp.path()).expect("canon want");
        let got = std::fs::canonicalize(stdout.trim()).expect("canon got");
        assert_eq!(got, want, "raw stdout: {stdout:?}");
    }

    #[test]
    fn local_run_propagates_io_error_for_nonexistent_program() {
        // Transport-level failure: program not on PATH. Distinct from
        // "program ran and exited non-zero" — the trait surfaces this
        // as Err so callers can tell "couldn't even start" from "ran
        // and reported a problem".
        let host = LocalHost::new();
        let err = host
            .run(None, "this-program-deliberately-does-not-exist", &[])
            .expect_err("should error");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn local_write_file_creates_new_file() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("note.toml");
        let host = LocalHost::new();
        host.write_file(&path, "task = \"hello\"\n").expect("write");
        assert_eq!(
            fs::read_to_string(&path).expect("readback"),
            "task = \"hello\"\n"
        );
    }

    #[test]
    fn local_write_file_overwrites_existing() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("note.toml");
        write_file(&path, "old content");
        let host = LocalHost::new();
        host.write_file(&path, "new content").expect("write");
        assert_eq!(fs::read_to_string(&path).expect("readback"), "new content");
    }

    #[cfg(unix)]
    #[test]
    fn ssh_run_quotes_program_and_args_for_remote_shell() {
        // SSH joins post-target args with spaces; the remote shell
        // re-tokenises. The trait quotes each token via shell_join_quoted
        // so paths with spaces / globs / leading-`#` (the `-F #{format}`
        // case that bit `ssh_argv`) round-trip verbatim. Pin both the
        // command shape and the per-token quoting.
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("ssh-calls.log");
        let mock = write_executable_mock_ssh(&log);
        let host =
            connect_with_binary_retrying_etxtbsy(HostId("devbox".into()), "devbox".into(), mock)
                .unwrap();

        host.run(None, "git", &["status", "--short"]).expect("run");

        let lines = read_log_lines(&log);
        // Line 0 is connect (-fN -M …); line 1 is our run().
        let invoked = lines.last().expect("at least one log line");
        assert!(
            invoked.contains("'git' 'status' '--short'"),
            "got: {invoked}"
        );
        assert!(invoked.contains("devbox"), "target missing: {invoked}");
    }

    #[cfg(unix)]
    #[test]
    fn ssh_run_with_cwd_prefixes_cd_to_remote_command() {
        // The cwd parameter routes through the *remote* shell via a
        // `cd <quoted cwd> && ` prefix — Command::current_dir doesn't
        // help here because it scopes the local `ssh` process, not the
        // remote shell. Tilde paths stay unquoted so the remote home
        // expands; non-tilde paths get single-quoted whole.
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("ssh-calls.log");
        let mock = write_executable_mock_ssh(&log);
        let host =
            connect_with_binary_retrying_etxtbsy(HostId("devbox".into()), "devbox".into(), mock)
                .unwrap();

        host.run(Some(Path::new("/srv/work/repo")), "git", &["status"])
            .expect("run");

        let invoked = read_log_lines(&log).last().cloned().expect("log");
        assert!(
            invoked.contains("cd '/srv/work/repo' && 'git' 'status'"),
            "got: {invoked}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ssh_run_preserves_remote_tilde_in_args_so_workspace_scan_finds_repos() {
        // Regression: scan_host_workspaces builds `find ~/workspace
        // -mindepth 1 …` via Host::run(None, "find", &[folder, …]).
        // When per-arg quoting fully single-quoted every token, the
        // remote shell saw `find '~/workspace' …` — POSIX `~` only
        // expands at the start of an *unquoted* word, so find exited
        // non-zero and the remote repo picker showed nothing. Pin the
        // tilde-preserving shape on a `find`-style call so the bug
        // can't silently come back.
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("ssh-calls.log");
        let mock = write_executable_mock_ssh(&log);
        let host =
            connect_with_binary_retrying_etxtbsy(HostId("devbox".into()), "devbox".into(), mock)
                .unwrap();

        host.run(
            None,
            "find",
            &[
                "~/workspace",
                "-mindepth",
                "1",
                "-maxdepth",
                "1",
                "-type",
                "d",
            ],
        )
        .expect("run");

        let invoked = read_log_lines(&log).last().cloned().expect("log");
        assert!(
            invoked.contains("'find' ~/'workspace' '-mindepth' '1' '-maxdepth' '1' '-type' 'd'"),
            "tilde-prefixed arg must stay unquoted at the `~/` so the \
             remote shell expands it. got: {invoked}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ssh_run_preserves_remote_tilde_for_home_expansion() {
        // `~/work` must reach the remote shell *unquoted* at the
        // leading `~/` so the remote user's home expands. Anything
        // beyond the prefix is single-quoted as usual. Same shape
        // `transcript_root = "~/.claude/projects"` relies on.
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("ssh-calls.log");
        let mock = write_executable_mock_ssh(&log);
        let host =
            connect_with_binary_retrying_etxtbsy(HostId("devbox".into()), "devbox".into(), mock)
                .unwrap();

        host.run(Some(Path::new("~/work/repo")), "pwd", &[])
            .expect("run");

        let invoked = read_log_lines(&log).last().cloned().expect("log");
        assert!(
            invoked.contains("cd ~/'work/repo' && 'pwd'"),
            "got: {invoked}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ssh_write_file_pipes_content_to_remote_cat() {
        // Verifies two things end-to-end: (1) the remote command is
        // `cat > <quoted path>` and (2) stdin content reaches the
        // remote `cat` verbatim. Uses a tailored mock that reads
        // stdin and writes it to a content-log alongside the
        // argv-log, then asserts on both.
        let tmp = TempDir::new().unwrap();
        let argv_log = tmp.path().join("ssh-calls.log");
        let stdin_log = tmp.path().join("ssh-stdin.bin");
        let mock = write_executable_mock_ssh_capturing_stdin(&argv_log, &stdin_log);
        let host =
            connect_with_binary_retrying_etxtbsy(HostId("devbox".into()), "devbox".into(), mock)
                .unwrap();

        let payload = "task = \"refactor parser\"\nbase_branch = \"main\"\n";
        host.write_file(Path::new("/srv/work/repo/.agent-mux/task.toml"), payload)
            .expect("write_file");

        let invoked = read_log_lines(&argv_log).last().cloned().expect("argv log");
        assert!(
            invoked.contains("cat > '/srv/work/repo/.agent-mux/task.toml'"),
            "got: {invoked}"
        );
        let stdin_captured = fs::read_to_string(&stdin_log).expect("stdin readback");
        assert_eq!(stdin_captured, payload);
    }

    /// Mock `ssh` that, in addition to logging argv (matching
    /// [`write_executable_mock_ssh`]), drains its stdin into
    /// `stdin_log`. Used by `ssh_write_file_pipes_content_to_remote_cat`
    /// to verify the body of a `Host::write_file` call actually
    /// reaches the remote shell — the argv alone says nothing about
    /// content.
    #[cfg(unix)]
    fn write_executable_mock_ssh_capturing_stdin(argv_log: &Path, stdin_log: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = argv_log.parent().expect("log path has parent");
        let mock = dir.join("mock-ssh-with-stdin");
        let script = format!(
            "#!/usr/bin/env bash\n\
             printf '%s\\n' \"$*\" >> {}\n\
             cat > {}\n\
             exit 0\n",
            argv_log.display(),
            stdin_log.display(),
        );
        write_file(&mock, &script);
        let mut perms = fs::metadata(&mock).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&mock, perms).unwrap();
        mock
    }
}
