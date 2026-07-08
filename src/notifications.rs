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
    /// Whether this notification is for a *blocking* prompt — a
    /// permission request or elicitation dialog the agent is stuck on,
    /// as opposed to a finished turn / idle nudge. Drives the Linux
    /// urgency hint: `true` → [`notify_rust::Urgency::Critical`] (the
    /// toast stays on screen until dismissed, since the agent can't
    /// proceed without an answer), `false` → the default expiring
    /// toast. Ignored by the macOS/WSL backends, which have no urgency
    /// knob.
    pub blocking: bool,
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

    /// Synchronous dispatch for one-shot CLI use (`notify-test`). The
    /// CLI process exits as soon as the subcommand returns, so the
    /// default `dispatch` — which spawns the subprocess work onto a
    /// detached thread — loses a race: the parent dies before the
    /// thread reaches `fork+exec`, and no notification ever fires.
    ///
    /// Real backends override this to run the subprocess invocation in
    /// the caller's thread (for the notification daemon shell-out) and
    /// to use `Command::spawn` instead of `Command::status` for any
    /// audio player (so the child is fully forked before this returns
    /// but the CLI doesn't block for the full playback duration).
    ///
    /// The default implementation falls through to `dispatch`, which is
    /// correct for synchronous test recorders.
    ///
    /// # Errors
    ///
    /// Same contract as `dispatch`.
    fn dispatch_blocking(&self, payload: Payload) -> Result<(), String> {
        self.dispatch(payload)
    }
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
            apply_urgency(&mut n, payload.blocking);
            let _ = n.show();
        });
        if let Some(path) = sound_file {
            play_sound_file(path);
        }
        Ok(())
    }

    fn dispatch_blocking(&self, payload: Payload) -> Result<(), String> {
        let request_default_sound = should_play_os_default(&payload);
        let mut n = notify_rust::Notification::new();
        n.summary(&payload.title)
            .body(&payload.body)
            .appname("agent-mux");
        if request_default_sound {
            n.sound_name("default");
        }
        apply_urgency(&mut n, payload.blocking);
        let _ = n.show();
        if let Some(path) = payload.sound_file {
            play_sound_file_blocking(&path);
        }
        Ok(())
    }
}

/// Set the XDG urgency hint on a `notify-rust` notification: a blocking
/// prompt becomes `Critical` (stays on screen until the user dismisses
/// it — the agent is stuck until answered), everything else stays at
/// the default expiring urgency. The `urgency` method only exists on
/// Linux/BSD and Windows in notify-rust (macOS has no urgency knob), so
/// the call is `cfg`-gated; on macOS this is a no-op, which is correct
/// because the macOS path uses [`OsascriptDispatcher`], not this one.
#[allow(unused_variables)]
fn apply_urgency(n: &mut notify_rust::Notification, blocking: bool) {
    #[cfg(all(unix, not(target_os = "macos")))]
    if blocking {
        n.urgency(notify_rust::Urgency::Critical);
    }
}

/// Longest hook `message` we surface as body text. Claude Code prompts
/// are short ("Claude needs your permission to use Bash"), but an
/// elicitation question can run long; clipping keeps the toast to a
/// scannable line rather than a wall of text.
const BODY_MESSAGE_CLIP: usize = 140;

