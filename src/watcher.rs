use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use notify::{EventKind, RecursiveMode, Watcher};

use crate::agent::{AgentDerivation, AgentKind, agent};
use crate::attachment::{LivePaneSnapshot, list_live_panes};
use crate::host::Host;
use crate::session::{Attention, HostId, SessionId};

/// How much of the transcript's tail to read when deriving attention.
/// Transcripts are append-only JSONL; reading the last few KB is enough
/// to find the most recent meaningful entry without parsing the whole file.
const TAIL_BYTES: u64 = 32 * 1024;

/// Default interval between remote transcript polls. Low enough that
/// needs-input feels live (a few seconds end-to-end), high enough that
/// an idle ten-session host isn't generating continuous SSH round-trips.
/// Becomes configurable in M5.
pub const REMOTE_POLL_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Debug)]
pub struct AttentionUpdate {
    pub id: SessionId,
    /// Which agent CLI produced this update. The tail was already parsed
    /// through this agent's `derive`, so the value is informational for
    /// the catalog handler (it applies attention by id); carried alongside
    /// `id` so an event is self-describing and future routing needs no
    /// second lookup.
    pub agent: AgentKind,
    pub attention: Attention,
    /// Whether `attention` is `Working` *because* the last transcript
    /// entry is a bare assistant `tool_use` — the signature of an
    /// in-flight tool or a blocked permission prompt. The catalog uses
    /// this to refuse to clear a live hook "blocked" pin on the prompt's
    /// own entry (see [`crate::agent::AgentDerivation`] and
    /// [`crate::catalog::SessionCatalog::apply_heuristic_attention`]).
    /// `false` for every non-`tool_use` derivation.
    pub from_tool_use: bool,
    /// Transcript mtime captured at the moment the event was produced.
    /// `None` only when the producer couldn't stat the file
    /// (filesystem hiccup, file vanished between event and stat) —
    /// every prime, polled, and live-notify path now ships the file's
    /// current mtime so the notifier's startup-replay gate (see
    /// `Transition::source_at`) can recognise events derived from
    /// bytes written before the current run began. The catalog also
    /// uses it to keep the "last activity" cell live across a running
    /// conversation; `touch_activity` is a no-op when the value
    /// matches what's already stored, so the prime case (where the
    /// mtime equals discovery's) costs nothing.
    pub mtime: Option<SystemTime>,
    /// Files edited within the scanned tail, most-recent-first (see
    /// [`crate::agent::AgentDerivation`]). The catalog unions these
    /// into `Session.edited_files` via `merge_edited_files` so the picker
    /// stays current as the conversation runs. Empty when the tail held
    /// no edits or couldn't be read.
    pub edited_files: Vec<PathBuf>,
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
        /// Which agent CLI owns this transcript, resolved by the (host ×
        /// agent) root the path was discovered under. The catalog builds
        /// the session through this agent's parser.
        agent: AgentKind,
        path: PathBuf,
        mtime: SystemTime,
    },
    /// A Claude Code `Notification` hook event arrived for `id` via
    /// the file-based ingress (`agent-mux hook` subcommand → marker
    /// file → this watcher → main loop → catalog). Forces the session
    /// into `NeedsInput`, regardless of what the heuristic would
    /// otherwise derive. `received_at` is the `SystemTime` at which
    /// the marker file's filename was minted, used to pin hook
    /// authority in the catalog (heuristic updates with newer
    /// transcript mtimes clear the pin).
    Hook {
        id: SessionId,
        received_at: SystemTime,
        /// Whether this hook is a blocking prompt (`permission_prompt` /
        /// `elicitation_dialog`) rather than an idle nudge — drives the
        /// session's `blocking_prompt` flag for the "answer me" glyph.
        blocking_prompt: bool,
        /// The Claude Code payload's `message` field (the prompt text),
        /// or `None` when absent. Carried through to the notifier as
        /// the toast body — far more informative than the project path.
        message: Option<String>,
    },
    /// Snapshot of every live tmux pane on `host`: `cwds` carries
    /// each pane's `pane_current_path`, `session_names` carries each
    /// pane's owning tmux `session_name`. Indices align: index `i` of
    /// `cwds` describes the same pane as index `i` of `session_names`.
    /// The catalog uses this to decide per-session whether Enter will
    /// be a fast switch (deterministic `agent-mux-<id>` tmux session
    /// exists, or a pane matches the session's `project_dir`) vs an
    /// auto-resume (no match — fall through to the agent's resume
    /// command). Empty lists are a valid value (no live panes / no tmux server
    /// / ssh hiccup) — every session on the host transitions to
    /// `Some(false)` in that case.
    LivePanes {
        host: HostId,
        cwds: Vec<PathBuf>,
        session_names: Vec<String>,
    },
}

