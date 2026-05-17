use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use ratatui::style::Color;
use serde::Deserialize;

/// Reserved host name standing for the local machine. May not be used as a
/// `[hosts.<name>]` key — the local host is implicit, not configured.
pub const LOCAL_HOST_NAME: &str = "local";

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(toml::de::Error),
    ReservedHostName(String),
    EmptySshTarget(String),
    /// A theme field carried a string that isn't a recognised colour
    /// (named ANSI colour, `bright_*` variant, hex `#RRGGBB`, or empty
    /// for default). Loud failure beats silent fallback because the
    /// user's intent was specific.
    InvalidColor {
        field: String,
        value: String,
    },
    /// `theme.preset` named a preset that isn't built in. Reported with
    /// the available preset names so the user can correct without
    /// hunting through docs.
    UnknownThemePreset(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Parse(e) => write!(f, "parse: {e}"),
            Self::ReservedHostName(name) => {
                write!(f, "host name {name:?} is reserved")
            }
            Self::EmptySshTarget(name) => {
                write!(f, "host {name:?} has an empty `ssh` field")
            }
            Self::InvalidColor { field, value } => {
                write!(f, "theme.{field}: {value:?} is not a recognised colour")
            }
            Self::UnknownThemePreset(name) => {
                write!(
                    f,
                    "theme.preset: {name:?} is not a recognised preset (available: {})",
                    Theme::preset_names().join(", ")
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct HostConfig {
    pub ssh: String,
    #[serde(default = "default_transcript_root")]
    pub transcript_root: PathBuf,
}

fn default_transcript_root() -> PathBuf {
    PathBuf::from("~/.claude/projects")
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub workspace_folders: Vec<PathBuf>,
    #[serde(default)]
    pub hosts: BTreeMap<String, HostConfig>,
    #[serde(default)]
    pub notifications: NotificationsConfig,
    /// Raw `[theme]` strings as read from disk. Resolved to a [`Theme`]
    /// of typed `ratatui` colours at load time via [`Theme::from_config`].
    /// Public so the loader can validate and reject bad names early.
    #[serde(default)]
    pub theme: ThemeConfig,
}

/// Raw theme strings from `[theme]` in `config.toml`. Each colour field
/// is `Option<String>` so we can distinguish "user didn't touch this
/// key" (inherit from the preset) from "user set it to the empty string"
/// (explicit "no fg colour, terminal default"). The `preset` field
/// chooses which built-in baseline the per-element overrides apply on
/// top of; missing preset means [`Theme::preset_default`].
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ThemeConfig {
    /// Named baseline. See [`Theme::preset_names`] for the built-in set.
    pub preset: Option<String>,
    /// Colour of the `●` glyph for sessions in `NeedsInput`.
    pub needs_input: Option<String>,
    /// Colour of the `◐` glyph for sessions currently `Working`.
    pub working: Option<String>,
    /// Colour of the `○` glyph for `Idle` sessions.
    pub idle: Option<String>,
    /// Colour of the `·` glyph for `Unknown` (no signal yet) sessions.
    pub unknown: Option<String>,
    /// Colour of `⚒ Tool: …` lines in the preview pane.
    pub tool_use: Option<String>,
    /// Colour of `↳ ok` lines in the preview pane.
    pub tool_result_ok: Option<String>,
    /// Colour of `↳ error` lines in the preview pane.
    pub tool_result_err: Option<String>,
    /// Colour of `> …` user prompt lines in the preview pane. Default
    /// (when absent) is the terminal's foreground; the bold modifier
    /// still applies regardless of colour.
    pub user_fg: Option<String>,
    /// Colour of assistant prose lines in the preview pane. Default
    /// (when absent) is the terminal's foreground. There is no longer
    /// a dim modifier baked in — assistant prose was unreadable on
    /// several common palettes; users who want a quieter assistant can
    /// set this to e.g. `bright_black` themselves.
    pub assistant_fg: Option<String>,
}

/// Resolved theme: each field is the parsed `ratatui::Color` or `None`
/// (meaning "leave the terminal default in place"). Built once at config
/// load via [`Theme::from_config`] so render paths never re-parse strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Theme {
    pub needs_input: Option<Color>,
    pub working: Option<Color>,
    pub idle: Option<Color>,
    pub unknown: Option<Color>,
    pub tool_use: Option<Color>,
    pub tool_result_ok: Option<Color>,
    pub tool_result_err: Option<Color>,
    pub user_fg: Option<Color>,
    pub assistant_fg: Option<Color>,
}

impl Theme {
    /// Names of the built-in presets, in the order they're listed in
    /// user-facing error messages and docs.
    #[must_use]
    pub fn preset_names() -> &'static [&'static str] {
        &[
            "default",
            "bright",
            "mono",
            "warm",
            "cool",
            "solarized",
            "gruvbox",
            "nord",
        ]
    }

    /// The "default" preset: matches the colour scheme that shipped
    /// before presets existed. Cyan/green/red preview, attention
    /// glyphs uncoloured. Used when `cfg.preset` is absent.
    #[must_use]
    pub fn preset_default() -> Self {
        Self {
            needs_input: None,
            working: None,
            idle: None,
            unknown: None,
            tool_use: Some(Color::Cyan),
            tool_result_ok: Some(Color::Green),
            tool_result_err: Some(Color::Red),
            user_fg: None,
            assistant_fg: None,
        }
    }

    /// Loud, high-contrast palette. Every attention glyph carries a
    /// colour, preview switches to `bright_*` variants. For terminals
    /// that render bright variants distinctly from the base — most do.
    #[must_use]
    pub fn preset_bright() -> Self {
        Self {
            needs_input: Some(Color::LightRed),
            working: Some(Color::LightYellow),
            idle: Some(Color::DarkGray),
            unknown: Some(Color::Gray),
            tool_use: Some(Color::LightCyan),
            tool_result_ok: Some(Color::LightGreen),
            tool_result_err: Some(Color::LightRed),
            user_fg: None,
            assistant_fg: None,
        }
    }

    /// Monochrome: no foreground colours at all. The bold modifier on
    /// user prompts is the lone structural signal carrying the user
    /// versus assistant distinction. For users on terminals without
    /// colour, or simply for a quieter palette.
    #[must_use]
    pub fn preset_mono() -> Self {
        Self::default()
    }

    /// Sunset / warm palette: reds, ambers, and earthy browns. Picks
    /// custom RGB instead of plain `Yellow`/`Red` so the warmth is more
    /// distinctive than the bare ANSI red would suggest.
    #[must_use]
    pub fn preset_warm() -> Self {
        Self {
            needs_input: Some(Color::Rgb(0xff, 0x57, 0x33)),
            working: Some(Color::Rgb(0xf4, 0xa7, 0x38)),
            idle: Some(Color::Rgb(0x8c, 0x6e, 0x54)),
            unknown: Some(Color::Rgb(0xa0, 0x87, 0x70)),
            tool_use: Some(Color::Rgb(0xd1, 0xa3, 0x47)),
            tool_result_ok: Some(Color::Rgb(0xb3, 0xa2, 0x28)),
            tool_result_err: Some(Color::Rgb(0xcc, 0x3a, 0x20)),
            user_fg: None,
            assistant_fg: None,
        }
    }

    /// Ocean / cool palette: blues, teals, sea greens. Errors stay
    /// rose-red so attention still pops against the surrounding cool
    /// tones — a pure all-blue palette buries the primary attention
    /// signal.
    #[must_use]
    pub fn preset_cool() -> Self {
        Self {
            needs_input: Some(Color::Rgb(0xe2, 0x6d, 0x75)),
            working: Some(Color::Rgb(0x6c, 0xb4, 0xd6)),
            idle: Some(Color::Rgb(0x47, 0x66, 0x80)),
            unknown: Some(Color::Rgb(0x5e, 0x7e, 0x94)),
            tool_use: Some(Color::Rgb(0x39, 0xa3, 0xa3)),
            tool_result_ok: Some(Color::Rgb(0x4c, 0xaa, 0x6c)),
            tool_result_err: Some(Color::Rgb(0xe2, 0x6d, 0x75)),
            user_fg: None,
            assistant_fg: None,
        }
    }

    /// Solarized accents (the palette's canonical 8 colours). Designed
    /// to work over either the dark or light Solarized backgrounds —
    /// agent-mux doesn't set its own background, so accent-only is the
    /// right slice to expose.
    #[must_use]
    pub fn preset_solarized() -> Self {
        Self {
            needs_input: Some(Color::Rgb(0xdc, 0x32, 0x2f)), // red
            working: Some(Color::Rgb(0xb5, 0x89, 0x00)),     // yellow
            idle: Some(Color::Rgb(0x58, 0x6e, 0x75)),        // base01
            unknown: Some(Color::Rgb(0x58, 0x6e, 0x75)),
            tool_use: Some(Color::Rgb(0x26, 0x8b, 0xd2)), // blue
            tool_result_ok: Some(Color::Rgb(0x85, 0x99, 0x00)), // green
            tool_result_err: Some(Color::Rgb(0xdc, 0x32, 0x2f)),
            user_fg: None,
            assistant_fg: None,
        }
    }

    /// Gruvbox bright variants. Earthy/retro feel that reads well on
    /// dark terminals; the bright accents pop more than gruvbox's
    /// muted "neutral" palette would.
    #[must_use]
    pub fn preset_gruvbox() -> Self {
        Self {
            needs_input: Some(Color::Rgb(0xfb, 0x49, 0x34)), // bright red
            working: Some(Color::Rgb(0xfa, 0xbd, 0x2f)),     // bright yellow
            idle: Some(Color::Rgb(0x92, 0x83, 0x74)),        // gray
            unknown: Some(Color::Rgb(0x92, 0x83, 0x74)),
            tool_use: Some(Color::Rgb(0x8e, 0xc0, 0x7c)), // bright aqua
            tool_result_ok: Some(Color::Rgb(0xb8, 0xbb, 0x26)), // bright green
            tool_result_err: Some(Color::Rgb(0xfb, 0x49, 0x34)),
            user_fg: None,
            assistant_fg: None,
        }
    }

    /// Nord aurora + frost palette. Cool slate tones with the aurora
    /// accents (red, yellow, green) for the primary semantic events.
    #[must_use]
    pub fn preset_nord() -> Self {
        Self {
            needs_input: Some(Color::Rgb(0xbf, 0x61, 0x6a)), // aurora red
            working: Some(Color::Rgb(0xeb, 0xcb, 0x8b)),     // aurora yellow
            idle: Some(Color::Rgb(0x4c, 0x56, 0x6a)),        // polar night
            unknown: Some(Color::Rgb(0x4c, 0x56, 0x6a)),
            tool_use: Some(Color::Rgb(0x88, 0xc0, 0xd0)), // frost cyan
            tool_result_ok: Some(Color::Rgb(0xa3, 0xbe, 0x8c)), // aurora green
            tool_result_err: Some(Color::Rgb(0xbf, 0x61, 0x6a)),
            user_fg: None,
            assistant_fg: None,
        }
    }

    /// Resolve a preset by name. Used both as the baseline for
    /// [`Theme::from_config`] and (transitively) by tests that want a
    /// known preset without going through TOML.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::UnknownThemePreset`] when `name` is not
    /// in [`Theme::preset_names`].
    pub fn preset(name: &str) -> Result<Self, ConfigError> {
        match name {
            "default" => Ok(Self::preset_default()),
            "bright" => Ok(Self::preset_bright()),
            "mono" => Ok(Self::preset_mono()),
            "warm" => Ok(Self::preset_warm()),
            "cool" => Ok(Self::preset_cool()),
            "solarized" => Ok(Self::preset_solarized()),
            "gruvbox" => Ok(Self::preset_gruvbox()),
            "nord" => Ok(Self::preset_nord()),
            _ => Err(ConfigError::UnknownThemePreset(name.to_string())),
        }
    }

    /// Resolve a [`ThemeConfig`] into a `Theme`. First chooses a base
    /// preset (default if `cfg.preset` is `None`), then overlays each
    /// per-element field the user explicitly set — `Some(s)` from the
    /// config takes precedence over the preset's value; `None` inherits.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::UnknownThemePreset`] for an unrecognised
    /// preset name and [`ConfigError::InvalidColor`] on the first
    /// malformed colour string. Subsequent malformed fields are not
    /// reported in the same pass.
    pub fn from_config(cfg: &ThemeConfig) -> Result<Self, ConfigError> {
        let base = match cfg.preset.as_deref() {
            None => Self::preset_default(),
            Some(name) => Self::preset(name)?,
        };
        Ok(Self {
            needs_input: overlay("needs_input", cfg.needs_input.as_deref(), base.needs_input)?,
            working: overlay("working", cfg.working.as_deref(), base.working)?,
            idle: overlay("idle", cfg.idle.as_deref(), base.idle)?,
            unknown: overlay("unknown", cfg.unknown.as_deref(), base.unknown)?,
            tool_use: overlay("tool_use", cfg.tool_use.as_deref(), base.tool_use)?,
            tool_result_ok: overlay(
                "tool_result_ok",
                cfg.tool_result_ok.as_deref(),
                base.tool_result_ok,
            )?,
            tool_result_err: overlay(
                "tool_result_err",
                cfg.tool_result_err.as_deref(),
                base.tool_result_err,
            )?,
            user_fg: overlay("user_fg", cfg.user_fg.as_deref(), base.user_fg)?,
            assistant_fg: overlay(
                "assistant_fg",
                cfg.assistant_fg.as_deref(),
                base.assistant_fg,
            )?,
        })
    }
}

/// Apply one user-set field on top of the preset's value. `None` means
/// the user didn't touch this key, so the preset wins. `Some(s)` parses
/// `s` (where empty / `"default"` both yield no colour, same as a
/// stand-alone field).
fn overlay(
    field: &str,
    user: Option<&str>,
    fallback: Option<Color>,
) -> Result<Option<Color>, ConfigError> {
    match user {
        Some(s) => parse_color(s, field),
        None => Ok(fallback),
    }
}

fn parse_color(raw: &str, field: &str) -> Result<Option<Color>, ConfigError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("default") {
        return Ok(None);
    }
    if let Some(hex) = trimmed.strip_prefix('#')
        && hex.len() == 6
        && let Ok(rgb) = u32::from_str_radix(hex, 16)
    {
        #[allow(clippy::cast_possible_truncation)]
        let r = ((rgb >> 16) & 0xff) as u8;
        #[allow(clippy::cast_possible_truncation)]
        let g = ((rgb >> 8) & 0xff) as u8;
        #[allow(clippy::cast_possible_truncation)]
        let b = (rgb & 0xff) as u8;
        return Ok(Some(Color::Rgb(r, g, b)));
    }
    let colour = match trimmed.to_ascii_lowercase().as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" | "bright_black" => Color::DarkGray,
        "bright_red" | "lightred" => Color::LightRed,
        "bright_green" | "lightgreen" => Color::LightGreen,
        "bright_yellow" | "lightyellow" => Color::LightYellow,
        "bright_blue" | "lightblue" => Color::LightBlue,
        "bright_magenta" | "lightmagenta" => Color::LightMagenta,
        "bright_cyan" | "lightcyan" => Color::LightCyan,
        "white" | "bright_white" => Color::White,
        _ => {
            return Err(ConfigError::InvalidColor {
                field: field.to_string(),
                value: trimmed.to_string(),
            });
        }
    };
    Ok(Some(colour))
}