/// Build the `(title, body)` pair for a `NeedsInput` notification.
/// Pulled out of the dispatch path so the formatting is unit-testable
/// and shared with [`Notifier::test_payload`].
///
/// - **Title** leads with the session `name` (the same label the
///   sidebar shows) and a state suffix, so the user can triage *which*
///   session and *how urgent* without opening the dashboard: a blocking
///   prompt reads "needs your input" (the agent is stuck on a
///   permission / elicitation answer), everything else "finished" (turn
///   ended, or an idle nudge). No `agent-mux:` prefix — `notify-rust`
///   already sets the app name, so the prefix only wasted the line the
///   user scans first.
/// - **Body** prefers the hook `message` — the actual prompt text,
///   which is the single most useful datum — and falls back to the
///   project's basename when there's no hook (the heuristic path). The
///   host is appended only when remote; `local` is noise.
#[must_use]
fn format_notification(
    name: &str,
    host: &HostId,
    project: &Path,
    blocking: bool,
    message: Option<&str>,
) -> (String, String) {
    let state = if blocking {
        "needs your input"
    } else {
        "finished"
    };
    let title = format!("{name} — {state}");

    let mut body = match message.map(str::trim).filter(|m| !m.is_empty()) {
        Some(msg) => clip_message(msg),
        None => project
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    if !host.is_local() {
        if body.is_empty() {
            body = host.as_str().to_string();
        } else {
            body.push_str(" · ");
            body.push_str(host.as_str());
        }
    }
    (title, body)
}

/// Collapse a hook message to a single scannable line: internal
/// whitespace (hook messages can carry newlines) becomes single spaces,
/// and anything past [`BODY_MESSAGE_CLIP`] characters is truncated with
/// an ellipsis. Counts by `char` so a multibyte boundary is never split.
#[must_use]
fn clip_message(msg: &str) -> String {
    let collapsed = msg.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= BODY_MESSAGE_CLIP {
        return collapsed;
    }
    let truncated: String = collapsed.chars().take(BODY_MESSAGE_CLIP - 1).collect();
    format!("{truncated}\u{2026}")
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

/// Blocking-CLI counterpart to [`play_sound_file`]. Forks the player via
/// `Command::spawn` (which returns once the child has `fork+exec`'d) and
/// drops the `Child` handle so playback continues independently after
/// the parent CLI process exits. Used by `Dispatcher::dispatch_blocking`
/// from the `notify-test` subcommand, where the detached-thread strategy
/// of [`play_sound_file`] would lose a race against the parent's exit
/// and the user would never hear the sound.
///
/// Returns once the first player on `$PATH` has been successfully
/// spawned; later candidates are not tried. If none can be spawned the
/// function returns having played nothing — matching `play_sound_file`'s
/// best-effort contract.
pub fn play_sound_file_blocking(path: &Path) {
    for (program, args) in candidate_players(path) {
        if Command::new(program)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
        {
            return;
        }
    }
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

    fn dispatch_blocking(&self, payload: Payload) -> Result<(), String> {
        let script = build_osascript(&payload);
        let _ = Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Some(path) = payload.sound_file {
            play_sound_file_blocking(&path);
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

/// Fold non-ASCII characters to ASCII analogs for the WSL toast path.
///
/// WSL→Windows argv conversion goes through NT's wide-arg layer under
/// the active console code page; non-ASCII bytes the user (or we) put
/// in `Payload::title`/`body` arrive at `wsl-notify-send.exe` mojibake'd.
/// 2026-05-23 dogfooding caught the production body's `·` (U+00B7,
/// from `format!("{host} · {project}", ...)`) rendering as garbage in
/// the resulting toast. Mapping the common typographic offenders to
/// ASCII before the spawn fixes both the separator we control and any
/// smart-quote/em-dash a user puts in a task title; unknown non-ASCII
/// falls back to `?` so the toast stays readable rather than corrupt.
#[must_use]
fn ascii_fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\u{00B7}' => out.push('|'),
            '\u{2013}' | '\u{2014}' => out.push('-'),
            '\u{2018}' | '\u{2019}' => out.push('\''),
            '\u{201C}' | '\u{201D}' => out.push('"'),
            '\u{2026}' => out.push_str("..."),
            c if c.is_ascii() => out.push(c),
            _ => out.push('?'),
        }
    }
    out
}

/// WSL dispatcher that shells out to `wsl-notify-send.exe` on the
/// Windows side. Requires the binary on the user's Windows `PATH`
/// (installed separately — see <https://github.com/stuartleeks/wsl-notify-send>).
/// Bypasses Linux D-Bus, which 2026-05-20 dogfooding confirmed is
/// fragile under `WSLg`. Title and body are passed through
/// [`ascii_fold`] first because the interop arg pipe is lossy on
/// non-ASCII (see that function's docs).
pub struct WslToastDispatcher;

impl Dispatcher for WslToastDispatcher {
    fn dispatch(&self, payload: Payload) -> Result<(), String> {
        let sound_file = payload.sound_file.clone();
        let title = ascii_fold(&payload.title);
        let body = ascii_fold(&payload.body);
        std::thread::spawn(move || {
            // argv: `wsl-notify-send.exe --category <title> <body>`. Per
            // 2026-05-23 dogfooding against v0.1.871612270, `--category`
            // surfaces as the toast title and the sole positional is the
            // body — empirical, not what `--help` implies. Passing two
            // positionals makes the binary print its usage and exit
            // non-zero, which the spawned thread silently swallows, so a
            // wrong shape disappears as "no toast" rather than an error.
            let _ = Command::new("wsl-notify-send.exe")
                .arg("--category")
                .arg(&title)
                .arg(&body)
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

    fn dispatch_blocking(&self, payload: Payload) -> Result<(), String> {
        let _ = Command::new("wsl-notify-send.exe")
            .arg("--category")
            .arg(ascii_fold(&payload.title))
            .arg(ascii_fold(&payload.body))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Some(path) = payload.sound_file {
            play_sound_file_blocking(&path);
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
    std::fs::read_to_string("/proc/sys/kernel/osrelease").is_ok_and(|s| {
        let lower = s.to_ascii_lowercase();
        lower.contains("microsoft") || lower.contains("wsl")
    })
}

#[derive(Debug, Clone, Default)]
struct SessionState {
    last_fired: Option<SystemTime>,
    fired_for_current_episode: bool,
    /// Payload captured when an entry into `NeedsInput` was suppressed
    /// because the user was actively viewing the session at the time.
    /// Drained by [`Notifier::on_terminal_focus_lost`] to deliver a
    /// belated toast when the user moves window-manager focus away
    /// from the terminal without any new attention transition.
    /// Cleared when the session leaves `NeedsInput` (the trigger no
    /// longer applies) or once the belated toast actually fires.
    pending_payload: Option<Payload>,
}

pub struct Notifier {
    dispatcher: Box<dyn Dispatcher>,
    state: HashMap<SessionId, SessionState>,
    config: NotificationsConfig,
    /// Wall-clock moment this `Notifier` was constructed. Used by the
    /// startup-replay gate in [`Self::on_attention_update`]: a
    /// [`Transition`] whose `source_at` is older than this is dropped
    /// without dispatch, so hook markers drained from disk on launch
    /// don't fire a stack of OS toasts for events that predate the
    /// run. Captured here (not [`Transition`]) because every call
    /// site shares the same anchor — the lifetime of this `Notifier`.
    created_at: SystemTime,
}

impl Notifier {
    #[must_use]
    pub fn new(dispatcher: Box<dyn Dispatcher>, config: NotificationsConfig) -> Self {
        Self::with_created_at(dispatcher, config, SystemTime::now())
    }

    /// Like [`Self::new`] but lets the caller pin `created_at`. Tests
    /// use this to drive the startup-replay gate deterministically;
    /// production wires through [`Self::new`].
    #[must_use]
    pub fn with_created_at(
        dispatcher: Box<dyn Dispatcher>,
        config: NotificationsConfig,
        created_at: SystemTime,
    ) -> Self {
        Self {
            dispatcher,
            state: HashMap::new(),
            config,
            created_at,
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
                // The user has acted (or attention drifted) — any
                // payload we were holding for a belated focus-loss
                // toast no longer applies. Clearing here keeps
                // `on_terminal_focus_lost` from firing for an episode
                // that's already over.
                s.pending_payload = None;
            }
            return;
        }
        if t.prev == Attention::NeedsInput {
            return;
        }
        // Startup-replay gate: a transition whose underlying signal
        // pre-dates this `Notifier` is a replay of something that
        // happened while agent-mux wasn't running (canonical case:
        // the hook-marker startup sweep draining files written by
        // claude before launch). The catalog state was already
        // applied upstream so the row paints `NeedsInput` from frame
        // one; only the toast is suppressed. Deliberately placed
        // before the `enabled`/`disabled_hosts` and `actively_viewed`
        // gates so a stale event can't stash a pending payload for
        // belated dispatch either.
        if let Some(source_at) = t.source_at
            && source_at < self.created_at
        {
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
        let (title, body) = format_notification(t.title, t.host, t.project, t.blocking, t.message);
        let payload = Payload {
            title,
            body,
            blocking: t.blocking,
            sound: self.config.sound,
            sound_file: self.config.sound_file.clone(),
        };
        // Suppress without arming the episodic flag: the user is
        // looking at this session right now, but if they later move
        // focus away while the session remains in NeedsInput, a
        // *future* transition (e.g. they reply, Claude works, stops
        // again) should still fire. Arming here would block that.
        // The payload is stashed so [`on_terminal_focus_lost`] can
        // deliver a belated toast if the user alt-tabs away without
        // any further attention transition (the false-negative path
        // 2026-05-24 dogfooding surfaced).
        if t.actively_viewed {
            let entry = self.state.entry(t.id.clone()).or_default();
            entry.pending_payload = Some(payload);
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
        if self.dispatcher.dispatch(payload).is_ok() {
            entry.last_fired = Some(now);
            entry.fired_for_current_episode = true;
            // Belated toast no longer needed — the immediate one fired.
            entry.pending_payload = None;
        }
    }

    /// Deliver toasts for sessions whose entry into `NeedsInput` was
    /// previously suppressed because the user was actively viewing
    /// them. Called from the main event loop on DEC 1004 `FocusLost`
    /// (window-manager focus left the agent-mux terminal).
    ///
    /// Why this exists: [`on_attention_update`] deliberately does not
    /// arm `fired_for_current_episode` when `actively_viewed` is true
    /// — a *future* attention transition for the same session must
    /// still fire normally. But moving window-manager focus away
    /// without any further transition produced no signal at all,
    /// which 2026-05-24 dogfooding caught as the false-negative
    /// "Claude asked for a permission and I didn't notice because
    /// I'd alt-tabbed to my browser." This method closes that gap.
    ///
    /// For each session with a stored pending payload (and the
    /// episodic flag still clear), the captured payload is dispatched
    /// and the flag is armed. Sessions without a pending payload are
    /// untouched; the master toggle and per-host disables already
    /// gated the entry into the pending state at suppression time, so
    /// no second check is needed here. On dispatch failure the
    /// payload is restored to give the next focus-loss edge a retry,
    /// matching the rest of the notifier's "transient failure should
    /// not silently swallow a user-attention signal" stance.
    pub fn on_terminal_focus_lost(&mut self, now: SystemTime) {
        // Two-phase to sidestep the `&mut self.dispatcher` /
        // `&mut self.state` borrow conflict: first collect the work
        // items (id + payload + current debounce timestamp), then
        // hand each off to the dispatcher in a borrow-free loop, then
        // write the resulting state back.
        let mut work: Vec<(SessionId, Payload)> = Vec::new();
        for (id, entry) in &mut self.state {
            let Some(payload) = entry.pending_payload.as_ref() else {
                continue;
            };
            if entry.fired_for_current_episode {
                continue;
            }
            if let Some(last) = entry.last_fired
                && now
                    .duration_since(last)
                    .ok()
                    .is_some_and(|d| d < DEBOUNCE_WINDOW)
            {
                continue;
            }
            work.push((id.clone(), payload.clone()));
        }
        for (id, payload) in work {
            let dispatched = self.dispatcher.dispatch(payload).is_ok();
            if let Some(entry) = self.state.get_mut(&id)
                && dispatched
            {
                entry.last_fired = Some(now);
                entry.fired_for_current_episode = true;
                entry.pending_payload = None;
            }
            // On dispatch failure we deliberately leave `pending_payload`
            // intact (it was never `take`n in this revision) so a
            // later focus-loss edge can retry — same "transient
            // backend outage shouldn't silently swallow a
            // user-attention signal" stance as the rest of the notifier.
        }
    }

    /// Build the payload the notifier would dispatch for a synthetic
    /// transition. Used by the `notify-test` subcommand to render and
    /// fire a one-off notification matching the user's current config
    /// without provoking a real session transition. Pulled out of the
    /// dispatch path so the test subcommand can introspect the payload
    /// (e.g. log what it sent) before handing it to the dispatcher.
    #[must_use]
    pub fn test_payload(
        &self,
        title: &str,
        host: &str,
        project: &Path,
        blocking: bool,
        message: Option<&str>,
    ) -> Payload {
        // Route through the same formatter the live path uses so
        // `notify-test` previews exactly what a real toast looks like.
        // The `notify-test --blocking` flag drives the `blocking` +
        // `message` inputs so the user can preview the sticky
        // "needs your input" variant, not just the "finished" fallback.
        let (title, body) =
            format_notification(title, &HostId(host.to_string()), project, blocking, message);
        Payload {
            title,
            body,
            blocking,
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
    /// Calls `dispatch_blocking` rather than `dispatch` because the
    /// `notify-test` process exits as soon as this returns. The
    /// default `dispatch` spawns the OS-notification and audio-player
    /// invocations onto detached threads, which lose a race against
    /// the parent exit and never fire. The blocking variant runs the
    /// `osascript`/`wsl-notify-send`/`notify-rust` call in the
    /// caller's thread and uses `Command::spawn` for the audio player
    /// (returns once the child has fork+exec'd, so playback survives
    /// the CLI exit without blocking on its full duration).
    ///
    /// # Errors
    ///
    /// Propagates the dispatcher's error string verbatim.
    pub fn dispatch_test(&self, payload: Payload) -> Result<(), String> {
        self.dispatcher.dispatch_blocking(payload)
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
    /// Whether this `NeedsInput` is a Claude Code *blocking prompt*
    /// (permission request / elicitation dialog — the agent is stuck
    /// waiting on a specific answer) versus a finished turn / idle
    /// nudge. Mirrors the session's `blocking_prompt` flag at the call
    /// site. Drives both the title wording ("needs your input" vs
    /// "finished") and the Linux urgency hint (`Critical` when true).
    pub blocking: bool,
    /// The Claude Code `Notification` hook's `message` field when this
    /// transition was driven by a hook event (e.g. "Claude needs your
    /// permission to use Bash", or an elicitation question) — the most
    /// informative body text available. `None` on the heuristic path
    /// (a transcript-derived transition has no prompt text), in which
    /// case the body falls back to the project context.
    pub message: Option<&'a str>,
    /// True when the user is *actively engaged* with this specific
    /// session at transition time — the embedded PTY pane currently
    /// hosts this session and keyboard focus is on the terminal, not
    /// the sidebar. Set by the call site in `main.rs`; the notifier
    /// uses it to suppress an OS toast that would tell the user
    /// something they're already looking at.
    ///
    /// Suppression skips the episodic-flag arm so a *later* transition
    /// (or this same `NeedsInput` episode observed once focus has
    /// moved elsewhere) still fires normally. The suppressed payload
    /// is stashed on the per-session state so
    /// [`Notifier::on_terminal_focus_lost`] can deliver a belated
    /// toast when the user moves window-manager focus away from the
    /// terminal without producing any new attention transition —
    /// closes the false-negative path 2026-05-24 dogfooding surfaced.
    pub actively_viewed: bool,
    /// Wall-clock moment the underlying signal was produced — for
    /// hook events this is the marker filename's millisecond prefix
    /// (`received_at`); for heuristic transitions it's the transcript
    /// file's mtime at the moment the event was produced (carried in
    /// [`crate::watcher::AttentionUpdate::mtime`]).
    ///
    /// When `Some(t)` and `t < Notifier::created_at`, the notifier
    /// drops the dispatch: the signal predates the current agent-mux
    /// run, so a toast would be a replay of a pre-launch event. The
    /// catalog state was already applied upstream, so rows still
    /// paint `NeedsInput` from frame one — only the OS toast is
    /// suppressed. When `None` (rare: a stat failure left the
    /// producer without a timestamp), the gate is inactive — better
    /// an over-fire than a silenced live transition. Suppression
    /// skips the episodic-flag arm so a *future* live transition for
    /// the same session still fires.
    pub source_at: Option<SystemTime>,
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

    #[test]
    fn ascii_fold_passes_through_pure_ascii_and_substitutes_known_offenders() {
        assert_eq!(ascii_fold("plain ascii"), "plain ascii");
        // Production body separator from notifications.rs `format!("{} · {}", ...)`.
        assert_eq!(ascii_fold("local · /tmp/dir"), "local | /tmp/dir");
        // Likely to appear in user-supplied task titles.
        assert_eq!(ascii_fold("a — b – c"), "a - b - c");
        assert_eq!(
            ascii_fold("\u{2018}q\u{2019} \u{201C}r\u{201D}"),
            "'q' \"r\""
        );
        assert_eq!(ascii_fold("done\u{2026}"), "done...");
        // Unknown non-ASCII degrades to `?` rather than smuggling raw bytes.
        assert_eq!(ascii_fold("emoji 🎉 here"), "emoji ? here");
    }

    fn payload(title: &str, body: &str, sound: bool) -> Payload {
        Payload {
            title: title.into(),
            body: body.into(),
            blocking: false,
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
                blocking: false,
                message: None,
                actively_viewed: false,
                source_at: None,
            },
            now,
        );
    }

    /// `fire`, but with the user actively engaged with the transitioning
    /// session at the moment of the update. Mirrors the main.rs call
    /// site's `actively_viewed = true` branch.
    #[allow(clippy::too_many_arguments)]
    fn fire_active(
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
                blocking: false,
                message: None,
                actively_viewed: true,
                source_at: None,
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
        assert_eq!(log_titles(&rec), vec!["refactor parser — finished"]);
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

    /// Helper: like `fire`, but with an explicit `source_at`. Used by
    /// the startup-replay-gate tests below. Mirrors the production
    /// wiring on the hook arm where `received_at` flows through to the
    /// notifier as `source_at`.
    #[allow(clippy::too_many_arguments)]
    fn fire_with_source_at(
        n: &mut Notifier,
        id: &SessionId,
        prev: Attention,
        new: Attention,
        title: &str,
        host: &HostId,
        project: &Path,
        now: SystemTime,
        source_at: Option<SystemTime>,
    ) {
        n.on_attention_update(
            &Transition {
                id,
                prev,
                new,
                title,
                host,
                project,
                blocking: false,
                message: None,
                actively_viewed: false,
                source_at,
            },
            now,
        );
    }

    #[test]
    fn startup_replay_gate_suppresses_dispatch_when_source_predates_notifier() {
        // 2026-05-26 dogfooding: opening agent-mux with leftover hook
        // markers on disk stacked an OS toast per marker as the
        // startup sweep drained them. The fix gates dispatch on the
        // event's `source_at`: a marker minted before the notifier
        // was created is a replay, not a live event.
        let rec = std::sync::Arc::new(RecorderDispatcher::default());
        let mut n = Notifier::with_created_at(
            Box::new(SharedRecorder(std::sync::Arc::clone(&rec))),
            NotificationsConfig::default(),
            at(100),
        );
        fire_with_source_at(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(101),
            Some(at(50)),
        );
        assert!(
            rec.log.lock().unwrap().is_empty(),
            "stale-source transition should not dispatch",
        );
    }

    #[test]
    fn startup_replay_gate_lets_through_signals_at_or_after_notifier_creation() {
        let rec = std::sync::Arc::new(RecorderDispatcher::default());
        let mut n = Notifier::with_created_at(
            Box::new(SharedRecorder(std::sync::Arc::clone(&rec))),
            NotificationsConfig::default(),
            at(100),
        );
        // Boundary: source_at == created_at is treated as live (the
        // gate is strict-less-than, matching the "drained from disk"
        // shape — a marker minted in the same millisecond agent-mux
        // launched is almost certainly a live event the user wants).
        fire_with_source_at(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(105),
            Some(at(100)),
        );
        assert_eq!(rec.log.lock().unwrap().len(), 1);
    }

    #[test]
    fn startup_replay_gate_does_not_arm_episodic_flag_so_later_live_event_still_fires() {
        // The gate must drop the dispatch without marking the session
        // as "already notified this episode" — otherwise the stale
        // replay would silently mute the first *real* notification for
        // the same session. This was the load-bearing reason to gate
        // before payload construction *and* before the episodic-flag
        // arm in `on_attention_update`.
        let rec = std::sync::Arc::new(RecorderDispatcher::default());
        let mut n = Notifier::with_created_at(
            Box::new(SharedRecorder(std::sync::Arc::clone(&rec))),
            NotificationsConfig::default(),
            at(100),
        );
        // Stale replay (pre-startup hook marker).
        fire_with_source_at(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(101),
            Some(at(50)),
        );
        // The catalog's pinned state has the session at NeedsInput;
        // a subsequent live hook event re-asserts NeedsInput from
        // NeedsInput (no transition). Walk the session out of
        // NeedsInput first (matching the real flow: assistant runs a
        // tool → Working) so the next entry into NeedsInput is a
        // genuine transition the notifier should fire on.
        fire_with_source_at(
            &mut n,
            &sid("a"),
            Attention::NeedsInput,
            Attention::Working,
            "x",
            &local(),
            Path::new("/p"),
            at(110),
            Some(at(110)),
        );
        fire_with_source_at(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(115),
            Some(at(115)),
        );
        assert_eq!(
            rec.log.lock().unwrap().len(),
            1,
            "live transition after a stale-replay drop must still dispatch",
        );
    }

    #[test]
    fn startup_replay_gate_is_inactive_when_source_at_is_none() {
        // Heuristic call sites pass `source_at: None` (no timestamp
        // available), and the existing behaviour — fire on transition
        // — must be preserved.
        let rec = std::sync::Arc::new(RecorderDispatcher::default());
        let mut n = Notifier::with_created_at(
            Box::new(SharedRecorder(std::sync::Arc::clone(&rec))),
            NotificationsConfig::default(),
            at(1_000),
        );
        fire_with_source_at(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(1_001),
            None,
        );
        assert_eq!(rec.log.lock().unwrap().len(), 1);
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
    fn payload_title_leads_with_session_and_body_carries_remote_context() {
        // Heuristic path (no hook message) on a remote host: the title
        // leads with the session name + "finished" state (no wasteful
        // `agent-mux:` prefix), and the body is the project *basename*
        // plus the remote host label.
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
        assert_eq!(log[0].title, "refactor parser — finished");
        assert_eq!(log[0].body, "mux · alpenglow");
    }

    #[test]
    fn format_notification_local_finished_drops_host_and_uses_basename() {
        // Local + no hook message: title says "finished", body is just
        // the project basename (no `local ·` noise).
        let (title, body) = format_notification(
            "refactor parser",
            &HostId::local(),
            Path::new("/home/user/work/mux"),
            false,
            None,
        );
        assert_eq!(title, "refactor parser — finished");
        assert_eq!(body, "mux");
    }

    #[test]
    fn format_notification_blocking_with_hook_message_uses_message_as_body() {
        // Blocking prompt + hook message: title flags "needs your
        // input", body is the actual prompt text.
        let (title, body) = format_notification(
            "deploy",
            &HostId::local(),
            Path::new("/w/deploy"),
            true,
            Some("Claude needs your permission to use Bash"),
        );
        assert_eq!(title, "deploy — needs your input");
        assert_eq!(body, "Claude needs your permission to use Bash");
    }

    #[test]
    fn format_notification_remote_message_appends_host() {
        let (_, body) = format_notification(
            "deploy",
            &HostId("alpenglow".to_string()),
            Path::new("/w/deploy"),
            true,
            Some("Approve running git push?"),
        );
        assert_eq!(body, "Approve running git push? · alpenglow");
    }

    #[test]
    fn clip_message_collapses_whitespace_and_truncates_long_input() {
        assert_eq!(clip_message("a\n  b\tc"), "a b c");
        let long = "x".repeat(200);
        let clipped = clip_message(&long);
        assert_eq!(clipped.chars().count(), BODY_MESSAGE_CLIP);
        assert!(
            clipped.ends_with('\u{2026}'),
            "expected ellipsis: {clipped}"
        );
    }

    #[test]
    fn blocking_payload_sets_blocking_flag_for_urgency() {
        // The `blocking` bool must reach the Payload so the Linux
        // dispatcher can raise urgency to Critical.
        let (mut n, rec) = notifier_with_log();
        n.on_attention_update(
            &Transition {
                id: &sid("a"),
                prev: Attention::Working,
                new: Attention::NeedsInput,
                title: "deploy",
                host: &local(),
                project: Path::new("/w/deploy"),
                blocking: true,
                message: Some("Approve?"),
                actively_viewed: false,
                source_at: None,
            },
            at(0),
        );
        let log = rec.log.lock().unwrap();
        assert!(log[0].blocking, "blocking flag must reach the payload");
        assert_eq!(log[0].title, "deploy — needs your input");
        assert_eq!(log[0].body, "Approve?");
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
        assert_eq!(log[0].title, "y — finished");
    }

    #[test]
    fn does_not_fire_when_user_is_actively_viewing_the_session() {
        let (mut n, rec) = notifier_with_log();
        fire_active(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(0),
        );
        assert_eq!(rec.log.lock().unwrap().len(), 0);
    }

    #[test]
    fn actively_viewed_suppression_does_not_arm_episodic_flag() {
        // A user who's looking at the pane during the transition gets
        // no toast. If they later move focus away and another
        // transition arrives for the same session, that one MUST
        // fire — arming the episodic flag at the active-viewing point
        // would block it incorrectly.
        let (mut n, rec) = notifier_with_log();
        fire_active(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "first",
            &local(),
            Path::new("/p"),
            at(0),
        );
        assert_eq!(rec.log.lock().unwrap().len(), 0, "active-view suppression");
        // Session leaves NeedsInput (user replied), comes back. By
        // then the user has moved focus away.
        fire(
            &mut n,
            &sid("a"),
            Attention::NeedsInput,
            Attention::Working,
            "first",
            &local(),
            Path::new("/p"),
            at(10),
        );
        fire(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "second",
            &local(),
            Path::new("/p"),
            at(20),
        );
        let log = rec.log.lock().unwrap();
        assert_eq!(log.len(), 1, "second transition should fire");
        assert_eq!(log[0].title, "second — finished");
    }

    #[test]
    fn focus_loss_delivers_belated_toast_for_actively_viewed_suppression() {
        // The dogfooded scenario (2026-05-24): user is staring at the
        // session's embedded pane when it enters NeedsInput, then
        // alt-tabs to a browser. The transition-time suppression is
        // correct (no toast while looking at it), but the focus-loss
        // edge must deliver the belated toast — otherwise the user
        // misses the permission prompt entirely.
        let (mut n, rec) = notifier_with_log();
        fire_active(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "permission prompt",
            &local(),
            Path::new("/proj"),
            at(0),
        );
        assert_eq!(rec.log.lock().unwrap().len(), 0, "suppressed at transition");
        n.on_terminal_focus_lost(at(1));
        let log = rec.log.lock().unwrap();
        assert_eq!(log.len(), 1, "belated toast on focus loss");
        assert_eq!(log[0].title, "permission prompt — finished");
    }

    #[test]
    fn focus_loss_is_a_noop_when_no_session_has_a_pending_payload() {
        // The vast majority of focus-loss edges happen with no pending
        // payloads — every alt-tab away while nothing is waiting. The
        // method must be cheap and silent in that case.
        let (mut n, rec) = notifier_with_log();
        n.on_terminal_focus_lost(at(0));
        assert!(rec.log.lock().unwrap().is_empty());
        // And a normal (non-actively-viewed) fire followed by focus
        // loss must not re-fire — the episodic flag is already armed,
        // no pending payload was stored.
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
        assert_eq!(rec.log.lock().unwrap().len(), 1);
        n.on_terminal_focus_lost(at(20));
        assert_eq!(
            rec.log.lock().unwrap().len(),
            1,
            "focus loss must not duplicate an already-fired toast"
        );
    }

    #[test]
    fn focus_loss_does_not_refire_after_session_leaves_needs_input() {
        // Pending payload must be cleared when the user replies (or
        // attention drifts) so a later focus-loss doesn't surface a
        // stale toast for an episode that's already over.
        let (mut n, rec) = notifier_with_log();
        fire_active(
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
            at(5),
        );
        n.on_terminal_focus_lost(at(10));
        assert!(
            rec.log.lock().unwrap().is_empty(),
            "no toast — the episode is over"
        );
    }

    #[test]
    fn focus_loss_belated_toast_arms_episodic_flag_so_idle_redrives_dont_refire() {
        // Once the belated toast lands, a subsequent NeedsInput→NeedsInput
        // catalog tick (the watcher periodically re-asserts the same
        // state) must not double-fire. Same invariant the normal
        // dispatch path enforces via `fired_for_current_episode`.
        let (mut n, rec) = notifier_with_log();
        fire_active(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(0),
        );
        n.on_terminal_focus_lost(at(1));
        assert_eq!(rec.log.lock().unwrap().len(), 1);
        // Watcher re-asserts (no real transition). Must not refire.
        fire(
            &mut n,
            &sid("a"),
            Attention::NeedsInput,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(10),
        );
        // A second focus-loss edge (user re-focused then alt-tabbed
        // again) must also not refire — pending was drained.
        n.on_terminal_focus_lost(at(20));
        assert_eq!(rec.log.lock().unwrap().len(), 1);
    }

    #[test]
    fn focus_loss_belated_toast_restores_pending_on_dispatch_failure() {
        // Symmetric with the normal dispatch path: a transient
        // backend outage must not silently swallow the user-attention
        // signal. Restoring the pending payload lets the next focus-
        // loss edge retry (e.g. user re-focuses then alt-tabs again).
        let mut n = Notifier::new(Box::new(FailingDispatcher), NotificationsConfig::default());
        n.on_attention_update(
            &Transition {
                id: &sid("a"),
                prev: Attention::Working,
                new: Attention::NeedsInput,
                title: "x",
                host: &local(),
                project: Path::new("/p"),
                blocking: false,
                message: None,
                actively_viewed: true,
                source_at: None,
            },
            at(0),
        );
        n.on_terminal_focus_lost(at(1));
        // Failed dispatch must leave the session re-fire-able. Swap
        // in a working recorder and prove a second focus-loss fires.
        let rec = std::sync::Arc::new(RecorderDispatcher::default());
        n.dispatcher = Box::new(SharedRecorder(std::sync::Arc::clone(&rec)));
        n.on_terminal_focus_lost(at(2));
        assert_eq!(
            rec.log.lock().unwrap().len(),
            1,
            "retry should fire after a working dispatcher is swapped in"
        );
    }

    #[test]
    fn focus_loss_belated_toast_respects_debounce_against_prior_fire() {
        // Edge case: a normal fire arms last_fired, then the session
        // leaves and re-enters NeedsInput while actively-viewed
        // (stashes pending), and the user alt-tabs within the
        // debounce window of the prior fire. The same debounce that
        // protects against catalog flapping should suppress the
        // belated toast too — otherwise we get two pings in <5s
        // for what's effectively the same attention episode from the
        // user's perspective.
        let (mut n, rec) = notifier_with_log();
        // Fire 1 at t=0 (normal, not actively viewed).
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
        // Leaves and re-enters NeedsInput within the debounce window,
        // now actively viewed — stashes pending.
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
        fire_active(
            &mut n,
            &sid("a"),
            Attention::Working,
            Attention::NeedsInput,
            "x",
            &local(),
            Path::new("/p"),
            at(2),
        );
        // Focus loss inside the debounce window — must NOT fire.
        n.on_terminal_focus_lost(at(3));
        assert_eq!(
            rec.log.lock().unwrap().len(),
            1,
            "debounce should suppress the belated toast"
        );
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
            blocking: false,
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

    /// Recorder that distinguishes `dispatch` vs `dispatch_blocking`.
    /// Pins the invariant that `Notifier::dispatch_test` (CLI path) routes
    /// through the blocking variant — a regression to `dispatch` would
    /// reintroduce the race where the CLI process exits before the
    /// detached thread can `fork+exec` and the notification never fires.
    #[derive(Default)]
    struct RouteRecorder {
        async_calls: Mutex<usize>,
        sync_calls: Mutex<usize>,
    }

    impl Dispatcher for RouteRecorder {
        fn dispatch(&self, _: Payload) -> Result<(), String> {
            *self.async_calls.lock().unwrap() += 1;
            Ok(())
        }
        fn dispatch_blocking(&self, _: Payload) -> Result<(), String> {
            *self.sync_calls.lock().unwrap() += 1;
            Ok(())
        }
    }

    struct SharedRouteRecorder(std::sync::Arc<RouteRecorder>);
    impl Dispatcher for SharedRouteRecorder {
        fn dispatch(&self, p: Payload) -> Result<(), String> {
            self.0.dispatch(p)
        }
        fn dispatch_blocking(&self, p: Payload) -> Result<(), String> {
            self.0.dispatch_blocking(p)
        }
    }

    #[test]
    fn dispatch_test_routes_through_dispatch_blocking_not_dispatch() {
        let rec = std::sync::Arc::new(RouteRecorder::default());
        let notifier = Notifier::new(
            Box::new(SharedRouteRecorder(std::sync::Arc::clone(&rec))),
            NotificationsConfig::default(),
        );
        let payload = notifier.test_payload("x", "h", Path::new("/p"), false, None);
        notifier.dispatch_test(payload).unwrap();
        assert_eq!(
            *rec.sync_calls.lock().unwrap(),
            1,
            "dispatch_test must use the blocking variant",
        );
        assert_eq!(
            *rec.async_calls.lock().unwrap(),
            0,
            "dispatch_test must not use the fire-and-forget variant",
        );
    }

    #[test]
    fn default_dispatch_blocking_falls_through_to_dispatch() {
        // Synchronous test dispatchers should not need to override
        // dispatch_blocking; the default routes back to dispatch.
        struct OnlyDispatch(Mutex<usize>);
        impl Dispatcher for OnlyDispatch {
            fn dispatch(&self, _: Payload) -> Result<(), String> {
                *self.0.lock().unwrap() += 1;
                Ok(())
            }
        }
        let d = OnlyDispatch(Mutex::new(0));
        d.dispatch_blocking(Payload {
            title: "x".into(),
            body: "y".into(),
            blocking: false,
            sound: false,
            sound_file: None,
        })
        .unwrap();
        assert_eq!(*d.0.lock().unwrap(), 1);
    }

    #[test]
    fn test_payload_uses_current_config_sound_and_file() {
        let cfg = NotificationsConfig {
            sound: true,
            sound_file: Some(PathBuf::from("/abs/ding.mp3")),
            ..NotificationsConfig::default()
        };
        let (n, _) = notifier_with_log_and_config(cfg);
        let p = n.test_payload("preview", "alpenglow", Path::new("/work/mux"), false, None);
        assert_eq!(p.title, "preview — finished");
        assert_eq!(p.body, "mux · alpenglow");
        assert!(p.sound);
        assert_eq!(p.sound_file.as_deref(), Some(Path::new("/abs/ding.mp3")));
    }

    #[test]
    fn test_payload_blocking_previews_the_needs_input_variant() {
        // `notify-test --blocking` drives this: the title flips to
        // "needs your input", the sample message becomes the body, and
        // the `blocking` flag is set so the Linux dispatcher can raise
        // urgency to Critical.
        let (n, _) = notifier_with_log();
        let p = n.test_payload(
            "preview",
            "local",
            Path::new("/work/mux"),
            true,
            Some("Approve running git push?"),
        );
        assert_eq!(p.title, "preview — needs your input");
        assert_eq!(p.body, "Approve running git push?");
        assert!(p.blocking, "blocking flag must be set for urgency");
    }
}