pub struct TranscriptWatcher {
    /// Kept alive for the lifetime of the dashboard; dropping it tears
    /// the notify backend down. We also call `.watch` on it from
    /// `add_target` in the no-recursive-root fallback path.
    watcher: notify::RecommendedWatcher,
    /// Path → (session id, agent). The agent is stored per known target so
    /// a filesystem event routes to the right parser even in the per-file
    /// fallback mode where no recursive root is available to match against.
    targets: Arc<Mutex<HashMap<PathBuf, (SessionId, AgentKind)>>>,
    event_tx: Sender<WatcherEvent>,
    host: Arc<dyn Host>,
    /// True when at least one recursive root watch is active. When false,
    /// `add_target` falls back to per-file watches so the watcher still
    /// works in the degenerate no-discovery-root case.
    has_recursive_root: bool,
}

/// Longest-prefix match of `path` against the watched roots, yielding the
/// owning agent + its root. `None` when no root contains the path (e.g.
/// per-file fallback mode, where `roots` is empty).
fn agent_for_path<'a>(
    roots: &'a [(AgentKind, PathBuf)],
    path: &Path,
) -> Option<(AgentKind, &'a Path)> {
    roots
        .iter()
        .filter(|(_, root)| path.starts_with(root))
        .max_by_key(|(_, root)| root.as_os_str().len())
        .map(|(k, root)| (*k, root.as_path()))
}

impl TranscriptWatcher {
    /// Clone the sender that backs this watcher's event channel.
    /// Used by sibling subsystems (e.g. the hook-marker watcher in
    /// `hook_ingest`) that need to feed `WatcherEvent`s into the same
    /// channel the dashboard's main loop already drains, without
    /// each one having to plumb its own receiver.
    #[must_use]
    pub fn event_sender(&self) -> Sender<WatcherEvent> {
        self.event_tx.clone()
    }

