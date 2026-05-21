//! M4 attention notifications.
//!
//! Fires an OS-level notification when a session transitions into
//! `NeedsInput` from any other attention state, so the user knows a
//! Claude Code conversation is waiting on them even when agent-mux's
//! window isn't on screen.
//!
//! Suppression has two layers, both per-session:
//!
//! 1. **Episodic flag** — once a notification fires, it stays
//!    suppressed until the session leaves `NeedsInput` (back to
//!    `Working`/`Idle`/`Unknown`). One ping per "the assistant just
//!    stopped" event; ignoring the notification while the session
//!    waits does not produce a second ping.
//! 2. **Time window** — even if the flag would otherwise allow a
//!    fire, refuse to re-fire within [`DEBOUNCE_WINDOW`] of the prior
//!    notification. This absorbs Working↔NeedsInput flapping at the
//!    watcher's poll cadence (~1–3s); a transcript that briefly
//!    flickered through Working would otherwise clear the flag and
//!    re-arm.
//!
//! The notifier owns the suppression state; the catalog owns the
//! attention itself. Wiring lives in `main.rs`, called from the
//! `WatcherEvent::Attention` handler — the catalog returns the
//! previous attention from `update_attention`, which is the signal
//! the notifier needs to recognise an actual transition.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use crate::config::{NotificationsBackend, NotificationsConfig};
use crate::session::{Attention, HostId, SessionId};

/// Same-session re-fire window. Notifications for the same `SessionId`
/// within this duration of the previous fire are suppressed even if the
/// episodic flag would otherwise allow them.
///
/// Five seconds is just above the watcher's polling cadence
/// (`REMOTE_POLL_INTERVAL` = 3s), which means one bad poll won't double-fire
/// but two genuine `NeedsInput` episodes spaced ≥5s apart still notify.
pub const DEBOUNCE_WINDOW: Duration = Duration::from_secs(5);

/// The notification payload handed to the dispatcher. Split into
/// `title` (loud) and `body` (context) because the OS surfaces those
/// differently across platforms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payload {
    pub title: String,
    pub body: String,
    /// Whether to request an audible cue from the OS notification
    /// system. Plumbed through the payload so the dispatcher can stay
    /// stateless — config decisions live with the [`Notifier`], not
    /// the backend.
    ///
    /// Ignored when `sound_file` is set — that path plays its file via
    /// a side-process player and silences the OS notification sound
    /// instead to avoid a double-up.
    pub sound: bool,
    /// Optional explicit audio file to play alongside a silent OS
    /// notification. When present, the dispatcher spawns a platform-
    /// appropriate player (`afplay` on macOS, `ffplay`/`paplay` on
    /// Linux) against this path and asks the OS notification not to
    /// play its own sound. Filed 2026-05-21 after dogfooding surfaced
    /// the macOS default sound as aggressive enough that the user kept
    /// `sound = true` off altogether — `sound_file` is the
    /// vocabulary-free alternative.
    pub sound_file: Option<PathBuf>,
}

/// Pluggable notification backend. Tests use a recorder to capture
/// every dispatch synchronously; production uses [`LibNotifyDispatcher`]
/// which shells out via `notify-rust`.
pub trait Dispatcher: Send + Sync {
    /// Hand a payload to the backend.
    ///
    /// # Errors
    ///
    /// Implementations return `Err` when they cannot enqueue the
    /// notification (missing daemon, malformed payload, etc.). The
    /// notifier treats failure as "did not fire" — suppression state
    /// is not armed, so the next attempt can try again.
    fn dispatch(&self, payload: Payload) -> Result<(), String>;
}

/// Production dispatcher backed by `notify-rust`. Spawns a one-shot
/// thread per notification so a slow D-Bus reply (Linux) or
/// `NSUserNotification` handoff (macOS) cannot back-pressure the UI
/// thread. Errors inside the spawned thread are swallowed — a missing
/// notification daemon is dogfood feedback, not a crash condition.
pub struct LibNotifyDispatcher;

impl Dispatcher for LibNotifyDispatcher {
    fn dispatch(&self, payload: Payload) -> Result<(), String> {
        let sound_file = payload.sound_file.clone();
        let request_default_sound = should_play_os_default(&payload);
        std::thread::spawn(move || {
            let mut n = notify_rust::Notification::new();
            n.summary(&payload.title)
                .body(&payload.body)
                .appname("agent-mux");
            if request_default_sound {
                n.sound_name("default");
            }
            let _ = n.show();
        });
        if let Some(path) = sound_file {
            play_sound_file(path);
        }
        Ok(())
    }
}

