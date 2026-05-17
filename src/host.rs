use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
}

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
        let status = Command::new(&ssh_binary)
            .arg("-fN")
            .arg("-M")
            .arg("-S")
            .arg(&control_path)
            .arg("-o")
            .arg("ControlPersist=600")
            .arg("-o")
            .arg("ConnectTimeout=5")
            .arg(&ssh_target)
            .status()?;
        if !status.success() {
            return Err(io::Error::other(format!(
                "ssh ControlMaster setup failed for {ssh_target} (exit {status})"
            )));
        }
        Ok(Self {
            id,
            ssh_target,
            control_path,
            ssh_binary,
        })
    }

    fn ssh_command(&self) -> Command {
        let mut cmd = Command::new(&self.ssh_binary);
        cmd.arg("-S").arg(&self.control_path);
        cmd.arg(&self.ssh_target);
        cmd
    }

    fn run(&self, remote_cmd: &str) -> io::Result<Vec<u8>> {
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
        let stdout = self.run(&cmd)?;
        parse_find_output(&stdout)
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        let quoted = shell_quote_path(&path.to_string_lossy());
        let stdout = self.run(&format!("cat {quoted}"))?;
        String::from_utf8(stdout).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    fn read_tail(&self, path: &Path, n_bytes: u64) -> io::Result<String> {
        let quoted = shell_quote_path(&path.to_string_lossy());
        let stdout = self.run(&format!("tail -c {n_bytes} {quoted}"))?;
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
fn shell_single_quote(s: &str) -> String {
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
    fn control_socket_path_includes_host_id_and_pid() {
        let path = super::control_socket_path(&HostId("devbox".into()));
        let name = path.file_name().expect("filename").to_string_lossy();
        let pid = std::process::id().to_string();
        assert!(name.contains("devbox"), "{name}");
        assert!(name.contains(&pid), "{name}");
        assert!(name.ends_with(".sock"), "{name}");
    }
}
