//! Off-thread, per-host construction of a [`Session`] from a transcript
//! path the watcher just announced.
//!
//! [`crate::discovery::build_session`] is not cheap on a remote host: it
//! reads the transcript in full, stats the `project_dir`, and reads both
//! `.agent-mux/task.toml` and the worktree `.git` pointer — four
//! sequential round-trips through the host's `ControlMaster`. Measured
//! 2026-10-01 against a Coder-proxied remote, that is ~450 ms of latency
//! per small read plus the transcript payload: **2.24 s for a single
//! 1.2 MB transcript**.
//!
//! Running that from the main loop's `WatcherEvent::NewTranscript` arm —
//! which is where it lived until this module existed — froze the whole
//! dashboard for the duration: no redraw, no keypress, once per newly
//! seen transcript. A cold cache on a fifteen-session host meant roughly
//! half a minute of dead UI at startup, which is the shape the user
//! reported. ARCHITECTURE.md's "no synchronous shell-outs from the UI
//! thread" rules it out on principle; the latency numbers make it a bug.
//!
//! The fix is the pattern `refresh_git_changed_files` already uses: do
//! the I/O on a background thread and return the result through the
//! watcher's event channel, so the main loop only ever handles finished
//! data. Two refinements on top of a bare `thread::spawn` per request:
//!
//! - **One worker per host, serial.** A poll tick that discovers fifteen
//!   unknown transcripts would otherwise fan fifteen concurrent reads
//!   onto a single SSH `ControlMaster`, which multiplexes them over one
//!   TCP connection anyway — the concurrency buys nothing and competes
//!   with the attention pollers for the same pipe. Serial per host keeps
//!   the ordering legible and the connection unsaturated.
//! - **In-flight de-duplication.** The local `notify` watcher re-fires
//!   `NewTranscript` for a file that is still being written (that is the
//!   documented retry mechanism for partially-written transcripts), so
//!   without a guard a single transcript could queue many identical
//!   builds. A path already in flight is dropped; once its result lands,
//!   a later event re-requests it, which is what preserves the retry.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::SystemTime;

use crate::agent::AgentKind;
use crate::discovery::build_session;
use crate::host::Host;
use crate::session::HostId;
use crate::watcher::WatcherEvent;

/// One unit of work for a host's builder thread.
struct BuildRequest {
    kind: AgentKind,
    path: PathBuf,
    mtime: SystemTime,
}

/// Owns the per-host builder threads and the in-flight set.
///
/// Threads are created lazily — a host that never announces a new
/// transcript never gets one — and terminate when this struct drops (the
/// request `Sender` goes with it) or when the watcher's event receiver
/// goes away, matching the teardown contract every other background
/// thread in the process uses.
pub struct SessionBuilders {
    events: Sender<WatcherEvent>,
    queues: HashMap<HostId, Sender<BuildRequest>>,
    in_flight: HashSet<(HostId, PathBuf)>,
}

impl SessionBuilders {
    #[must_use]
    pub fn new(events: Sender<WatcherEvent>) -> Self {
        Self {
            events,
            queues: HashMap::new(),
            in_flight: HashSet::new(),
        }
    }

    /// Queue a build of `path` against `host`. Returns `false` when the
    /// request was dropped because an identical one is already in flight.
    ///
    /// The result arrives later as [`WatcherEvent::SessionBuilt`], which
    /// the caller must pass to [`Self::finish`] to clear the in-flight
    /// marker — otherwise a transcript whose first build yielded nothing
    /// (a file still being written) could never be retried.
    pub fn request(
        &mut self,
        host_id: &HostId,
        host: &Arc<dyn Host>,
        kind: AgentKind,
        path: PathBuf,
        mtime: SystemTime,
    ) -> bool {
        let key = (host_id.clone(), path.clone());
        if self.in_flight.contains(&key) {
            return false;
        }
        let queue = self.queues.entry(host_id.clone()).or_insert_with(|| {
            let (tx, rx) = channel();
            let host = Arc::clone(host);
            let host_id = host_id.clone();
            let events = self.events.clone();
            thread::spawn(move || run_worker(&host, &host_id, &rx, &events));
            tx
        });
        if queue.send(BuildRequest { kind, path, mtime }).is_err() {
            // The worker is gone (its event receiver dropped — the app is
            // shutting down). Forget the queue so a later request doesn't
            // keep writing into a dead channel, and report the request as
            // not taken.
            self.queues.remove(host_id);
            return false;
        }
        self.in_flight.insert(key);
        true
    }

    /// Clear the in-flight marker for a finished build.
    pub fn finish(&mut self, host_id: &HostId, path: &Path) {
        self.in_flight
            .remove(&(host_id.clone(), path.to_path_buf()));
    }

    /// Whether a build for `path` on `host_id` is currently queued or
    /// running. Test/diagnostic surface.
    #[must_use]
    pub fn is_in_flight(&self, host_id: &HostId, path: &Path) -> bool {
        self.in_flight
            .contains(&(host_id.clone(), path.to_path_buf()))
    }
}