/// Whether the OS notification API should request its built-in default
/// sound. When the payload carries an explicit `sound_file`, we play
/// that file ourselves and ask the OS for silence; otherwise we honour
/// the `sound` toggle.
fn should_play_os_default(payload: &Payload) -> bool {
    payload.sound_file.is_none() && payload.sound
}

/// Spawn an OS-native audio player as a one-shot background thread to
/// play `path`. Used when a notification's payload carries an explicit
/// sound file — the file plays alongside a silent OS notification, so
/// the OS-notification-sound double-up is avoided.
///
/// Per-platform mechanics — see [`candidate_players`]. Errors are
/// swallowed inside the spawned thread; a missing player or unplayable
/// file is dogfood feedback, not a crash condition. The thread blocks
/// for the duration of playback (a few seconds at most for a typical
/// notification chime), but it's detached, so dispatcher latency stays
/// off the UI thread the same way the OS-notification spawn does.
pub fn play_sound_file(path: PathBuf) {
    std::thread::spawn(move || {
        for (program, args) in candidate_players(&path) {
            let status = Command::new(program)
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if matches!(status, Ok(s) if s.success()) {
                return;
            }
        }
    });
}

/// Per-platform list of player commands to try, in order. Each tuple is
/// `(program, argv)`; the first program found on `$PATH` that returns a
/// successful exit status wins.
///
/// macOS gets `afplay` (handles mp3/wav/aiff/m4a/caf out of the box,
/// ships with the OS). Linux/WSL tries `ffplay` first (most universal —
/// any ffmpeg install gets it) then `paplay` (`PulseAudio`, decodes
/// wav/ogg/flac without extra codecs). Other platforms return an empty
/// list, so [`play_sound_file`] is a no-op there.
#[must_use]
pub fn candidate_players(path: &Path) -> Vec<(&'static str, Vec<std::ffi::OsString>)> {
    let p = path.as_os_str().to_owned();
    if cfg!(target_os = "macos") {
        vec![("afplay", vec![p])]
    } else if cfg!(target_os = "linux") {
        vec![
            (
                "ffplay",
                vec![
                    "-nodisp".into(),
                    "-autoexit".into(),
                    "-loglevel".into(),
                    "quiet".into(),
                    p.clone(),
                ],
            ),
            ("paplay", vec![p]),
        ]
    } else {
        vec![]
    }
}

/// macOS dispatcher that shells out to `osascript -e 'display
/// notification …'`. Bypasses the `NSUserNotification` handoff that
/// `notify-rust` uses, which 2026-05-20 dogfooding confirmed was
/// unreliable on the user's setup. `osascript` ships with every Mac;
/// the dispatch is one fork+exec per notification, fire-and-forget on
/// a background thread so a slow Notification Center reply can't
/// back-pressure the UI thread.
pub struct OsascriptDispatcher;

impl Dispatcher for OsascriptDispatcher {
    fn dispatch(&self, payload: Payload) -> Result<(), String> {
        let sound_file = payload.sound_file.clone();
        let script = build_osascript(&payload);
        std::thread::spawn(move || {
            let _ = Command::new("osascript")
                .arg("-e")
                .arg(&script)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        });
        if let Some(path) = sound_file {
            play_sound_file(path);
        }
        Ok(())
    }
}

/// Compose the `AppleScript` source for one `display notification` call.
/// Lifted out of [`OsascriptDispatcher::dispatch`] so the
/// title/body escape contract is unit-testable without mocking the
/// `osascript` subprocess. Literal double-quotes in either field are
/// escaped so they can't break the surrounding string literal.
///
/// When `payload.sound_file` is set, the script omits the `sound name`
/// clause — the file plays via the side-process player path and the OS
/// notification stays silent to avoid a double-up.
#[must_use]
fn build_osascript(payload: &Payload) -> String {
    let title = payload.title.replace('"', "\\\"");
    let body = payload.body.replace('"', "\\\"");
    let mut script = format!("display notification \"{body}\" with title \"{title}\"");
    if should_play_os_default(payload) {
        script.push_str(" sound name \"default\"");
    }
    script
}

