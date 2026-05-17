use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use notify::{EventKind, RecursiveMode, Watcher};

use crate::attachment::list_live_pane_cwds;
use crate::host::Host;
use crate::session::{Attention, HostId, SessionId};

/// How much of the transcript's tail to read when deriving attention.
/// Transcripts are append-only JSONL; reading the last few KB is enough
/// to find the most recent meaningful entry without parsing the whole file.
const TAIL_BYTES: u64 = 32 * 1024;

/// Default interval between remote transcript polls. Low enough that
/// needs-input feels live (a few seconds end-to-end), high enough that
/// an idle ten-session host isn't generating continuous SSH round-trips.
/// Becomes configurable in M4.
pub const REMOTE_POLL_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Debug)]
pub struct AttentionUpdate {
    pub id: SessionId,
    pub attention: Attention,
}

/// Events emitted by the watcher. `Attention` flows from filesystem events
/// against transcripts already registered with the watcher. `NewTranscript`
/// flows when a previously-unknown `.jsonl` appears under the discovery
/// root, so the dashboard can pull it into the catalog without a restart.
/// The `host` tag lets the dashboard route the path through the right
/// `Host` impl when it builds the session — local transcripts come from
/// the recursive `notify` watch, remote transcripts from a per-host
/// polling thread. `mtime` is the file's last-modified time at discovery
/// (carried in-event so the dashboard does not need filesystem access to
/// stat a path on a remote host).
#[derive(Debug)]
pub enum WatcherEvent {
    Attention(AttentionUpdate),
    NewTranscript {
        host: HostId,
        path: PathBuf,
        mtime: SystemTime,
    },
    /// Snapshot of every live tmux pane's `pane_current_path` on
    /// `host`, as of the latest pane-poll tick. The catalog uses
    /// this to decide per-session whether Enter will be a fast
    /// switch (a pane matches the session's `project_dir`) vs an
    /// auto-resume (no match — fall through to `claude --resume`).
    /// An empty `cwds` is a valid value (it means no live panes /
    /// no tmux server / ssh hiccup) — every session on the host
    /// transitions to `Some(false)` in that case.
    LivePanes {
        host: HostId,
        cwds: Vec<PathBuf>,
    },
}

pub struct TranscriptWatcher {
    /// Kept alive for the lifetime of the dashboard; dropping it tears
    /// the notify backend down. We also call `.watch` on it from
    /// `add_target` in the no-recursive-root fallback path.
    watcher: notify::RecommendedWatcher,
    targets: Arc<Mutex<HashMap<PathBuf, SessionId>>>,
    event_tx: Sender<WatcherEvent>,
    host: Arc<dyn Host>,
    /// True when a recursive watch on the projects root is active. When
    /// false, `add_target` falls back to per-file watches so the watcher
    /// still works in the degenerate no-discovery-root case.
    has_recursive_root: bool,
}