    /// Start watching for transcript events.
    ///
    /// `roots` is one `(AgentKind, root)` pair per enabled agent. A single
    /// recursive `notify` watcher covers every root under one process (the
    /// "one filesystem watcher" discipline); a filesystem event is routed
    /// to its owning agent by longest-prefix root match, then filtered by
    /// that agent's [`crate::agent::AgentCli::is_transcript`] predicate.
    /// The reference agent's (claude) root is created if missing so the
    /// watch attaches on first run — other agents' roots are watched only
    /// when present, because the directory's existence *is* the "installed
    /// here" signal and fabricating it would defeat that. When no root can
    /// be watched recursively (empty `roots`, or every watch failed), the
    /// watcher falls back to per-file watches for the `initial` set and
    /// `NewTranscript` events will not fire.
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
    // Linear setup: watch each root, prime initial state, then spawn the
    // one notify thread. The steps read top-to-bottom and share locals;
    // extracting them would just scatter the setup across helpers.
    #[allow(clippy::too_many_lines)]
    pub fn start(
        host: Arc<dyn Host>,
        initial: Vec<(SessionId, PathBuf, AgentKind)>,
        roots: &[(AgentKind, PathBuf)],
    ) -> notify::Result<(Self, Receiver<WatcherEvent>)> {
        let (event_tx, event_rx) = mpsc::channel::<WatcherEvent>();
        let (notify_tx, notify_rx) = mpsc::channel();

        let mut watcher = notify::recommended_watcher(notify_tx)?;

        let mut watched_roots: Vec<(AgentKind, PathBuf)> = Vec::new();
        for (kind, root) in roots {
            if *kind == AgentKind::Claude {
                // Byte-identical to the pre-WP2 first-run behaviour: create
                // the reference agent's root so the watch attaches before
                // the agent itself gets around to creating it (and we'd
                // otherwise miss the discovery window).
                let _ = fs::create_dir_all(root);
            }
            if watcher.watch(root, RecursiveMode::Recursive).is_ok() {
                watched_roots.push((*kind, root.clone()));
            }
        }
        let has_recursive_root = !watched_roots.is_empty();

        if !has_recursive_root {
            for (_, path, _) in &initial {
                watcher.watch(path, RecursiveMode::NonRecursive)?;
            }
        }

        // Prime initial state so the UI shows real attention from frame one.
        // `mtime` carries the file's current on-disk mtime so the
        // notifier's startup-replay gate (see `Transition::source_at`)
        // can recognise these events as "state derived from bytes
        // written before agent-mux started" and suppress the toast.
        // `touch_activity` is still a no-op here because the catalog
        // already holds the discovery-time mtime — passing it through
        // doesn't rewind the cell.
        for (id, path, kind) in &initial {
            let mtime = fs::metadata(path).and_then(|m| m.modified()).ok();
            let detail = derive_attention_detail(host.as_ref(), path, *kind, Path::new(""));
            let _ = event_tx.send(WatcherEvent::Attention(AttentionUpdate {
                id: id.clone(),
                agent: *kind,
                attention: detail.attention,
                from_tool_use: detail.from_tool_use,
                mtime,
                edited_files: detail.edited_files,
            }));
        }

        let targets: Arc<Mutex<HashMap<PathBuf, (SessionId, AgentKind)>>> = Arc::new(Mutex::new(
            initial.into_iter().map(|(id, p, k)| (p, (id, k))).collect(),
        ));

        let targets_for_thread = Arc::clone(&targets);
        let event_tx_for_thread = event_tx.clone();
        let host_for_thread = Arc::clone(&host);
        let roots_for_thread = watched_roots;
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
                    // Route path → owning agent by longest-prefix root
                    // match, then apply *that agent's* top-level-transcript
                    // predicate. This drops sidechain/subagent transcripts
                    // nested deeper than the agent's tree shape (e.g. Claude
                    // Code's `<bucket>/<parent-id>/subagents/…`), which the
                    // recursive watch would otherwise surface as flapping
                    // standalone rows; `Host::list_transcripts` enforces the
                    // same shape at startup so both filter identically.
                    let routed = agent_for_path(&roots_for_thread, &path);
                    if let Some((kind, root)) = routed
                        && !agent(kind).is_transcript(&path, root)
                    {
                        continue;
                    }
                    let known = targets_for_thread
                        .lock()
                        .ok()
                        .and_then(|m| m.get(&path).cloned());
                    let outgoing = if let Some((id, kind)) = known {
                        // Stat the file alongside the tail read so the
                        // dashboard's last-activity cell stays live —
                        // dropping the mtime on stat failure (file
                        // vanished, filesystem hiccup) is fine; the
                        // catalog keeps its existing value.
                        let mtime = fs::metadata(&path).and_then(|m| m.modified()).ok();
                        let detail = derive_attention_detail(
                            host_for_thread.as_ref(),
                            &path,
                            kind,
                            Path::new(""),
                        );
                        WatcherEvent::Attention(AttentionUpdate {
                            id,
                            agent: kind,
                            attention: detail.attention,
                            from_tool_use: detail.from_tool_use,
                            mtime,
                            edited_files: detail.edited_files,
                        })
                    } else {
                        // A previously-unknown transcript: its agent is the
                        // routed one. Without a watched root (per-file
                        // fallback) we can't attribute it, so skip —
                        // `NewTranscript` never fires in that mode anyway.
                        let Some((kind, _)) = routed else {
                            continue;
                        };
                        // Stat now so the main thread doesn't have to;
                        // the file may have vanished between event and
                        // stat, in which case we drop and let the next
                        // event re-trigger.
                        let Ok(mtime) = fs::metadata(&path).and_then(|m| m.modified()) else {
                            continue;
                        };
                        WatcherEvent::NewTranscript {
                            host: HostId::local(),
                            agent: kind,
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
    pub fn add_target(
        &mut self,
        id: SessionId,
        path: PathBuf,
        kind: AgentKind,
    ) -> notify::Result<()> {
        if !self.has_recursive_root {
            self.watcher.watch(&path, RecursiveMode::NonRecursive)?;
        }
        let detail = derive_attention_detail(self.host.as_ref(), &path, kind, Path::new(""));
        let mtime = fs::metadata(&path).and_then(|m| m.modified()).ok();
        if let Ok(mut targets) = self.targets.lock() {
            targets.insert(path, (id.clone(), kind));
        }
        let _ = self.event_tx.send(WatcherEvent::Attention(AttentionUpdate {
            id,
            agent: kind,
            attention: detail.attention,
            from_tool_use: detail.from_tool_use,
            mtime,
            edited_files: detail.edited_files,
        }));
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
        kind: AgentKind,
    ) -> notify::Result<()> {
        if host.is_local() {
            self.add_target(id, path, kind)
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
        roots: Vec<(AgentKind, PathBuf)>,
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
            // WP8: hook markers live under each enabled agent's
            // `<root>/.agent-mux-hooks/` (claude + codex today). Poll each
            // one per tick over the same ControlMaster — an idle N-agent
            // host is N cheap `list_files` calls, and a marker-free dir is
            // a single empty listing.
            let hooks_dirs: Vec<PathBuf> = roots
                .iter()
                .map(|(_, r)| crate::hook_ingest::hook_dir_for_transcripts_root(r))
                .collect();
            loop {
                thread::sleep(interval);
                // Heal a dead connection before doing any work this tick.
                // For SSH hosts this re-establishes a `ControlMaster`
                // that died while the laptop slept (past ControlPersist),
                // so the master is warm before the user switches sessions
                // — keeping attach off the slow per-command-handshake
                // path. No-op for local hosts. A failure here means the
                // host is still unreachable; skip this tick's work and
                // retry next interval rather than killing the poller, so
                // a transient outage self-heals.
                match host.ensure_connected() {
                    Ok(true) => {
                        // Diagnostic, not user-facing — and this thread
                        // runs while the TUI owns the alternate screen, so
                        // an `eprintln!` here would paint over the
                        // dashboard. Route to the log file instead.
                        crate::logging::log_line(&format!(
                            "re-established connection to host '{}'",
                            host_id.0
                        ));
                    }
                    Ok(false) => {}
                    Err(e) => {
                        crate::logging::log_line(&format!(
                            "reconnect to host '{}' failed: {e}",
                            host_id.0
                        ));
                        continue;
                    }
                }
                // One `find` per enabled agent root per tick. An idle
                // N-agent host therefore costs N cheap finds (mtime-skip
                // keeps the per-transcript reads down to actual changes);
                // only *enabled* agents cost anything, and a root whose
                // directory doesn't exist folds into an empty listing.
                for (kind, root) in &roots {
                    if !poll_once(host.as_ref(), &host_id, root, *kind, &mut known, &tx) {
                        return;
                    }
                }
                // Sibling tick on the same SSH ControlMaster: drain any
                // new hook markers in each enabled agent's
                // `<root>/.agent-mux-hooks/` and emit them as
                // `WatcherEvent::Hook`. A failure here doesn't break
                // attention polling — the next tick retries and a really
                // broken host surfaces via `list_transcripts` failing
                // anyway.
                for hooks_dir in &hooks_dirs {
                    if !poll_hooks_once(host.as_ref(), hooks_dir, &tx) {
                        return;
                    }
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
                // Skip the (potentially `ConnectTimeout`-long) tmux query
                // when the host's connection is down — `ensure_connected`
                // shares the transcript poller's backoff, so a host whose
                // master can't be re-established isn't hit with a fresh
                // doomed `ssh tmux list-panes` every tick. While down,
                // report an empty snapshot (every session's pane goes
                // `Some(false)` — the user-visible reality) without the
                // round-trip. No-op for local hosts.
                let snap = match host.ensure_connected() {
                    Ok(_) => list_live_panes(host.as_ref()),
                    Err(_) => LivePaneSnapshot::default(),
                };
                if tx
                    .send(WatcherEvent::LivePanes {
                        host: host_id.clone(),
                        cwds: snap.cwds,
                        session_names: snap.session_names,
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

/// One tick of the remote hook-marker drain. Lists files in
/// `hooks_dir` over the same SSH connection the transcript poller
/// uses, bulk-reads new markers, emits one [`WatcherEvent::Hook`]
/// per successfully-parsed marker, then deletes the markers on the
/// remote so the directory doesn't grow without bound.
///
/// Returns `false` iff the channel receiver dropped (dashboard
/// exited); the polling thread breaks out of its loop in that case.
/// All other failures — missing dir, partial reads, SSH hiccups,
/// bad payloads — are swallowed silently so a transient remote
/// issue doesn't break the polling cadence. Markers that fail to
/// parse stay on the remote for human inspection (same contract as
/// the local `ingest_marker` path).
fn poll_hooks_once(host: &dyn Host, hooks_dir: &Path, tx: &Sender<WatcherEvent>) -> bool {
    let Ok(paths) = host.list_files(hooks_dir) else {
        return true;
    };
    // Skip mid-write `.tmp` artifacts for the same reason the local
    // ingest path does — a tmp+rename producer can leave them
    // briefly visible.
    let marker_paths: Vec<PathBuf> = paths
        .into_iter()
        .filter(|p| p.extension().is_none_or(|e| e != "tmp"))
        .collect();
    if marker_paths.is_empty() {
        return true;
    }
    let path_refs: Vec<&Path> = marker_paths.iter().map(PathBuf::as_path).collect();
    let Ok(contents) = host.read_many(&path_refs) else {
        return true;
    };
    for (path, content_result) in marker_paths.iter().zip(contents) {
        let Ok(raw) = content_result else {
            // Per-path NotFound / read failure — leave the marker
            // for the next tick. (A removed-mid-tick file lands here
            // and resolves itself.)
            continue;
        };
        let Ok(event) = crate::hook_ingest::parse_marker_content(path, &raw) else {
            // Bad payload stays on the remote for inspection.
            continue;
        };
        if tx
            .send(WatcherEvent::Hook {
                id: event.session_id,
                received_at: event.received_at,
                blocking_prompt: event.blocking_prompt,
                message: event.message,
            })
            .is_err()
        {
            return false;
        }
        // Best-effort delete; a failed remove just means the next
        // tick re-emits the same hook event, which the notifier's
        // episodic-flag suppression collapses harmlessly.
        let _ = host.remove(path);
    }
    true
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
    kind: AgentKind,
    known: &mut HashMap<PathBuf, (SessionId, SystemTime)>,
    tx: &Sender<WatcherEvent>,
) -> bool {
    let cli = agent(kind);
    let Ok(stats) = host.list_transcripts(root, &cli.listing()) else {
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
                let detail = derive_attention_detail(host, &stat.path, kind, Path::new(""));
                if tx
                    .send(WatcherEvent::Attention(AttentionUpdate {
                        id: id.clone(),
                        agent: kind,
                        attention: detail.attention,
                        from_tool_use: detail.from_tool_use,
                        mtime: Some(stat.mtime),
                        edited_files: detail.edited_files,
                    }))
                    .is_err()
                {
                    return false;
                }
            }
            continue;
        }
        let Some(id) = cli.session_id_from_path(&stat.path) else {
            continue;
        };
        known.insert(stat.path.clone(), (id.clone(), stat.mtime));
        if tx
            .send(WatcherEvent::NewTranscript {
                host: host_id.clone(),
                agent: kind,
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
        let detail = derive_attention_detail(host, &stat.path, kind, Path::new(""));
        if tx
            .send(WatcherEvent::Attention(AttentionUpdate {
                id,
                agent: kind,
                attention: detail.attention,
                from_tool_use: detail.from_tool_use,
                mtime: Some(stat.mtime),
                edited_files: detail.edited_files,
            }))
            .is_err()
        {
            return false;
        }
    }
    true
}

/// Larger escalation window for [`derive_attention_detail`] when the
/// default [`TAIL_BYTES`] tail yields no parseable entry. A single final
/// assistant message can exceed 32 KiB (a long answer on one JSONL
/// line); the 32 KiB tail then holds only an unterminated continuation
/// that fails to parse, so a *completed* turn would otherwise read as
/// `Unknown`. 1 MiB covers any realistic final message; a message larger
/// still falls back to `Unknown` (acceptably rare).
const TAIL_BYTES_ESCALATED: u64 = 1024 * 1024;

/// Derive an attention state from the most recent meaningful JSONL entry in
/// `transcript_path`, parsing through `kind`'s agent. Reads only the last
/// `TAIL_BYTES` of the file through `host`; the (possibly truncated) first
/// line is discarded by virtue of failing to parse, and the remaining lines
/// are walked to find the latest conversational entry.
#[must_use]
pub fn derive_attention(host: &dyn Host, transcript_path: &Path, kind: AgentKind) -> Attention {
    derive_attention_detail(host, transcript_path, kind, Path::new("")).attention
}

/// Like [`derive_attention`] but returns the full [`AgentDerivation`]
/// (attention + `from_tool_use` + edited files). The five watcher
/// producers use this so the catalog can protect a live hook pin from
/// the blocked prompt's own `tool_use` signature.
///
/// This is the host-read orchestration around the agent's pure parse: it
/// reads the tail through `host`, parses via the agent, and escalates to a
/// larger read once when the default tail yields `Unknown` — a guard
/// against a final message that overran the 32 KiB window leaving a
/// completed turn looking like "no signal". The parsing itself lives
/// behind [`crate::agent::AgentCli::derive`].
#[must_use]
pub fn derive_attention_detail(
    host: &dyn Host,
    transcript_path: &Path,
    kind: AgentKind,
    cwd: &Path,
) -> AgentDerivation {
    // `cwd` is the session's working directory, threaded through for agents
    // whose edited-file paths can be relative (pi). The watcher's own
    // callers pass an empty placeholder — a tail read has no header `cwd`
    // line — and the reference (claude) parser ignores it; discovery, which
    // *does* know the cwd, hands the real value to `AgentCli::derive`.
    let cli = agent(kind);
    let Ok(tail) = host.read_tail(transcript_path, TAIL_BYTES) else {
        return AgentDerivation {
            attention: Attention::Unknown,
            from_tool_use: false,
            edited_files: Vec::new(),
        };
    };
    let detail = cli.derive(&tail, cwd);
    if detail.attention == Attention::Unknown
        && let Ok(bigger) = host.read_tail(transcript_path, TAIL_BYTES_ESCALATED)
    {
        let escalated = cli.derive(&bigger, cwd);
        if escalated.attention != Attention::Unknown {
            return escalated;
        }
    }
    detail
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
        assert_eq!(
            derive_attention(&host(), f.path(), AgentKind::Claude),
            Attention::Unknown
        );
    }

    // These tests cover the *host-read orchestration* in
    // `derive_attention` (tail read via the Host, escalation, and routing
    // the bytes through the agent parser). The agent-specific
    // classification (stop-reason handling, edited-file extraction) is
    // pinned in the reference-agent module; the fixtures here stay minimal and
    // agent-neutral so this module carries no transcript-field coupling.

    #[test]
    fn only_housekeeping_entries_is_unknown() {
        // Entries the parser doesn't classify leave the state Unknown.
        let f = write_jsonl(&[
            r#"{"type":"permission-mode","permissionMode":"default"}"#,
            r#"{"type":"file-history-snapshot"}"#,
        ]);
        assert_eq!(
            derive_attention(&host(), f.path(), AgentKind::Claude),
            Attention::Unknown
        );
    }

    #[test]
    fn last_assistant_means_needs_input() {
        let f = write_jsonl(&[
            r#"{"type":"user","message":"hi"}"#,
            r#"{"type":"assistant","message":"hello"}"#,
        ]);
        assert_eq!(
            derive_attention(&host(), f.path(), AgentKind::Claude),
            Attention::NeedsInput
        );
    }

    #[test]
    fn last_user_message_means_working() {
        let f = write_jsonl(&[
            r#"{"type":"assistant","message":"hello"}"#,
            r#"{"type":"user","message":"do thing"}"#,
        ]);
        assert_eq!(
            derive_attention(&host(), f.path(), AgentKind::Claude),
            Attention::Working
        );
    }

    #[test]
    fn last_tool_result_means_working() {
        let f = write_jsonl(&[
            r#"{"type":"assistant","message":"running tool"}"#,
            r#"{"type":"user","toolUseResult":{"stdout":"ok"}}"#,
        ]);
        assert_eq!(
            derive_attention(&host(), f.path(), AgentKind::Claude),
            Attention::Working
        );
    }

    #[test]
    fn housekeeping_after_assistant_does_not_change_state() {
        let f = write_jsonl(&[
            r#"{"type":"user","message":"hi"}"#,
            r#"{"type":"assistant","message":"hello"}"#,
            r#"{"type":"file-history-snapshot"}"#,
        ]);
        assert_eq!(
            derive_attention(&host(), f.path(), AgentKind::Claude),
            Attention::NeedsInput
        );
    }

    #[test]
    fn nonexistent_file_is_unknown() {
        let path = std::path::PathBuf::from("/nonexistent/path/foo.jsonl");
        assert_eq!(
            derive_attention(&host(), &path, AgentKind::Claude),
            Attention::Unknown
        );
    }

    #[test]
    fn derive_attention_recovers_state_from_oversized_final_message() {
        // Regression for "completed not detected": a final assistant
        // message larger than TAIL_BYTES leaves only an unterminated
        // continuation in the 32 KiB tail, so the tail-only read derives
        // `Unknown`. The escalation re-read must recover the real state.
        // Uses a bare assistant message (no transcript-field coupling —
        // the classification detail is tested in the reference-agent module).
        let big_text = "x".repeat(40 * 1024); // > TAIL_BYTES (32 KiB) tail window
        let huge_final = format!(r#"{{"type":"assistant","message":"{big_text}"}}"#);
        let f = write_jsonl(&[r#"{"type":"user","message":"go"}"#, &huge_final]);
        assert_eq!(
            derive_attention(&host(), f.path(), AgentKind::Claude),
            Attention::NeedsInput,
            "oversized final message must still classify, not fall to unknown"
        );
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

        fn list_transcripts(
            &self,
            _root: &Path,
            _spec: &crate::agent::ListingSpec,
        ) -> io::Result<Vec<TranscriptStat>> {
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

        fn read_many(&self, paths: &[&Path]) -> io::Result<Vec<io::Result<String>>> {
            Ok(paths.iter().map(|p| self.read_to_string(p)).collect())
        }

        fn is_dir_many(&self, paths: &[&Path]) -> io::Result<Vec<bool>> {
            Ok(paths.iter().map(|p| self.is_dir(p)).collect())
        }

        fn run(&self, _: Option<&Path>, _: &str, _: &[&str]) -> io::Result<std::process::Output> {
            unreachable!()
        }

        fn write_file(&self, _: &Path, _: &str) -> io::Result<()> {
            unreachable!()
        }

        fn list_files(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
            // Return every file in the map whose parent matches dir.
            // Lets the hook-poller tests drive markers through the
            // same in-memory store as transcripts.
            let mut out: Vec<PathBuf> = self
                .files
                .lock()
                .unwrap()
                .keys()
                .filter(|p| p.parent() == Some(dir))
                .cloned()
                .collect();
            out.sort();
            Ok(out)
        }

        fn remove(&self, path: &Path) -> io::Result<()> {
            self.files.lock().unwrap().remove(path);
            Ok(())
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
            AgentKind::Claude,
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
        poll_once(
            &host,
            host.id(),
            Path::new("/r"),
            AgentKind::Claude,
            &mut known,
            &tx,
        );

        let events = drain(&rx);
        assert_eq!(events.len(), 1, "got: {events:?}");
        match &events[0] {
            WatcherEvent::Attention(u) => {
                assert_eq!(u.id.0, "s");
                assert_eq!(u.attention, Attention::NeedsInput);
                // mtime travels with the event so the catalog can keep
                // the sidebar's "last activity" cell live.
                assert_eq!(u.mtime, Some(ts(200)));
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
        poll_once(
            &host,
            host.id(),
            Path::new("/r"),
            AgentKind::Claude,
            &mut known,
            &tx,
        );

        let events = drain(&rx);
        assert_eq!(events.len(), 2, "got: {events:?}");
        match &events[0] {
            WatcherEvent::NewTranscript {
                host: h,
                agent: a,
                path: p,
                mtime: m,
            } => {
                assert_eq!(h.as_str(), "devbox");
                assert_eq!(*a, AgentKind::Claude);
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
        poll_once(
            &host,
            host.id(),
            Path::new("/r"),
            AgentKind::Claude,
            &mut known,
            &tx2,
        );
        assert!(drain(&rx2).is_empty());
    }

    #[test]
    fn poll_once_tags_events_with_the_polled_agent() {
        // WP2 routing: polling a codex root emits events carrying
        // `AgentKind::Codex`, and the session id is derived through the
        // codex path-id rule (trailing uuid of the rollout filename) — so
        // the catalog builds it through the right agent, not claude.
        let host = MockHost::new("devbox");
        let path = PathBuf::from(
            "/r/2026/07/09/rollout-2026-07-09T10-00-00-00000000-1111-2222-3333-444444444444.jsonl",
        );
        host.put(&path, user_line(), ts(50));

        let mut known: HashMap<PathBuf, (SessionId, SystemTime)> = HashMap::new();
        let (tx, rx) = mpsc::channel();
        poll_once(
            &host,
            host.id(),
            Path::new("/r"),
            AgentKind::Codex,
            &mut known,
            &tx,
        );

        let events = drain(&rx);
        assert_eq!(events.len(), 2, "got: {events:?}");
        match &events[0] {
            WatcherEvent::NewTranscript { agent, .. } => assert_eq!(*agent, AgentKind::Codex),
            other => panic!("expected NewTranscript, got: {other:?}"),
        }
        match &events[1] {
            WatcherEvent::Attention(u) => {
                assert_eq!(u.agent, AgentKind::Codex);
                assert_eq!(u.id.0, "00000000-1111-2222-3333-444444444444");
            }
            other => panic!("expected Attention, got: {other:?}"),
        }
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
            fn list_transcripts(
                &self,
                _: &Path,
                _: &crate::agent::ListingSpec,
            ) -> io::Result<Vec<TranscriptStat>> {
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
            fn read_many(&self, _: &[&Path]) -> io::Result<Vec<io::Result<String>>> {
                unreachable!()
            }
            fn is_dir_many(&self, _: &[&Path]) -> io::Result<Vec<bool>> {
                unreachable!()
            }
            fn run(
                &self,
                _: Option<&Path>,
                _: &str,
                _: &[&str],
            ) -> io::Result<std::process::Output> {
                unreachable!()
            }
            fn write_file(&self, _: &Path, _: &str) -> io::Result<()> {
                unreachable!()
            }
            fn list_files(&self, _: &Path) -> io::Result<Vec<PathBuf>> {
                Ok(Vec::new())
            }
            fn remove(&self, _: &Path) -> io::Result<()> {
                Ok(())
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
            AgentKind::Claude,
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
            !poll_once(
                &host,
                host.id(),
                Path::new("/r"),
                AgentKind::Claude,
                &mut known,
                &tx
            ),
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
            AgentKind::Claude,
            &mut known,
            &tx
        ));
        // `.hidden` has stem ".hidden" (no extension), so it does emit.
        // The point is no panic.
        assert!(!drain(&rx).is_empty());
    }

    // ---- poll_hooks_once ----

    #[test]
    fn poll_hooks_once_emits_event_per_marker_and_deletes_after_send() {
        let host = MockHost::new("devbox");
        let hooks_dir = PathBuf::from("/r/.agent-mux-hooks");
        let marker = hooks_dir.join("0000000123456-sess-a.json");
        host.put(
            &marker,
            r#"{"session_id":"sess-a","notification_type":"permission_prompt"}"#,
            ts(1),
        );
        let (tx, rx) = mpsc::channel();
        assert!(poll_hooks_once(&host, &hooks_dir, &tx));
        let events = drain(&rx);
        assert_eq!(events.len(), 1, "exactly one Hook event: {events:?}");
        match &events[0] {
            WatcherEvent::Hook { id, .. } => assert_eq!(id.0, "sess-a"),
            other => panic!("expected Hook event, got {other:?}"),
        }
        // Successful ingest deletes the marker so the next tick
        // doesn't re-emit (the notifier's episodic flag would
        // collapse the duplicate, but the watcher shouldn't generate
        // it in the first place).
        assert!(host.list_files(&hooks_dir).unwrap().is_empty());
    }

    #[test]
    fn poll_hooks_once_skips_tmp_artifacts() {
        // A producer mid-write leaves `<name>.tmp` briefly visible.
        // The remote poller must not bulk-read it (the content is
        // half-written) or delete it (the producer is still using it).
        let host = MockHost::new("devbox");
        let hooks_dir = PathBuf::from("/r/.agent-mux-hooks");
        let tmp_path = hooks_dir.join("0000000000001-sess-b.json.tmp");
        host.put(&tmp_path, r#"{"session_id":"sess-b"}"#, ts(1));
        let (tx, rx) = mpsc::channel();
        assert!(poll_hooks_once(&host, &hooks_dir, &tx));
        assert!(drain(&rx).is_empty(), "must not emit for .tmp");
        // The .tmp stays in place — producer still owns it.
        let remaining = host.list_files(&hooks_dir).unwrap();
        assert!(remaining.iter().any(|p| p == &tmp_path));
    }

    #[test]
    fn poll_hooks_once_returns_true_when_dir_missing() {
        // Fresh host that has never fired a hook — list_files returns
        // empty, no events, no error.
        let host = MockHost::new("devbox");
        let (tx, rx) = mpsc::channel();
        assert!(poll_hooks_once(
            &host,
            Path::new("/r/.agent-mux-hooks"),
            &tx
        ));
        assert!(drain(&rx).is_empty());
    }

    #[test]
    fn poll_hooks_once_leaves_bad_payloads_on_disk_for_inspection() {
        let host = MockHost::new("devbox");
        let hooks_dir = PathBuf::from("/r/.agent-mux-hooks");
        let bad = hooks_dir.join("0000000000001-bad.json");
        host.put(&bad, "garbage", ts(1));
        let (tx, rx) = mpsc::channel();
        assert!(poll_hooks_once(&host, &hooks_dir, &tx));
        assert!(drain(&rx).is_empty(), "no event for unparseable payload");
        // Bad marker stays for a human to look at.
        assert!(host.list_files(&hooks_dir).unwrap().contains(&bad));
    }

    #[test]
    fn poll_hooks_once_returns_false_when_receiver_dropped() {
        let host = MockHost::new("devbox");
        let hooks_dir = PathBuf::from("/r/.agent-mux-hooks");
        host.put(
            &hooks_dir.join("0000000000001-s.json"),
            r#"{"session_id":"s","notification_type":"idle_prompt"}"#,
            ts(1),
        );
        let (tx, rx) = mpsc::channel::<WatcherEvent>();
        drop(rx);
        assert!(!poll_hooks_once(&host, &hooks_dir, &tx));
    }
}