/// WSL dispatcher that shells out to `wsl-notify-send.exe` on the
/// Windows side. Requires the binary on the user's Windows `PATH`
/// (installed separately — see <https://github.com/stuartleeks/wsl-notify-send>).
/// Bypasses Linux D-Bus, which 2026-05-20 dogfooding confirmed is
/// fragile under `WSLg`.
pub struct WslToastDispatcher;

impl Dispatcher for WslToastDispatcher {
    fn dispatch(&self, payload: Payload) -> Result<(), String> {
        let sound_file = payload.sound_file.clone();
        std::thread::spawn(move || {
            // `wsl-notify-send.exe` flags: `--category` is the title;
            // the positional is the body. Older versions accept the
            // same shape; if a user has a different fork, an explicit
            // backend = "dbus" override gets them off this path.
            let _ = Command::new("wsl-notify-send.exe")
                .arg("--category")
                .arg(&payload.title)
                .arg(&payload.body)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        });
        if let Some(path) = sound_file {
            play_sound_file(path);
        }
        Ok(())
    }
}

/// Resolve a [`NotificationsBackend`] config value to a concrete
/// [`Dispatcher`] plus a short human label for the startup log. `Auto`
/// runs the platform probe described on the enum's docs; explicit
/// variants pass through verbatim.
///
/// The label is the value the user would put in `~/.config/agent-mux/config.toml`
/// (e.g. `"dbus"` for [`NotificationsBackend::Dbus`]) so the startup
/// banner doubles as a hint for opting out of `Auto` if the probe
/// misfires.
#[must_use]
pub fn pick_dispatcher(backend: NotificationsBackend) -> (Box<dyn Dispatcher>, &'static str) {
    let resolved = match backend {
        NotificationsBackend::Auto => detect_default_backend(),
        other => other,
    };
    match resolved {
        NotificationsBackend::Auto => unreachable!("detect_default_backend never returns Auto"),
        NotificationsBackend::Dbus => (Box::new(LibNotifyDispatcher), "dbus"),
        NotificationsBackend::Osascript => (Box::new(OsascriptDispatcher), "osascript"),
        NotificationsBackend::WslToast => (Box::new(WslToastDispatcher), "wsl-toast"),
    }
}

/// Auto-detect rule for `backend = "auto"`. Order matters: macOS check
/// first (it's the strictest cfg gate), then WSL (which is also Linux
/// but with a tell in `/proc/sys/kernel/osrelease`), then plain Linux.
#[must_use]
fn detect_default_backend() -> NotificationsBackend {
    if cfg!(target_os = "macos") {
        return NotificationsBackend::Osascript;
    }
    if is_wsl() {
        return NotificationsBackend::WslToast;
    }
    NotificationsBackend::Dbus
}

/// True when running under Microsoft's WSL (1 or 2). The kernel's
/// osrelease string carries `Microsoft` (WSL1) or `microsoft-standard-WSL2`
/// (WSL2); both are detected.
#[must_use]
fn is_wsl() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| {
            let lower = s.to_ascii_lowercase();
            lower.contains("microsoft") || lower.contains("wsl")
        })
        .unwrap_or(false)
}

#[derive(Debug, Clone, Default)]
struct SessionState {
    last_fired: Option<SystemTime>,
    fired_for_current_episode: bool,
}

pub struct Notifier {
    dispatcher: Box<dyn Dispatcher>,
    state: HashMap<SessionId, SessionState>,
    config: NotificationsConfig,
}

impl Notifier {
    #[must_use]
    pub fn new(dispatcher: Box<dyn Dispatcher>, config: NotificationsConfig) -> Self {
        Self {
            dispatcher,
            state: HashMap::new(),
            config,
        }
    }

    /// Replace the live notification config. Used by the M5
    /// reload-on-edit path so a config change takes effect without a
    /// restart. Suppression state is intentionally preserved — a
    /// reload should not re-fire already-acknowledged notifications.
    pub fn update_config(&mut self, config: NotificationsConfig) {
        self.config = config;
    }

    /// Drop bookkeeping for a session that no longer exists. The main
    /// loop calls this on catalog reconciliations that drop entries,
    /// preventing the `state` map from growing without bound across a
    /// long-lived process.
    pub fn forget(&mut self, id: &SessionId) {
        self.state.remove(id);
    }