impl TranscriptWatcher {
    /// Start watching for transcript events.
    ///
    /// When `discovery_root` is `Some` and watchable, a single recursive
    /// watch covers every transcript under it — both attention updates
    /// for known files and discovery of new ones. When it is `None`, or
    /// the recursive watch fails (e.g. the dir is on a filesystem that
    /// can't be recursively watched), the watcher falls back to per-file
    /// watches for the `initial` set and `NewTranscript` events will not
    /// fire.
    ///
    /// Emits an initial `Attention` update for each session in `initial`
    /// synchronously before the watcher thread starts, so the UI never
    /// has to display `Unknown` for a session that has on-disk content.
    ///
    /// `host` is the [`Host`] backing this watcher's transcript reads —
    /// always [`crate::host::LocalHost`] in practice, because this entry
    /// point sets up the `notify`-based recursive watch that only makes
    /// sense for the local filesystem. Remote hosts plug in afterwards
    /// via [`Self::start_polling_host`], each with their own background
    /// polling thread.
    ///
    /// # Errors
    /// Returns `notify::Error` if the platform watcher cannot be created
    /// or, in the per-file fallback, if any of the initial paths cannot
    /// be watched.
    pub fn start(
        host: Arc<dyn Host>,
        initial: Vec<(SessionId, PathBuf)>,
        discovery_root: Option<&Path>,
    ) -> notify::Result<(Self, Receiver<WatcherEvent>)> {
        let (event_tx, event_rx) = mpsc::channel::<WatcherEvent>();
        let (notify_tx, notify_rx) = mpsc::channel();

        let mut watcher = notify::recommended_watcher(notify_tx)?;

        let has_recursive_root = match discovery_root {
            Some(root) => {
                // Ensure the dir exists so the watch attaches on first-run
                // (claude code would create it on its own eventually, but
                // we'd miss the discovery window).
                let _ = fs::create_dir_all(root);
                watcher.watch(root, RecursiveMode::Recursive).is_ok()
            }
            None => false,
        };

        if !has_recursive_root {
            for (_, path) in &initial {
                watcher.watch(path, RecursiveMode::NonRecursive)?;
            }
        }

        // Prime initial state so the UI shows real attention from frame one.
        for (id, path) in &initial {
            let _ = event_tx.send(WatcherEvent::Attention(AttentionUpdate {
                id: id.clone(),
                attention: derive_attention(host.as_ref(), path),
            }));
        }

        let targets: Arc<Mutex<HashMap<PathBuf, SessionId>>> = Arc::new(Mutex::new(
            initial.into_iter().map(|(id, p)| (p, id)).collect(),
        ));

        let targets_for_thread = Arc::clone(&targets);
        let event_tx_for_thread = event_tx.clone();
        let host_for_thread = Arc::clone(&host);
        thread::spawn(move || {
            for res in notify_rx {
                let Ok(event) = res else { continue };
                let is_create = matches!(event.kind, EventKind::Create(_));
                let is_modify = matches!(event.kind, EventKind::Modify(_));
                if !is_create && !is_modify {
                    continue;
                }
                for path in event.paths {
                    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                        continue;
                    }
                    let known_id = targets_for_thread
                        .lock()
                        .ok()
                        .and_then(|m| m.get(&path).cloned());
                    let outgoing = if let Some(id) = known_id {
                        WatcherEvent::Attention(AttentionUpdate {
                            id,
                            attention: derive_attention(host_for_thread.as_ref(), &path),
                        })
                    } else {
                        // Stat now so the main thread doesn't have to;
                        // the file may have vanished between event and
                        // stat, in which case we drop and let the next
                        // event re-trigger.
                        let Ok(mtime) = fs::metadata(&path).and_then(|m| m.modified()) else {
                            continue;
                        };
                        WatcherEvent::NewTranscript {
                            host: HostId::local(),
                            path,
                            mtime,
                        }
                    };
                    if event_tx_for_thread.send(outgoing).is_err() {
                        return;
                    }
                }
            }
        });