/// User-tunable notification behaviour, surfaced under `[notifications]`
/// in `config.toml`. Missing section, missing keys, and a fully absent
/// config file all collapse to [`NotificationsConfig::default`] — every
/// knob has a sensible default so an empty config still behaves like
/// pre-M5 agent-mux.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct NotificationsConfig {
    /// Master on/off. When `false`, the notifier short-circuits on every
    /// transition and dispatches nothing. Default `true`: the M4 ship
    /// already produced notifications, and turning that off should be
    /// an explicit choice, not a silent regression after a config bump.
    pub enabled: bool,
    /// Whether the dispatcher requests an audible cue from the OS
    /// notification system (libnotify's `sound-name` / `AppleScript`'s
    /// `sound name`). Default `false` to preserve the pre-M5 silent
    /// behaviour; users opt in.
    pub sound: bool,
    /// Host labels for which notifications are suppressed entirely
    /// (matches against the `[hosts.<name>]` table key, or the literal
    /// `local`). Default empty. Use case: silence chatty CI/dev boxes
    /// while keeping pings for one specific host.
    pub disabled_hosts: Vec<String>,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sound: false,
            disabled_hosts: Vec::new(),
        }
    }
}

impl Config {
    /// Load from `~/.config/agent-mux/config.toml` (or
    /// `$XDG_CONFIG_HOME/agent-mux/config.toml` if set). Missing file is not
    /// an error — returns a default `Config` with an empty workspace list.
    ///
    /// # Errors
    /// [`ConfigError::Io`] for unreadable files (other than not-found);
    /// [`ConfigError::Parse`] if the TOML is malformed;
    /// [`ConfigError::ReservedHostName`] if a `[hosts.<name>]` table uses a
    /// reserved name (see [`LOCAL_HOST_NAME`]);
    /// [`ConfigError::EmptySshTarget`] if a host's `ssh` field is empty.
    pub fn load() -> Result<Self, ConfigError> {
        match default_config_path() {
            Some(p) => Self::load_from(&p),
            None => Ok(Self::default()),
        }
    }