/// Body of a host's builder thread: build each queued transcript in turn
/// and post the outcome — including "nothing usable" — back to the main
/// loop.
fn run_worker(
    host: &Arc<dyn Host>,
    host_id: &HostId,
    rx: &Receiver<BuildRequest>,
    events: &Sender<WatcherEvent>,
) {
    while let Ok(req) = rx.recv() {
        // A read error and a deliberate filter are the same thing to the
        // caller — neither produces a row — so both collapse to `None`.
        // The distinction only matters for retry, and retry is driven by
        // the watcher re-announcing the path, not by this result.
        let session = build_session(host.as_ref(), &req.path, req.kind, req.mtime)
            .ok()
            .flatten();
        if events
            .send(WatcherEvent::SessionBuilt {
                host: host_id.clone(),
                path: req.path,
                session: session.map(Box::new),
            })
            .is_err()
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::LocalHost;
    use std::fs::{self, create_dir_all};
    use std::sync::mpsc;

    fn local() -> Arc<dyn Host> {
        Arc::new(LocalHost::new())
    }

    /// Drain every event currently queued, blocking briefly for the
    /// first so a worker thread has a chance to post its result.
    fn recv_built(rx: &mpsc::Receiver<WatcherEvent>) -> WatcherEvent {
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("builder should post a result")
    }

    #[test]
    fn builds_a_session_off_thread_and_reports_it() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("work");
        create_dir_all(&cwd).unwrap();
        let entry = tmp.path().join("projects").join("-work");
        create_dir_all(&entry).unwrap();
        let path = entry.join("abc.jsonl");
        fs::write(
            &path,
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"hi\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let (tx, rx) = mpsc::channel();
        let mut builders = SessionBuilders::new(tx);
        let host_id = HostId::local();
        assert!(builders.request(
            &host_id,
            &local(),
            AgentKind::Claude,
            path.clone(),
            SystemTime::now()
        ));

        match recv_built(&rx) {
            WatcherEvent::SessionBuilt {
                host,
                path: p,
                session,
            } => {
                assert_eq!(host, host_id);
                assert_eq!(p, path);
                let session = session.expect("usable transcript should build");
                assert_eq!(session.id.0, "abc");
                assert_eq!(session.project_dir, cwd);
            }
            other => panic!("expected SessionBuilt, got {other:?}"),
        }
    }

    #[test]
    fn a_filtered_transcript_still_reports_so_the_marker_clears() {
        // A stillborn transcript builds to nothing. The event must still
        // come back, carrying the path: it's what lets `finish` clear the
        // in-flight marker, and without it the path would be wedged as
        // "already building" for the life of the process and could never
        // be retried once the user actually typed into it.
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("work");
        create_dir_all(&cwd).unwrap();
        let entry = tmp.path().join("projects").join("-work");
        create_dir_all(&entry).unwrap();
        let path = entry.join("dead.jsonl");
        fs::write(
            &path,
            format!("{{\"type\":\"user\",\"cwd\":\"{}\"}}\n", cwd.display()),
        )
        .unwrap();

        let (tx, rx) = mpsc::channel();
        let mut builders = SessionBuilders::new(tx);
        let host_id = HostId::local();
        builders.request(
            &host_id,
            &local(),
            AgentKind::Claude,
            path.clone(),
            SystemTime::now(),
        );

        match recv_built(&rx) {
            WatcherEvent::SessionBuilt {
                path: p, session, ..
            } => {
                assert_eq!(p, path);
                assert!(session.is_none(), "stillborn transcript must not build");
            }
            other => panic!("expected SessionBuilt, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_requests_are_dropped_while_in_flight_and_allowed_after() {
        // The local `notify` watcher re-fires `NewTranscript` while a
        // file is still being written. Without the guard those pile up
        // into N identical builds; with it, only the first is taken, and
        // the path becomes requestable again once its result lands —
        // which is what preserves the partial-file retry contract.
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("work");
        create_dir_all(&cwd).unwrap();
        let entry = tmp.path().join("projects").join("-work");
        create_dir_all(&entry).unwrap();
        let path = entry.join("abc.jsonl");
        fs::write(
            &path,
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"hi\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let (tx, rx) = mpsc::channel();
        let mut builders = SessionBuilders::new(tx);
        let host_id = HostId::local();
        let now = SystemTime::now();

        assert!(builders.request(&host_id, &local(), AgentKind::Claude, path.clone(), now));
        assert!(
            !builders.request(&host_id, &local(), AgentKind::Claude, path.clone(), now),
            "a second request for an in-flight path must be dropped"
        );
        assert!(builders.is_in_flight(&host_id, &path));

        recv_built(&rx);
        builders.finish(&host_id, &path);
        assert!(!builders.is_in_flight(&host_id, &path));
        assert!(
            builders.request(&host_id, &local(), AgentKind::Claude, path, now),
            "a finished path must be requestable again so retries work"
        );
    }

    #[test]
    fn requests_for_different_hosts_do_not_collide() {
        // The in-flight key is (host, path), not path alone: the same
        // transcript path can legitimately exist on two hosts.
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("work");
        create_dir_all(&cwd).unwrap();
        let entry = tmp.path().join("projects").join("-work");
        create_dir_all(&entry).unwrap();
        let path = entry.join("abc.jsonl");
        fs::write(
            &path,
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":\"hi\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let (tx, _rx) = mpsc::channel();
        let mut builders = SessionBuilders::new(tx);
        let now = SystemTime::now();
        assert!(builders.request(
            &HostId::local(),
            &local(),
            AgentKind::Claude,
            path.clone(),
            now
        ));
        assert!(builders.request(
            &HostId("devbox".into()),
            &local(),
            AgentKind::Claude,
            path,
            now
        ));
    }
}