        Ok((
            Self {
                watcher,
                targets,
                event_tx,
                host,
                has_recursive_root,
            },
            event_rx,
        ))
    }

    /// Register a newly-discovered transcript so future filesystem events
    /// against it become `Attention` updates rather than further
    /// `NewTranscript` emissions. Also emits an immediate initial
    /// `Attention` update so the dashboard reflects the session's current
    /// state without waiting for the next file write.
    ///
    /// Local-only: remote polling threads own their target set.
    ///
    /// # Errors
    /// In the no-recursive-root fallback path, returns `notify::Error` if
    /// the per-file watch cannot be installed. With a recursive root in
    /// place, this never fails.
    pub fn add_target(&mut self, id: SessionId, path: PathBuf) -> notify::Result<()> {
        if !self.has_recursive_root {
            self.watcher.watch(&path, RecursiveMode::NonRecursive)?;
        }
        let attention = derive_attention(self.host.as_ref(), &path);
        if let Ok(mut targets) = self.targets.lock() {
            targets.insert(path, id.clone());
        }
        let _ = self
            .event_tx
            .send(WatcherEvent::Attention(AttentionUpdate { id, attention }));
        Ok(())
    }

    /// Register a `NewTranscript`-discovered session with the watcher.
    /// For the local recursive watch this forwards to [`Self::add_target`]
    /// so subsequent filesystem events route as `Attention`. For hosts
    /// driven by a polling thread this is a no-op because the poll loop
    /// already owns the path → session-id mapping for its host.
    ///
    /// # Errors
    /// Propagates [`Self::add_target`]'s error in the local case.
    pub fn track_new_transcript(
        &mut self,
        host: &HostId,
        id: SessionId,
        path: PathBuf,
    ) -> notify::Result<()> {
        if host.is_local() {
            self.add_target(id, path)
        } else {
            Ok(())
        }
    }

    /// Spawn a background thread that polls `host` for transcript
    /// changes at `interval`. Used for hosts whose filesystem changes
    /// can't be observed via `notify` (SSH targets). The thread emits
    /// the same `WatcherEvent`s the local pipeline does, so the
    /// catalog update path stays unchanged.
    ///
    /// `initial` seeds the per-host known-set with sessions already
    /// discovered against this host: each entry's `SystemTime` anchors
    /// the first mtime comparison, so a transcript that grew between
    /// discovery and the first poll tick still surfaces a live
    /// `Attention` update.
    ///
    /// The thread terminates cleanly when the event receiver drops
    /// (i.e. the dashboard exits). No explicit shutdown handle.
    ///
    /// **Lifecycle note (see ARCHITECTURE.md / Process lifecycle):**
    /// this thread holds a clone of `Arc<dyn Host>`. If the process
    /// exits while the thread is mid-`thread::sleep`, the runtime
    /// kills the thread before its Arc reference drops — meaning
    /// `SshHost::Drop` does not run for that host. The remote
    /// `ControlMaster` is then reaped by `ControlPersist=600` (10 min)
    /// rather than the explicit `ssh -O exit`. That's the intended
    /// safety net; do not assume Drop fires on every shutdown path.
    pub fn start_polling_host(
        &self,
        host: Arc<dyn Host>,
        root: PathBuf,
        initial: Vec<(SessionId, PathBuf, SystemTime)>,
        interval: Duration,
    ) {
        let tx = self.event_tx.clone();
        thread::spawn(move || {
            let host_id = host.id().clone();
            let mut known: HashMap<PathBuf, (SessionId, SystemTime)> = initial
                .into_iter()
                .map(|(id, path, mtime)| (path, (id, mtime)))
                .collect();
            loop {
                thread::sleep(interval);
                if !poll_once(host.as_ref(), &host_id, &root, &mut known, &tx) {
                    return;
                }
            }
        });
    }

    /// Spawn a background thread that polls `host` for the set of live
    /// tmux panes (their `pane_current_path`) at `interval`, and emits
    /// one [`WatcherEvent::LivePanes`] per tick. Used for both local
    /// and remote hosts — the local host's pane state isn't observable
    /// via `notify`, and shelling out tmux every 3s on every keypress
    /// would violate the "switching never blocks on I/O" discipline.
    ///
    /// The first tick fires immediately (before the first sleep) so
    /// the dashboard learns initial pane state on first paint rather
    /// than waiting one interval. Subsequent ticks pace at `interval`.
    /// Errors (no tmux server, ssh failure) surface as an empty
    /// `cwds` list, which the catalog interprets as "every session on
    /// this host has no live pane".
    ///
    /// Thread terminates cleanly when the event receiver drops.
    /// Same SIGKILL/mid-sleep caveat as [`Self::start_polling_host`]:
    /// the `Arc<dyn Host>` clone held by this thread may not be
    /// released on a hard process exit, so `SshHost::Drop` is
    /// best-effort and `ControlPersist` is the safety net.
    pub fn start_pane_polling_host(&self, host: Arc<dyn Host>, interval: Duration) {
        let tx = self.event_tx.clone();
        thread::spawn(move || {
            let host_id = host.id().clone();
            loop {
                let cwds = list_live_pane_cwds(host.as_ref());
                if tx
                    .send(WatcherEvent::LivePanes {
                        host: host_id.clone(),
                        cwds,
                    })
                    .is_err()
                {
                    return;
                }
                thread::sleep(interval);
            }
        });
    }
}

/// One tick of the remote poller. Returns `false` iff the receiver
/// dropped (the dashboard exited), at which point the caller exits
/// the loop. Errors from `list_transcripts` are swallowed — a
/// transient SSH hiccup must not stop the polling cadence, and the
/// next tick retries from scratch.
fn poll_once(
    host: &dyn Host,
    host_id: &HostId,
    root: &Path,
    known: &mut HashMap<PathBuf, (SessionId, SystemTime)>,
    tx: &Sender<WatcherEvent>,
) -> bool {
    let Ok(stats) = host.list_transcripts(root) else {
        return true;
    };
    for stat in stats {
        if let Some((id, last_seen)) = known.get_mut(&stat.path) {
            // mtime-skip: the only way a transcript's derived attention
            // changes is via a write to it, and a write advances mtime.
            // Skipping unchanged files keeps the cost of an idle host
            // at one `find` per interval rather than N `tail -c`.
            if stat.mtime > *last_seen {
                *last_seen = stat.mtime;
                let attention = derive_attention(host, &stat.path);
                if tx
                    .send(WatcherEvent::Attention(AttentionUpdate {
                        id: id.clone(),
                        attention,
                    }))
                    .is_err()
                {
                    return false;
                }
            }
            continue;
        }
        let Some(stem) = stat.path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let id = SessionId(stem.to_string());
        known.insert(stat.path.clone(), (id.clone(), stat.mtime));
        if tx
            .send(WatcherEvent::NewTranscript {
                host: host_id.clone(),
                path: stat.path.clone(),
                mtime: stat.mtime,
            })
            .is_err()
        {
            return false;
        }
        // The dashboard would otherwise see this row sit at `Unknown`
        // until the *next* write — for a session that's been idle for
        // hours that could be a long time. Emit one Attention now.
        let attention = derive_attention(host, &stat.path);
        if tx
            .send(WatcherEvent::Attention(AttentionUpdate { id, attention }))
            .is_err()
        {
            return false;
        }
    }
    true
}