    /// Load from an explicit path. Same semantics as [`Config::load`].
    ///
    /// # Errors
    /// See [`Config::load`].
    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        let raw = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(ConfigError::Io(e)),
        };
        let mut cfg: Self = toml::from_str(&raw).map_err(ConfigError::Parse)?;
        cfg.workspace_folders = cfg.workspace_folders.iter().map(|p| expand(p)).collect();
        for (name, host) in &mut cfg.hosts {
            if name.eq_ignore_ascii_case(LOCAL_HOST_NAME) {
                return Err(ConfigError::ReservedHostName(name.clone()));
            }
            if host.ssh.trim().is_empty() {
                return Err(ConfigError::EmptySshTarget(name.clone()));
            }
            // No tilde expansion here: a remote host's `~/.claude/projects`
            // means the *remote* user's home, not ours. `SshHost` passes
            // the tilde through to the remote shell via `shell_quote_path`.
        }
        Ok(cfg)
    }
}

fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("agent-mux").join("config.toml"))
}

fn expand(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if s == "~" {
        return dirs::home_dir().unwrap_or_else(|| p.to_path_buf());
    }
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    p.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn expand_keeps_absolute_path() {
        assert_eq!(
            expand(Path::new("/work/repos")),
            PathBuf::from("/work/repos")
        );
    }

    #[test]
    fn expand_keeps_relative_path() {
        assert_eq!(expand(Path::new("work/repos")), PathBuf::from("work/repos"));
    }

    #[test]
    fn expand_handles_tilde_prefix() {
        let home = dirs::home_dir().expect("test runner has $HOME");
        assert_eq!(expand(Path::new("~/work")), home.join("work"));
    }

    #[test]
    fn expand_handles_bare_tilde() {
        let home = dirs::home_dir().expect("test runner has $HOME");
        assert_eq!(expand(Path::new("~")), home);
    }

    #[test]
    fn load_from_missing_file_returns_default() {
        let tmp = TempDir::new().expect("tempdir");
        let cfg =
            Config::load_from(&tmp.path().join("does-not-exist.toml")).expect("missing file is ok");
        assert!(cfg.workspace_folders.is_empty());
    }

    #[test]
    fn load_from_empty_file_returns_default() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(&path, "").expect("write");
        let cfg = Config::load_from(&path).expect("empty toml is ok");
        assert!(cfg.workspace_folders.is_empty());
    }

    #[test]
    fn load_from_parses_workspace_folders() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(
            &path,
            r#"workspace_folders = ["/work/repos", "/home/dev/code"]"#,
        )
        .expect("write");
        let cfg = Config::load_from(&path).expect("parse");
        assert_eq!(
            cfg.workspace_folders,
            vec![
                PathBuf::from("/work/repos"),
                PathBuf::from("/home/dev/code")
            ]
        );
    }

    #[test]
    fn load_from_expands_tilde_in_workspace_folders() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(&path, r#"workspace_folders = ["~/work"]"#).expect("write");
        let cfg = Config::load_from(&path).expect("parse");
        let home = dirs::home_dir().expect("home");
        assert_eq!(cfg.workspace_folders, vec![home.join("work")]);
    }

    #[test]
    fn load_from_rejects_malformed_toml() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(&path, "not = valid = toml").expect("write");
        let err = Config::load_from(&path).expect_err("should reject");
        assert!(matches!(err, ConfigError::Parse(_)), "got: {err:?}");
    }

    #[test]
    fn load_from_with_no_hosts_returns_empty_map() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(&path, r#"workspace_folders = ["/work"]"#).expect("write");
        let cfg = Config::load_from(&path).expect("parse");
        assert!(cfg.hosts.is_empty());
    }

    #[test]
    fn load_from_parses_hosts_with_defaults() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(
            &path,
            r#"
[hosts.devbox]
ssh = "devbox"
"#,
        )
        .expect("write");
        let cfg = Config::load_from(&path).expect("parse");
        let host = cfg.hosts.get("devbox").expect("devbox host");
        assert_eq!(host.ssh, "devbox");
        // Default is `~/.claude/projects`, kept as-is — the tilde must
        // resolve against the *remote* user's home, not ours.
        assert_eq!(host.transcript_root, PathBuf::from("~/.claude/projects"));
    }

    #[test]
    fn load_from_parses_explicit_transcript_root() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(
            &path,
            r#"
[hosts.gpu]
ssh = "gizmo@gpu-1.internal"
transcript_root = "/srv/claude/projects"
"#,
        )
        .expect("write");
        let cfg = Config::load_from(&path).expect("parse");
        let host = cfg.hosts.get("gpu").expect("gpu host");
        assert_eq!(host.ssh, "gizmo@gpu-1.internal");
        assert_eq!(host.transcript_root, PathBuf::from("/srv/claude/projects"));
    }

    #[test]
    fn load_from_leaves_tilde_unexpanded_in_remote_transcript_root() {
        // Regression: tilde in a remote-host `transcript_root` must
        // resolve against the *remote* home, not the local user's. The
        // config layer keeps it as a literal `~/…`; SshHost passes it
        // through to the remote shell intact.
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(
            &path,
            r#"
[hosts.devbox]
ssh = "devbox"
transcript_root = "~/scratch/claude"
"#,
        )
        .expect("write");
        let cfg = Config::load_from(&path).expect("parse");
        let host = cfg.hosts.get("devbox").expect("devbox host");
        assert_eq!(host.transcript_root, PathBuf::from("~/scratch/claude"));
    }

    #[test]
    fn load_from_parses_multiple_hosts() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(
            &path,
            r#"
workspace_folders = ["~/work"]

[hosts.devbox]
ssh = "devbox"

[hosts.gpu]
ssh = "user@gpu-1.internal"
transcript_root = "/srv/claude/projects"
"#,
        )
        .expect("write");
        let cfg = Config::load_from(&path).expect("parse");
        assert_eq!(cfg.hosts.len(), 2);
        // BTreeMap orders deterministically.
        let names: Vec<_> = cfg.hosts.keys().cloned().collect();
        assert_eq!(names, vec!["devbox".to_string(), "gpu".to_string()]);
    }

    #[test]
    fn load_from_rejects_reserved_local_host_name() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(
            &path,
            r#"
[hosts.local]
ssh = "anything"
"#,
        )
        .expect("write");
        let err = Config::load_from(&path).expect_err("should reject");
        assert!(
            matches!(err, ConfigError::ReservedHostName(ref n) if n == "local"),
            "got: {err:?}"
        );
    }

    #[test]
    fn load_from_rejects_reserved_local_host_name_case_insensitive() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(
            &path,
            r#"
[hosts.Local]
ssh = "anything"
"#,
        )
        .expect("write");
        let err = Config::load_from(&path).expect_err("should reject");
        assert!(
            matches!(err, ConfigError::ReservedHostName(_)),
            "got: {err:?}"
        );
    }

    #[test]
    fn load_from_omitting_notifications_section_uses_defaults() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(&path, "").expect("write");
        let cfg = Config::load_from(&path).expect("parse");
        assert!(cfg.notifications.enabled, "default is enabled=true");
        assert!(!cfg.notifications.sound, "default is sound=false");
        assert!(cfg.notifications.disabled_hosts.is_empty());
    }

    #[test]
    fn load_from_parses_notifications_section() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(
            &path,
            r#"
[notifications]
enabled = false
sound = true
disabled_hosts = ["alpenglow", "gpu-1"]
"#,
        )
        .expect("write");
        let cfg = Config::load_from(&path).expect("parse");
        assert!(!cfg.notifications.enabled);
        assert!(cfg.notifications.sound);
        assert_eq!(
            cfg.notifications.disabled_hosts,
            vec!["alpenglow".to_string(), "gpu-1".to_string()]
        );
    }

    #[test]
    fn load_from_partial_notifications_section_uses_defaults_for_missing_keys() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(
            &path,
            r"
[notifications]
sound = true
",
        )
        .expect("write");
        let cfg = Config::load_from(&path).expect("parse");
        // `enabled` defaults to true even though only `sound` is set.
        assert!(cfg.notifications.enabled);
        assert!(cfg.notifications.sound);
    }

    #[test]
    fn theme_default_resolves_to_pre_m5_colours() {
        // The default config preserves the colour scheme that shipped
        // before M5: cyan tool calls, green ok, red error, no glyph
        // colour. A bare `Config::default()` must reflect that.
        let theme = Theme::from_config(&ThemeConfig::default()).expect("default theme parses");
        assert_eq!(theme.tool_use, Some(Color::Cyan));
        assert_eq!(theme.tool_result_ok, Some(Color::Green));
        assert_eq!(theme.tool_result_err, Some(Color::Red));
        assert_eq!(theme.needs_input, None);
        assert_eq!(theme.working, None);
        assert_eq!(theme.idle, None);
        assert_eq!(theme.unknown, None);
    }

    #[test]
    fn theme_parses_named_ansi_colours() {
        let cfg = ThemeConfig {
            needs_input: Some("red".to_string()),
            working: Some("yellow".to_string()),
            idle: Some("gray".to_string()),
            unknown: Some("magenta".to_string()),
            tool_use: Some("blue".to_string()),
            tool_result_ok: Some("green".to_string()),
            tool_result_err: Some("white".to_string()),
            ..ThemeConfig::default()
        };
        let theme = Theme::from_config(&cfg).expect("parse");
        assert_eq!(theme.needs_input, Some(Color::Red));
        assert_eq!(theme.working, Some(Color::Yellow));
        assert_eq!(theme.idle, Some(Color::Gray));
        assert_eq!(theme.unknown, Some(Color::Magenta));
        assert_eq!(theme.tool_use, Some(Color::Blue));
        assert_eq!(theme.tool_result_ok, Some(Color::Green));
        assert_eq!(theme.tool_result_err, Some(Color::White));
    }

    #[test]
    fn theme_parses_bright_variants() {
        let cfg = ThemeConfig {
            needs_input: Some("bright_red".to_string()),
            ..ThemeConfig::default()
        };
        let theme = Theme::from_config(&cfg).expect("parse");
        assert_eq!(theme.needs_input, Some(Color::LightRed));
    }

    #[test]
    fn theme_parses_hex_colours() {
        let cfg = ThemeConfig {
            needs_input: Some("#FF8800".to_string()),
            ..ThemeConfig::default()
        };
        let theme = Theme::from_config(&cfg).expect("parse");
        assert_eq!(theme.needs_input, Some(Color::Rgb(0xff, 0x88, 0x00)));
    }

    #[test]
    fn theme_empty_string_and_default_keyword_both_yield_none_overriding_preset() {
        let cfg = ThemeConfig {
            tool_use: Some(String::new()),
            tool_result_ok: Some("default".to_string()),
            tool_result_err: Some("DEFAULT".to_string()),
            ..ThemeConfig::default()
        };
        let theme = Theme::from_config(&cfg).expect("parse");
        assert_eq!(theme.tool_use, None);
        assert_eq!(theme.tool_result_ok, None);
        assert_eq!(theme.tool_result_err, None);
    }

    #[test]
    fn theme_rejects_unknown_colour_with_field_name_in_error() {
        let cfg = ThemeConfig {
            needs_input: Some("puce".to_string()),
            ..ThemeConfig::default()
        };
        let err = Theme::from_config(&cfg).expect_err("should reject");
        match err {
            ConfigError::InvalidColor { field, value } => {
                assert_eq!(field, "needs_input");
                assert_eq!(value, "puce");
            }
            other => panic!("wrong error variant: {other:?}"),
        }
    }

    #[test]
    fn theme_default_preset_matches_pre_m5_colours() {
        let preset = Theme::preset("default").expect("default preset");
        assert_eq!(preset, Theme::from_config(&ThemeConfig::default()).unwrap());
    }

    #[test]
    fn theme_bright_preset_colours_every_attention_state() {
        let preset = Theme::preset("bright").expect("bright preset");
        assert!(preset.needs_input.is_some());
        assert!(preset.working.is_some());
        assert!(preset.idle.is_some());
        assert!(preset.unknown.is_some());
        assert!(preset.tool_use.is_some());
    }

    #[test]
    fn theme_mono_preset_has_no_colours() {
        let preset = Theme::preset("mono").expect("mono preset");
        assert_eq!(preset, Theme::default());
    }

    #[test]
    fn theme_preset_resolves_when_set_via_config() {
        let cfg = ThemeConfig {
            preset: Some("bright".to_string()),
            ..ThemeConfig::default()
        };
        assert_eq!(
            Theme::from_config(&cfg).unwrap(),
            Theme::preset_bright(),
            "no overrides set, so resolution equals the named preset",
        );
    }

    #[test]
    fn theme_per_field_overrides_apply_on_top_of_preset() {
        // Start from `mono` (everything None) then override a single
        // field. The other six fields stay None; only the overridden
        // one carries a colour. Confirms layering, not just preset
        // selection.
        let cfg = ThemeConfig {
            preset: Some("mono".to_string()),
            needs_input: Some("red".to_string()),
            ..ThemeConfig::default()
        };
        let theme = Theme::from_config(&cfg).expect("parse");
        assert_eq!(theme.needs_input, Some(Color::Red));
        assert_eq!(theme.working, None);
        assert_eq!(theme.tool_use, None);
    }

    #[test]
    fn theme_explicit_empty_override_resets_preset_value_to_none() {
        // `bright` preset gives every preview field a colour; setting
        // tool_use to "" should clear it specifically.
        let cfg = ThemeConfig {
            preset: Some("bright".to_string()),
            tool_use: Some(String::new()),
            ..ThemeConfig::default()
        };
        let theme = Theme::from_config(&cfg).expect("parse");
        assert_eq!(theme.tool_use, None);
        // Sibling fields still inherit from the bright preset.
        assert_eq!(theme.tool_result_ok, Some(Color::LightGreen));
    }

    #[test]
    fn every_preset_name_resolves_successfully() {
        // Doubles as a guard against `preset_names()` drifting from the
        // `match` arm in `preset()`: a name listed without an arm (or
        // vice versa) would surface here as an error or unreachable.
        for name in Theme::preset_names() {
            Theme::preset(name).unwrap_or_else(|e| panic!("preset {name:?} failed: {e:?}"));
        }
    }

    #[test]
    fn opinionated_presets_colour_needs_input() {
        // Needs-input is the primary attention signal; palettes that
        // make a deliberate aesthetic choice should keep it visible.
        // `default` preserves the pre-M5 (uncoloured-glyph) look and
        // `mono` is deliberately colour-less — both opt out.
        for name in Theme::preset_names() {
            if matches!(*name, "default" | "mono") {
                continue;
            }
            let theme = Theme::preset(name).unwrap();
            assert!(
                theme.needs_input.is_some(),
                "preset {name:?} leaves needs_input uncoloured — attention signal would vanish",
            );
        }
    }

    #[test]
    fn mono_is_the_only_fully_uncoloured_preset() {
        for name in Theme::preset_names() {
            let theme = Theme::preset(name).unwrap();
            let all_none = theme.needs_input.is_none()
                && theme.working.is_none()
                && theme.idle.is_none()
                && theme.unknown.is_none()
                && theme.tool_use.is_none()
                && theme.tool_result_ok.is_none()
                && theme.tool_result_err.is_none();
            if *name == "mono" {
                assert!(all_none, "mono preset must be fully uncoloured");
            } else {
                assert!(
                    !all_none,
                    "preset {name:?} should not match mono's emptiness"
                );
            }
        }
    }

    #[test]
    fn warm_and_cool_presets_are_distinct() {
        // Cheap smoke test that the two "palette feel" presets actually
        // differ — easy to typo identical RGB values when copy-pasting.
        assert_ne!(Theme::preset_warm(), Theme::preset_cool());
    }

    #[test]
    fn solarized_uses_canonical_red_accent() {
        // Pin one well-known value from each named palette so a
        // future refactor of the preset constants doesn't silently
        // drift away from the spec.
        let solarized = Theme::preset_solarized();
        assert_eq!(solarized.needs_input, Some(Color::Rgb(0xdc, 0x32, 0x2f)));
    }

    #[test]
    fn gruvbox_uses_bright_red_accent() {
        let gruvbox = Theme::preset_gruvbox();
        assert_eq!(gruvbox.needs_input, Some(Color::Rgb(0xfb, 0x49, 0x34)));
    }

    #[test]
    fn nord_uses_aurora_red_accent() {
        let nord = Theme::preset_nord();
        assert_eq!(nord.needs_input, Some(Color::Rgb(0xbf, 0x61, 0x6a)));
    }

    #[test]
    fn theme_unknown_preset_reports_available_names_in_error() {
        let cfg = ThemeConfig {
            preset: Some("nonexistent".to_string()),
            ..ThemeConfig::default()
        };
        let err = Theme::from_config(&cfg).expect_err("should reject");
        let msg = format!("{err}");
        assert!(msg.contains("nonexistent"), "got: {msg}");
        for name in Theme::preset_names() {
            assert!(msg.contains(name), "missing preset name {name:?}: {msg}");
        }
    }

    #[test]
    fn load_from_parses_theme_section_end_to_end() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(
            &path,
            r##"
[theme]
needs_input = "red"
tool_use = "#aabbcc"
"##,
        )
        .expect("write");
        let cfg = Config::load_from(&path).expect("parse");
        let theme = Theme::from_config(&cfg.theme).expect("resolve");
        assert_eq!(theme.needs_input, Some(Color::Red));
        assert_eq!(theme.tool_use, Some(Color::Rgb(0xaa, 0xbb, 0xcc)));
        // Defaults still in place for un-overridden fields.
        assert_eq!(theme.tool_result_ok, Some(Color::Green));
    }

    #[test]
    fn load_from_rejects_empty_ssh_target() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.toml");
        fs::write(
            &path,
            r#"
[hosts.devbox]
ssh = "   "
"#,
        )
        .expect("write");
        let err = Config::load_from(&path).expect_err("should reject");
        assert!(
            matches!(err, ConfigError::EmptySshTarget(ref n) if n == "devbox"),
            "got: {err:?}"
        );
    }
}