    /// Called at the catalog's attention-update boundary.
    ///
    /// Fires a notification iff:
    /// - `new == NeedsInput` and `prev != NeedsInput` (an actual transition in),
    /// - the session's episodic flag is clear (no notification has
    ///   fired for this `NeedsInput` episode),
    /// - and the time-window debounce has elapsed since the last fire
    ///   for this session.
    ///
    /// `now` is passed in (rather than fetched here) so tests can drive
    /// the time-window logic deterministically.
    pub fn on_attention_update(&mut self, t: &Transition<'_>, now: SystemTime) {
        // Leaving-NeedsInput bookkeeping runs even when the master
        // toggle is off: turning notifications back on later should
        // not see a stale "still in episode" flag from before the toggle.
        if t.new != Attention::NeedsInput {
            if t.prev == Attention::NeedsInput
                && let Some(s) = self.state.get_mut(t.id)
            {
                s.fired_for_current_episode = false;
            }
            return;
        }
        if t.prev == Attention::NeedsInput {
            return;
        }
        if !self.config.enabled {
            return;
        }
        if self
            .config
            .disabled_hosts
            .iter()
            .any(|h| h == t.host.as_str())
        {
            return;
        }
        let entry = self.state.entry(t.id.clone()).or_default();
        if entry.fired_for_current_episode {
            return;
        }
        if let Some(last) = entry.last_fired
            && now
                .duration_since(last)
                .ok()
                .is_some_and(|d| d < DEBOUNCE_WINDOW)
        {
            return;
        }
        let payload = Payload {
            title: format!("agent-mux: {}", t.title),
            body: format!("{} · {}", t.host, t.project.display()),
            sound: self.config.sound,
            sound_file: self.config.sound_file.clone(),
        };
        if self.dispatcher.dispatch(payload).is_ok() {
            entry.last_fired = Some(now);
            entry.fired_for_current_episode = true;
        }
    }

    /// Build the payload the notifier would dispatch for a synthetic
    /// transition. Used by the `notify-test` subcommand to render and
    /// fire a one-off notification matching the user's current config
    /// without provoking a real session transition. Pulled out of the
    /// dispatch path so the test subcommand can introspect the payload
    /// (e.g. log what it sent) before handing it to the dispatcher.
    #[must_use]
    pub fn test_payload(&self, title: &str, host: &str, project: &Path) -> Payload {
        Payload {
            title: format!("agent-mux: {title}"),
            body: format!("{host} · {}", project.display()),
            sound: self.config.sound,
            sound_file: self.config.sound_file.clone(),
        }
    }

    /// Hand a payload directly to the dispatcher, bypassing the
    /// per-session suppression bookkeeping. Used by the `notify-test`
    /// subcommand for the end-to-end verification path: the user wants
    /// the test fire to *actually* fire even if a real notification
    /// just landed for the same dummy session.
    ///
    /// # Errors
    ///
    /// Propagates the dispatcher's error string verbatim.
    pub fn dispatch_test(&self, payload: Payload) -> Result<(), String> {
        self.dispatcher.dispatch(payload)
    }
}