/// Derive an attention state from the most recent meaningful JSONL entry in
/// `transcript_path`. Reads only the last `TAIL_BYTES` of the file through
/// `host`; the (possibly truncated) first line is discarded by virtue of
/// failing to parse, and the remaining lines are walked to find the latest
/// conversational entry.
#[must_use]
pub fn derive_attention(host: &dyn Host, transcript_path: &Path) -> Attention {
    let Ok(tail) = host.read_tail(transcript_path, TAIL_BYTES) else {
        return Attention::Unknown;
    };

    let mut last: Option<EntryKind> = None;
    for line in tail.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(kind) = classify(&value) {
            last = Some(kind);
        }
    }

    match last {
        Some(EntryKind::Assistant) => Attention::NeedsInput,
        Some(EntryKind::UserMessage | EntryKind::ToolResult) => Attention::Working,
        None => Attention::Unknown,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Assistant,
    UserMessage,
    ToolResult,
}

fn classify(value: &serde_json::Value) -> Option<EntryKind> {
    let entry_type = value.get("type")?.as_str()?;
    match entry_type {
        "assistant" => Some(EntryKind::Assistant),
        "user" => {
            if value.get("toolUseResult").is_some() {
                Some(EntryKind::ToolResult)
            } else {
                Some(EntryKind::UserMessage)
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::LocalHost;

    fn host() -> LocalHost {
        LocalHost::new()
    }

    fn write_jsonl(lines: &[&str]) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        let content = lines.join("\n") + "\n";
        fs::write(f.path(), content).unwrap();
        f
    }

    #[test]
    fn empty_file_is_unknown() {
        let f = tempfile::NamedTempFile::new().unwrap();
        assert_eq!(derive_attention(&host(), f.path()), Attention::Unknown);
    }

    #[test]
    fn only_housekeeping_entries_is_unknown() {
        let f = write_jsonl(&[
            r#"{"type":"permission-mode","permissionMode":"default"}"#,
            r#"{"type":"file-history-snapshot"}"#,
            r#"{"type":"ai-title","title":"x"}"#,
        ]);
        assert_eq!(derive_attention(&host(), f.path()), Attention::Unknown);
    }

    #[test]
    fn last_assistant_means_needs_input() {
        let f = write_jsonl(&[
            r#"{"type":"user","message":"hi"}"#,
            r#"{"type":"assistant","message":"hello"}"#,
        ]);
        assert_eq!(derive_attention(&host(), f.path()), Attention::NeedsInput);
    }

    #[test]
    fn last_user_message_means_working() {
        let f = write_jsonl(&[
            r#"{"type":"assistant","message":"hello"}"#,
            r#"{"type":"user","message":"do thing"}"#,
        ]);
        assert_eq!(derive_attention(&host(), f.path()), Attention::Working);
    }

    #[test]
    fn last_tool_result_means_working() {
        let f = write_jsonl(&[
            r#"{"type":"assistant","message":"running tool"}"#,
            r#"{"type":"user","toolUseResult":{"stdout":"ok"}}"#,
        ]);
        assert_eq!(derive_attention(&host(), f.path()), Attention::Working);
    }

    #[test]
    fn housekeeping_after_assistant_does_not_change_state() {
        let f = write_jsonl(&[
            r#"{"type":"user","message":"hi"}"#,
            r#"{"type":"assistant","message":"hello"}"#,
            r#"{"type":"file-history-snapshot"}"#,
            r#"{"type":"ai-title","title":"x"}"#,
        ]);
        assert_eq!(derive_attention(&host(), f.path()), Attention::NeedsInput);
    }

    #[test]
    fn nonexistent_file_is_unknown() {
        let path = std::path::PathBuf::from("/nonexistent/path/foo.jsonl");
        assert_eq!(derive_attention(&host(), &path), Attention::Unknown);
    }

    // ---- poll_once ----
    //
    // The remote-host polling path runs `poll_once` once per interval.
    // The unit tests below cover its event-emission contract using an
    // in-memory `MockHost` so mtimes are deterministic — sleep-based
    // tests against a real filesystem are flaky on fast disks.

    use crate::host::TranscriptStat;
    use std::io;
    use std::sync::Mutex;
    use std::time::{Duration, UNIX_EPOCH};

    /// In-memory `Host` whose `list_transcripts` and `read_tail` are
    /// driven entirely by the test. Avoids depending on filesystem
    /// mtime resolution.
    struct MockHost {
        id: HostId,
        files: Mutex<HashMap<PathBuf, (String, SystemTime)>>,
    }

    impl MockHost {
        fn new(label: &str) -> Self {
            Self {
                id: HostId(label.to_string()),
                files: Mutex::new(HashMap::new()),
            }
        }

        fn put(&self, path: &Path, content: &str, mtime: SystemTime) {
            self.files
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), (content.to_string(), mtime));
        }
    }

    impl Host for MockHost {
        fn id(&self) -> &HostId {
            &self.id
        }

        fn list_transcripts(&self, _root: &Path) -> io::Result<Vec<TranscriptStat>> {
            let mut out: Vec<_> = self
                .files
                .lock()
                .unwrap()
                .iter()
                .map(|(p, (_, m))| TranscriptStat {
                    path: p.clone(),
                    mtime: *m,
                })
                .collect();
            // Stable ordering so test event-stream assertions are stable.
            out.sort_by(|a, b| a.path.cmp(&b.path));
            Ok(out)
        }

        fn read_to_string(&self, path: &Path) -> io::Result<String> {
            self.files
                .lock()
                .unwrap()
                .get(path)
                .map(|(c, _)| c.clone())
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }

        fn read_tail(&self, path: &Path, _n_bytes: u64) -> io::Result<String> {
            self.read_to_string(path)
        }

        fn is_dir(&self, _path: &Path) -> bool {
            true
        }

        fn ssh_argv(&self, _tty: bool, _remote_cmd: &[&str]) -> Option<Vec<String>> {
            None
        }
    }

    fn drain(rx: &Receiver<WatcherEvent>) -> Vec<WatcherEvent> {
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            out.push(e);
        }
        out
    }

    fn ts(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn assistant_line() -> &'static str {
        r#"{"type":"user","message":"hi"}
{"type":"assistant","message":"hello"}
"#
    }

    fn user_line() -> &'static str {
        r#"{"type":"assistant","message":"hello"}
{"type":"user","message":"do thing"}
"#
    }

    #[test]
    fn poll_once_emits_nothing_when_no_transcripts_changed() {
        let host = MockHost::new("devbox");
        let path = PathBuf::from("/r/p/s.jsonl");
        host.put(&path, assistant_line(), ts(100));

        let mut known: HashMap<PathBuf, (SessionId, SystemTime)> =
            [(path.clone(), (SessionId("s".into()), ts(100)))]
                .into_iter()
                .collect();
        let (tx, rx) = mpsc::channel();

        assert!(poll_once(
            &host,
            host.id(),
            Path::new("/r"),
            &mut known,
            &tx
        ));
        assert!(drain(&rx).is_empty());
    }

    #[test]
    fn poll_once_emits_attention_for_known_path_when_mtime_advances() {
        let host = MockHost::new("devbox");
        let path = PathBuf::from("/r/p/s.jsonl");
        // Seed: known at mtime=100. Now bump to 200 with content that
        // derives to NeedsInput.
        host.put(&path, assistant_line(), ts(200));

        let mut known: HashMap<PathBuf, (SessionId, SystemTime)> =
            [(path.clone(), (SessionId("s".into()), ts(100)))]
                .into_iter()
                .collect();
        let (tx, rx) = mpsc::channel();
        poll_once(&host, host.id(), Path::new("/r"), &mut known, &tx);

        let events = drain(&rx);
        assert_eq!(events.len(), 1, "got: {events:?}");
        match &events[0] {
            WatcherEvent::Attention(u) => {
                assert_eq!(u.id.0, "s");
                assert_eq!(u.attention, Attention::NeedsInput);
            }
            other => panic!("expected Attention, got: {other:?}"),
        }
        // Known-set should now record the new mtime so the next tick
        // doesn't re-emit.
        assert_eq!(known.get(&path).unwrap().1, ts(200));
    }

    #[test]
    fn poll_once_emits_new_transcript_then_attention_for_unknown_paths() {
        let host = MockHost::new("devbox");
        let path = PathBuf::from("/r/p/fresh.jsonl");
        host.put(&path, user_line(), ts(50));

        let mut known: HashMap<PathBuf, (SessionId, SystemTime)> = HashMap::new();
        let (tx, rx) = mpsc::channel();
        poll_once(&host, host.id(), Path::new("/r"), &mut known, &tx);

        let events = drain(&rx);
        assert_eq!(events.len(), 2, "got: {events:?}");
        match &events[0] {
            WatcherEvent::NewTranscript {
                host: h,
                path: p,
                mtime: m,
            } => {
                assert_eq!(h.as_str(), "devbox");
                assert_eq!(p, &path);
                assert_eq!(*m, ts(50));
            }
            other => panic!("expected NewTranscript first, got: {other:?}"),
        }
        match &events[1] {
            WatcherEvent::Attention(u) => {
                assert_eq!(u.id.0, "fresh");
                assert_eq!(u.attention, Attention::Working);
            }
            other => panic!("expected Attention second, got: {other:?}"),
        }
        // The new path is now in known-set; a subsequent tick with no
        // mtime change should be silent.
        let (tx2, rx2) = mpsc::channel();
        poll_once(&host, host.id(), Path::new("/r"), &mut known, &tx2);
        assert!(drain(&rx2).is_empty());
    }

    #[test]
    fn poll_once_swallows_list_transcripts_errors() {
        // A host that always errors should not propagate — the next
        // poll iteration retries. The cadence is the resilience.
        struct FlakyHost(HostId);
        impl Host for FlakyHost {
            fn id(&self) -> &HostId {
                &self.0
            }
            fn list_transcripts(&self, _: &Path) -> io::Result<Vec<TranscriptStat>> {
                Err(io::Error::other("ssh: connection refused"))
            }
            fn read_to_string(&self, _: &Path) -> io::Result<String> {
                unreachable!()
            }
            fn read_tail(&self, _: &Path, _: u64) -> io::Result<String> {
                unreachable!()
            }
            fn is_dir(&self, _: &Path) -> bool {
                false
            }
            fn ssh_argv(&self, _: bool, _: &[&str]) -> Option<Vec<String>> {
                None
            }
        }
        let host = FlakyHost(HostId("flaky".into()));
        let mut known = HashMap::new();
        let (tx, rx) = mpsc::channel();
        assert!(poll_once(
            &host,
            host.id(),
            Path::new("/r"),
            &mut known,
            &tx
        ));
        assert!(drain(&rx).is_empty());
    }

    #[test]
    fn poll_once_returns_false_when_receiver_dropped() {
        let host = MockHost::new("devbox");
        let path = PathBuf::from("/r/p/s.jsonl");
        host.put(&path, assistant_line(), ts(200));
        let mut known: HashMap<PathBuf, (SessionId, SystemTime)> =
            [(path.clone(), (SessionId("s".into()), ts(100)))]
                .into_iter()
                .collect();

        let (tx, rx) = mpsc::channel();
        drop(rx);
        assert!(
            !poll_once(&host, host.id(), Path::new("/r"), &mut known, &tx),
            "should signal shutdown when no receiver"
        );
    }

    #[test]
    fn poll_once_skips_paths_without_a_file_stem() {
        // Pathological: a path that doesn't yield a valid file stem
        // (empty, or non-UTF-8 components on Linux). We can't easily
        // construct one with stem.is_none() *and* path.is_some(); but
        // we can verify behaviour against a normal hidden-file path
        // doesn't blow up. The branch is defensive — this test mostly
        // asserts the function tolerates exotic inputs without panic.
        let host = MockHost::new("devbox");
        let path = PathBuf::from("/r/p/.hidden");
        host.put(&path, "{}\n", ts(10));
        let mut known = HashMap::new();
        let (tx, rx) = mpsc::channel();
        assert!(poll_once(
            &host,
            host.id(),
            Path::new("/r"),
            &mut known,
            &tx
        ));
        // `.hidden` has stem ".hidden" (no extension), so it does emit.
        // The point is no panic.
        assert!(!drain(&rx).is_empty());
    }
}
