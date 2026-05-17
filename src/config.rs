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

/// Raw theme strings from `[theme]` in `config.toml`. Each field is the
/// colour name for one semantic UI element. Empty string = "use the
/// terminal's default foreground" (no `fg` set on the `Style`). Defaults
/// reproduce the colour scheme that shipped before M5 so an empty config
/// renders identically.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ThemeConfig {
    /// Colour of the `●` glyph for sessions in `NeedsInput`.
    pub needs_input: String,
    /// Colour of the `◐` glyph for sessions currently `Working`.
    pub working: String,
    /// Colour of the `○` glyph for `Idle` sessions.
    pub idle: String,
    /// Colour of the `·` glyph for `Unknown` (no signal yet) sessions.
    pub unknown: String,
    /// Colour of `⚒ Tool: …` lines in the preview pane.
    pub tool_use: String,
    /// Colour of `↳ ok` lines in the preview pane.
    pub tool_result_ok: String,
    /// Colour of `↳ error` lines in the preview pane.
    pub tool_result_err: String,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            needs_input: String::new(),
            working: String::new(),
            idle: String::new(),
            unknown: String::new(),
            tool_use: "cyan".to_string(),
            tool_result_ok: "green".to_string(),
            tool_result_err: "red".to_string(),
        }
    }
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
}

impl Theme {
    /// Parse every field of `cfg` into a `Color`. Empty strings (and
    /// the literal `"default"`) resolve to `None`. Unrecognised names
    /// produce [`ConfigError::InvalidColor`] tagged with the offending
    /// field name so the user can find it without grepping.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidColor`] on the first malformed
    /// colour string. Subsequent malformed fields are not reported in
    /// the same pass — fixing the first error and re-loading surfaces
    /// the next.
    pub fn from_config(cfg: &ThemeConfig) -> Result<Self, ConfigError> {
        Ok(Self {
            needs_input: parse_color(&cfg.needs_input, "needs_input")?,
            working: parse_color(&cfg.working, "working")?,
            idle: parse_color(&cfg.idle, "idle")?,
            unknown: parse_color(&cfg.unknown, "unknown")?,
            tool_use: parse_color(&cfg.tool_use, "tool_use")?,
            tool_result_ok: parse_color(&cfg.tool_result_ok, "tool_result_ok")?,
            tool_result_err: parse_color(&cfg.tool_result_err, "tool_result_err")?,
        })
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
            needs_input: "red".to_string(),
            working: "yellow".to_string(),
            idle: "gray".to_string(),
            unknown: "magenta".to_string(),
            tool_use: "blue".to_string(),
            tool_result_ok: "green".to_string(),
            tool_result_err: "white".to_string(),
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
            needs_input: "bright_red".to_string(),
            ..ThemeConfig::default()
        };
        let theme = Theme::from_config(&cfg).expect("parse");
        assert_eq!(theme.needs_input, Some(Color::LightRed));
    }

    #[test]
    fn theme_parses_hex_colours() {
        let cfg = ThemeConfig {
            needs_input: "#FF8800".to_string(),
            ..ThemeConfig::default()
        };
        let theme = Theme::from_config(&cfg).expect("parse");
        assert_eq!(theme.needs_input, Some(Color::Rgb(0xff, 0x88, 0x00)));
    }

    #[test]
    fn theme_empty_string_and_default_keyword_both_yield_none() {
        let cfg = ThemeConfig {
            tool_use: String::new(),
            tool_result_ok: "default".to_string(),
            tool_result_err: "DEFAULT".to_string(),
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
            needs_input: "puce".to_string(),
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