/// One attention transition handed to [`Notifier::on_attention_update`].
/// Bundles the session-derived display labels alongside the transition
/// itself so the notifier signature stays compact and the caller can
/// build the struct directly from a catalog entry.
pub struct Transition<'a> {
    pub id: &'a SessionId,
    pub prev: Attention,
    pub new: Attention,
    pub title: &'a str,
    pub host: &'a HostId,
    pub project: &'a Path,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn pick_dispatcher_explicit_dbus_returns_dbus_label() {
        let (_, label) = pick_dispatcher(NotificationsBackend::Dbus);
        assert_eq!(label, "dbus");
    }

    #[test]
    fn pick_dispatcher_explicit_osascript_returns_osascript_label() {
        let (_, label) = pick_dispatcher(NotificationsBackend::Osascript);
        assert_eq!(label, "osascript");
    }

    #[test]
    fn pick_dispatcher_explicit_wsl_toast_returns_wsl_toast_label() {
        let (_, label) = pick_dispatcher(NotificationsBackend::WslToast);
        assert_eq!(label, "wsl-toast");
    }

    fn payload(title: &str, body: &str, sound: bool) -> Payload {
        Payload {
            title: title.into(),
            body: body.into(),
            sound,
            sound_file: None,
        }
    }

    #[test]
    fn build_osascript_escapes_embedded_double_quotes_in_title_and_body() {
        // A session title with a literal `"` would otherwise close the
        // surrounding string in the script source, breaking syntax.
        let payload = payload(
            r#"refactor "preview""#,
            r#"local · /tmp/dir "with quotes""#,
            false,
        );
        let script = build_osascript(&payload);
        assert!(
            script.contains(r#"\"preview\""#),
            "title quote not escaped:\n{script}",
        );
        assert!(
            script.contains(r#"\"with quotes\""#),
            "body quote not escaped:\n{script}",
        );
    }

    #[test]
    fn build_osascript_appends_sound_name_when_sound_requested() {
        let payload = payload("x", "y", true);
        assert!(
            build_osascript(&payload).contains("sound name \"default\""),
            "sound clause missing",
        );
    }

    #[test]
    fn build_osascript_omits_sound_name_when_sound_file_takes_over() {
        // sound_file owns the audio cue when set — the OS notification
        // must stay silent so the user doesn't hear the file plus the
        // default chime on top of each other.
        let mut p = payload("x", "y", true);
        p.sound_file = Some(PathBuf::from("/System/Library/Sounds/Tink.aiff"));
        assert!(
            !build_osascript(&p).contains("sound name"),
            "sound_file should silence the OS default clause:\n{}",
            build_osascript(&p),
        );
    }

    #[test]
    fn build_osascript_omits_sound_name_when_silent() {
        let payload = payload("x", "y", false);
        assert!(
            !build_osascript(&payload).contains("sound name"),
            "sound clause should be absent",
        );
    }

    #[test]
    fn pick_dispatcher_auto_returns_one_of_the_real_backends() {
        // Auto runs the platform probe; on every supported host we
        // should land on one of the three concrete labels, never on
        // "auto" (which would imply detect_default_backend leaked the
        // input variant back out).
        let (_, label) = pick_dispatcher(NotificationsBackend::Auto);
        assert!(
            matches!(label, "dbus" | "osascript" | "wsl-toast"),
            "got: {label}",
        );
    }

    /// Test dispatcher that records every payload it receives. Wrapped
    /// in `Arc<Mutex<…>>` so test code can inspect the log after the
    /// `Notifier` has dropped the `Box<dyn Dispatcher>`.
    #[derive(Default)]
    struct RecorderDispatcher {
        log: Mutex<Vec<Payload>>,
    }

    impl Dispatcher for RecorderDispatcher {
        fn dispatch(&self, payload: Payload) -> Result<(), String> {
            self.log.lock().unwrap().push(payload);
            Ok(())
        }
    }

    /// Newtype that forwards `dispatch` to an `Arc<RecorderDispatcher>`,
    /// letting tests share a recorder between a `Box<dyn Dispatcher>`
    /// owned by the notifier and an inspection handle owned by the test.
    struct SharedRecorder(std::sync::Arc<RecorderDispatcher>);

    impl Dispatcher for SharedRecorder {
        fn dispatch(&self, p: Payload) -> Result<(), String> {
            self.0.dispatch(p)
        }
    }

    /// Dispatcher whose every call fails. Used to verify that a failing
    /// dispatch does not poison the suppression state (the flag and
    /// timestamp only update on success — otherwise a transient libnotify
    /// outage would silently mute the user).
    struct FailingDispatcher;

    impl Dispatcher for FailingDispatcher {
        fn dispatch(&self, _: Payload) -> Result<(), String> {
            Err("backend down".to_string())
        }
    }

    fn sid(s: &str) -> SessionId {
        SessionId(s.to_string())
    }

    fn local() -> HostId {
        HostId::local()
    }

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// Helper: build a `Notifier` with default (enabled, no sound, no
    /// host disables) config and return the dispatcher's log handle
    /// alongside it so each test can inspect the dispatches directly.
    fn notifier_with_log() -> (Notifier, std::sync::Arc<RecorderDispatcher>) {
        notifier_with_log_and_config(NotificationsConfig::default())
    }

    fn notifier_with_log_and_config(
        config: NotificationsConfig,
    ) -> (Notifier, std::sync::Arc<RecorderDispatcher>) {
        let rec = std::sync::Arc::new(RecorderDispatcher::default());
        let notifier = Notifier::new(
            Box::new(SharedRecorder(std::sync::Arc::clone(&rec))),
            config,
        );
        (notifier, rec)
    }

    /// Test helper that mirrors the wire-up shape in main.rs — bundles
    /// the seven leaf values into a `Transition` and forwards to
    /// `on_attention_update`. Lets each test express one call as one
    /// readable line.
    #[allow(clippy::too_many_arguments)]
    fn fire(
        n: &mut Notifier,
        id: &SessionId,
        prev: Attention,
        new: Attention,
        title: &str,
        host: &HostId,
        project: &Path,
        now: SystemTime,
    ) {
        n.on_attention_update(
            &Transition {
                id,
                prev,
                new,
                title,
                host,
                project,
            },
            now,
        );
    }

    fn log_titles(rec: &RecorderDispatcher) -> Vec<String> {
        rec.log
            .lock()
            .unwrap()
            .iter()
            .map(|p| p.title.clone())
            .collect()
    }

    #[test]
    fn fires_on_working_to_needs_input_transition() {
        let (mut n, rec) = notifier_with_log();
        fire(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "refactor parser",
            &local(),
            Path::new("/proj"),
            at(100),
        );
        assert_eq!(log_titles(&rec), vec!["agent-mux: refactor parser"]);
    }

    #[test]
    fn fires_on_idle_to_needs_input_transition() {
        let (mut n, rec) = notifier_with_log();
        fire(
            &mut n,
            &sid("a"),
            Attention::Idle,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(100),
        );
        assert_eq!(rec.log.lock().unwrap().len(), 1);
    }

    #[test]
    fn fires_on_unknown_to_needs_input_transition() {
        let (mut n, rec) = notifier_with_log();
        fire(
            &mut n,
            &sid("a"),
            Attention::Unknown,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(100),
        );
        assert_eq!(rec.log.lock().unwrap().len(), 1);
    }

    #[test]
    fn does_not_fire_when_new_state_is_not_needs_input() {
        let (mut n, rec) = notifier_with_log();
        for new in [Attention::Working, Attention::Idle, Attention::Unknown] {
            fire(
                &mut n,
                &sid("a"),
                Attention::NeedsInput,
                new,
                "x",
                &local(),
                Path::new("/p"),
                at(100),
            );
        }
        assert!(rec.log.lock().unwrap().is_empty());
    }

    #[test]
    fn does_not_fire_when_prev_state_was_already_needs_input() {
        let (mut n, rec) = notifier_with_log();
        // Catalog re-derived the same state; not a transition.
        fire(
            &mut n,
            &sid("a"),
            Attention::NeedsInput,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(100),
        );
        assert!(rec.log.lock().unwrap().is_empty());
    }

    #[test]
    fn episodic_flag_suppresses_refire_until_session_leaves_needs_input() {
        // Realistic flow: notify on entry → assistant produces tool_use
        // (Working) → assistant produces another text response stopping
        // (NeedsInput). Without the flag-clear, the second NeedsInput
        // would refire. The flag-clear only happens when we observe a
        // leaving-NeedsInput transition.
        let (mut n, rec) = notifier_with_log();

        // First entry — fires.
        fire(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(100),
        );

        // Session sits in NeedsInput for ten seconds (past the debounce
        // window). The user hasn't acted yet. No second fire.
        fire(
            &mut n,
            &sid("a"),
            Attention::NeedsInput,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(120),
        );

        assert_eq!(rec.log.lock().unwrap().len(), 1);
    }

    #[test]
    fn refires_after_session_leaves_and_returns_to_needs_input() {
        let (mut n, rec) = notifier_with_log();
        // Episode 1: enter → fire.
        fire(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(0),
        );
        // Leave NeedsInput (user attached and replied → assistant working).
        fire(
            &mut n,
            &sid("a"),
            Attention::NeedsInput,
            Attention::Working,
            "x",
            &local(),
            Path::new("/p"),
            at(60),
        );
        // Episode 2: assistant stopped again → fire again. Well past the
        // debounce window so only the episodic flag's reset can let this
        // through.
        fire(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(120),
        );
        assert_eq!(rec.log.lock().unwrap().len(), 2);
    }

    #[test]
    fn debounce_window_suppresses_rapid_flapping_even_when_flag_clears() {
        // The watcher might briefly flick Working between two NeedsInput
        // events. Without the time-window debounce, the leaving-state
        // event would clear the flag and the next event would re-fire.
        let (mut n, rec) = notifier_with_log();

        // T=0: fire.
        fire(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(0),
        );
        // T=1: flap back to Working (clears flag).
        fire(
            &mut n,
            &sid("a"),
            Attention::NeedsInput,
            Attention::Working,
            "x",
            &local(),
            Path::new("/p"),
            at(1),
        );
        // T=2: flap back to NeedsInput. Flag is clear, but the time
        // window says "you fired 2s ago, hush."
        fire(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(2),
        );
        assert_eq!(rec.log.lock().unwrap().len(), 1);
    }

    #[test]
    fn debounce_window_does_not_block_genuine_episode_past_the_window() {
        let (mut n, rec) = notifier_with_log();
        fire(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(0),
        );
        fire(
            &mut n,
            &sid("a"),
            Attention::NeedsInput,
            Attention::Working,
            "x",
            &local(),
            Path::new("/p"),
            at(1),
        );
        // T=10: well past DEBOUNCE_WINDOW (5s).
        fire(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(10),
        );
        assert_eq!(rec.log.lock().unwrap().len(), 2);
    }

    #[test]
    fn debounce_does_not_cross_sessions() {
        // A's recent fire must not suppress B's first fire.
        let (mut n, rec) = notifier_with_log();
        fire(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(0),
        );
        fire(
            &mut n,
            &sid("b"),
            Attention::Working,
            Attention::NeedsInput,
            "y",
            &local(),
            Path::new("/p"),
            at(1),
        );
        assert_eq!(rec.log.lock().unwrap().len(), 2);
    }

    #[test]
    fn payload_includes_session_title_host_and_project() {
        let (mut n, rec) = notifier_with_log();
        fire(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "refactor parser",
            &HostId("alpenglow".to_string()),
            Path::new("/home/user/work/mux"),
            at(0),
        );
        let log = rec.log.lock().unwrap();
        assert_eq!(log[0].title, "agent-mux: refactor parser");
        assert_eq!(log[0].body, "alpenglow · /home/user/work/mux");
    }

    #[test]
    fn failed_dispatch_does_not_arm_suppression_so_next_attempt_can_fire() {
        // A transient backend outage must not mute the user. We model
        // this by failing once, then succeeding: the second attempt
        // should fire even though the first appeared to "fire" from
        // the caller's perspective.
        let mut n = Notifier::new(Box::new(FailingDispatcher), NotificationsConfig::default());
        fire(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(0),
        );
        // Swap in a recorder; if suppression were armed we'd see zero
        // dispatches. (We construct fresh state to simulate "backend
        // came back" — same Notifier instance, same session id, but
        // its state map shouldn't have been touched by the failed call.)
        let rec = std::sync::Arc::new(RecorderDispatcher::default());
        // Replace the dispatcher behind the existing notifier.
        n.dispatcher = Box::new(SharedRecorder(std::sync::Arc::clone(&rec)));
        fire(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(0),
        );
        assert_eq!(rec.log.lock().unwrap().len(), 1);
    }

    #[test]
    fn does_not_fire_when_master_toggle_is_disabled() {
        let cfg = NotificationsConfig {
            enabled: false,
            ..NotificationsConfig::default()
        };
        let (mut n, rec) = notifier_with_log_and_config(cfg);
        fire(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(0),
        );
        assert!(rec.log.lock().unwrap().is_empty());
    }

    #[test]
    fn does_not_fire_for_a_disabled_host() {
        let cfg = NotificationsConfig {
            disabled_hosts: vec!["alpenglow".to_string()],
            ..NotificationsConfig::default()
        };
        let (mut n, rec) = notifier_with_log_and_config(cfg);
        fire(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &HostId("alpenglow".to_string()),
            Path::new("/p"),
            at(0),
        );
        // Different host on the same notifier still fires.
        fire(
            &mut n,
            &sid("b"),
            Attention::Working,
            Attention::NeedsInput,
            "y",
            &local(),
            Path::new("/p"),
            at(0),
        );
        let log = rec.log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].title, "agent-mux: y");
    }

    #[test]
    fn payload_carries_sound_flag_from_config() {
        let cfg = NotificationsConfig {
            sound: true,
            ..NotificationsConfig::default()
        };
        let (mut n, rec) = notifier_with_log_and_config(cfg);
        fire(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(0),
        );
        assert!(rec.log.lock().unwrap()[0].sound);
    }

    #[test]
    fn payload_sound_defaults_to_false() {
        let (mut n, rec) = notifier_with_log();
        fire(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(0),
        );
        assert!(!rec.log.lock().unwrap()[0].sound);
    }

    #[test]
    fn update_config_replaces_live_config_without_resetting_state() {
        // Start enabled, fire once → flag set. Disable via reload.
        // The leaving-NeedsInput bookkeeping should still clear the
        // flag so when the user re-enables later, a fresh episode
        // notifies. (Disabling doesn't reset suppression state.)
        let (mut n, rec) = notifier_with_log();
        fire(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(0),
        );
        assert_eq!(rec.log.lock().unwrap().len(), 1);
        n.update_config(NotificationsConfig {
            enabled: false,
            ..NotificationsConfig::default()
        });
        // Bookkeeping still runs while disabled.
        fire(
            &mut n,
            &sid("a"),
            Attention::NeedsInput,
            Attention::Working,
            "x",
            &local(),
            Path::new("/p"),
            at(60),
        );
        // Re-enable, then a new transition INTO NeedsInput must fire.
        n.update_config(NotificationsConfig::default());
        fire(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(120),
        );
        assert_eq!(rec.log.lock().unwrap().len(), 2);
    }

    #[test]
    fn forget_clears_state_so_next_transition_can_fire_without_debounce() {
        let (mut n, rec) = notifier_with_log();
        fire(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(0),
        );
        n.forget(&sid("a"));
        // T=1: would normally be suppressed by the time window, but
        // forget wiped the state.
        fire(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(1),
        );
        assert_eq!(rec.log.lock().unwrap().len(), 2);
    }

    #[test]
    fn payload_propagates_sound_file_from_config() {
        let cfg = NotificationsConfig {
            sound_file: Some(PathBuf::from("/abs/path/ping.mp3")),
            ..NotificationsConfig::default()
        };
        let (mut n, rec) = notifier_with_log_and_config(cfg);
        fire(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(0),
        );
        let log = rec.log.lock().unwrap();
        assert_eq!(
            log[0].sound_file.as_deref(),
            Some(Path::new("/abs/path/ping.mp3"))
        );
    }

    #[test]
    fn payload_sound_file_defaults_to_none() {
        let (mut n, rec) = notifier_with_log();
        fire(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(0),
        );
        assert!(rec.log.lock().unwrap()[0].sound_file.is_none());
    }

    #[test]
    fn should_play_os_default_returns_true_only_when_sound_true_and_no_file() {
        let no_file = Payload {
            title: "x".into(),
            body: "y".into(),
            sound: true,
            sound_file: None,
        };
        assert!(should_play_os_default(&no_file));

        let with_file = Payload {
            sound_file: Some(PathBuf::from("/tmp/x.mp3")),
            ..no_file.clone()
        };
        assert!(
            !should_play_os_default(&with_file),
            "sound_file presence must silence the OS default"
        );

        let silent = Payload {
            sound: false,
            sound_file: None,
            ..no_file
        };
        assert!(!should_play_os_default(&silent));
    }

    #[test]
    fn candidate_players_on_macos_uses_afplay() {
        // Pinned per-platform so a future edit to the player list (e.g.
        // adding a fallback) preserves the headline tool the rest of
        // the design assumes is present on every Mac.
        if !cfg!(target_os = "macos") {
            return;
        }
        let players = candidate_players(Path::new("/abs/sound.aiff"));
        assert!(
            players.iter().any(|(p, _)| *p == "afplay"),
            "macos missing afplay in {players:?}"
        );
    }

    #[test]
    fn candidate_players_on_linux_tries_ffplay_first_then_paplay() {
        if !cfg!(target_os = "linux") {
            return;
        }
        let players = candidate_players(Path::new("/abs/sound.ogg"));
        let names: Vec<&str> = players.iter().map(|(p, _)| *p).collect();
        assert_eq!(
            names,
            vec!["ffplay", "paplay"],
            "Linux player order must keep ffplay primary (broadest codec coverage)"
        );
    }

    #[test]
    fn test_payload_uses_current_config_sound_and_file() {
        let cfg = NotificationsConfig {
            sound: true,
            sound_file: Some(PathBuf::from("/abs/ding.mp3")),
            ..NotificationsConfig::default()
        };
        let (n, _) = notifier_with_log_and_config(cfg);
        let p = n.test_payload("preview", "alpenglow", Path::new("/work/mux"));
        assert_eq!(p.title, "agent-mux: preview");
        assert_eq!(p.body, "alpenglow · /work/mux");
        assert!(p.sound);
        assert_eq!(p.sound_file.as_deref(), Some(Path::new("/abs/ding.mp3")));
    }
}
