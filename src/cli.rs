//! Subcommand handlers for the non-TUI entry points.
//!
//! `agent-mux themes` — coloured browser of every built-in preset, so
//! the user can see the actual palette before editing `config.toml`.
//!
//! `agent-mux config` — diagnostic view of the live-parsed configuration:
//! which file was loaded, every value agent-mux saw. Answers "is my
//! config actually being read?" without spelunking through logs.
//!
//! `agent-mux help` — subcommand overview *plus* the compact reference
//! of every config key. Reference lives here (instead of with `config`)
//! so the diagnostic view stays focused.
//!
//! All three write to a caller-supplied `Write` so the output is unit-
//! testable against an in-memory buffer. Production wiring in `main.rs`
//! passes `io::stdout()`.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use ratatui::style::Color;

use crate::config::{Config, ConfigError, Theme};
use crate::notifications::{Notifier, Payload};

/// ANSI reset escape. After every coloured glyph or label we restore the
/// terminal's default so nothing bleeds into the next line — important
/// when `agent-mux themes` is piped into a pager that doesn't strip
/// escapes automatically.
const ANSI_RESET: &str = "\x1b[0m";

/// Print every built-in theme preset with each colourable element
/// shown in its actual colour. `color` controls whether ANSI escapes
/// are emitted; `main.rs` passes `is_terminal()` so pipes/files get
/// plain text and a real TTY gets the swatch.
///
/// # Errors
///
/// Propagates any `io::Error` from `out`.
///
/// # Panics
///
/// Panics if `Theme::preset_names()` returns a name that
/// `Theme::preset()` doesn't accept — that invariant is enforced by a
/// dedicated test so a panic here would indicate a code-review miss.
pub fn print_themes<W: Write>(out: &mut W, color: bool) -> io::Result<()> {
    let presets: Vec<(&'static str, Theme)> = Theme::preset_names()
        .iter()
        .map(|name| {
            (
                *name,
                Theme::preset(name).expect("preset_names() out of sync with preset()"),
            )
        })
        .collect();

    let preset_col = std::cmp::max(
        "preset".chars().count(),
        presets
            .iter()
            .map(|(n, _)| n.chars().count())
            .max()
            .unwrap_or(0),
    );
    // For each element column, take the wider of "<glyph> <label>"
    // (the header) and the longest cell value across every preset.
    // Computed at render time rather than hard-coded so a future
    // preset with a longer colour string doesn't silently truncate.
    let col_widths: Vec<usize> = THEME_TABLE_COLUMNS
        .iter()
        .map(|(glyph, label)| {
            let header_w = format!("{glyph} {label}").chars().count();
            let max_value_w = presets
                .iter()
                .map(|(_, t)| theme_cell_text(theme_field(t, label)).chars().count())
                .max()
                .unwrap_or(0);
            std::cmp::max(header_w, max_value_w)
        })
        .collect();

    writeln!(out, "Available theme presets ({}):", presets.len())?;
    writeln!(out)?;
    write_table_header(out, preset_col, &col_widths)?;
    write_table_divider(out, preset_col, &col_widths)?;
    for (name, theme) in &presets {
        write_table_row(out, color, name, theme, preset_col, &col_widths)?;
    }
    writeln!(out)?;
    writeln!(out, "Configure in ~/.config/agent-mux/config.toml:")?;
    writeln!(out)?;
    writeln!(out, "  [theme]")?;
    writeln!(out, "  preset = \"<name>\"        # one of the names above")?;
    writeln!(
        out,
        "  needs_input = \"red\"      # optional per-field override"
    )?;
    writeln!(out)?;
    writeln!(out, "See `agent-mux help` for the full key reference.")?;
    Ok(())
}

/// Single source of truth for the table's element columns: the glyph
/// that appears in the dashboard for each attention state, plus the
/// config field name. [`theme_field`] dispatches on the label so adding
/// a column is a one-line change here once a new theme field lands.
const THEME_TABLE_COLUMNS: &[(&str, &str)] = &[
    ("✓", "needs_input"),
    ("!", "blocked"),
    ("◐", "working"),
    ("○", "idle"),
    ("·", "unknown"),
];

fn theme_field(t: &Theme, label: &str) -> Option<Color> {
    match label {
        "needs_input" => t.needs_input,
        "blocked" => t.blocked,
        "working" => t.working,
        "idle" => t.idle,
        "unknown" => t.unknown,
        _ => None,
    }
}

/// Render a colour into the textual form a user would write in
/// `config.toml`. `None` becomes an em-dash so unset overrides read as
/// "no value here, falling through to the terminal default" without
/// occupying the full "(terminal default)" phrase the per-element
/// renderer used to emit — cells need to stay narrow.
fn theme_cell_text(c: Option<Color>) -> String {
    match c {
        Some(c) => colour_label(c),
        None => "—".to_string(),
    }
}

fn write_table_header<W: Write>(
    out: &mut W,
    preset_col: usize,
    col_widths: &[usize],
) -> io::Result<()> {
    write!(out, "{:<preset_col$}", "preset")?;
    for ((glyph, label), w) in THEME_TABLE_COLUMNS.iter().zip(col_widths.iter()) {
        let header = format!("{glyph} {label}");
        write!(out, "  {header:<w$}")?;
    }
    writeln!(out)
}

fn write_table_divider<W: Write>(
    out: &mut W,
    preset_col: usize,
    col_widths: &[usize],
) -> io::Result<()> {
    write!(out, "{}", "─".repeat(preset_col))?;
    for w in col_widths {
        write!(out, "  {}", "─".repeat(*w))?;
    }
    writeln!(out)
}

fn write_table_row<W: Write>(
    out: &mut W,
    color: bool,
    name: &str,
    theme: &Theme,
    preset_col: usize,
    col_widths: &[usize],
) -> io::Result<()> {
    write!(out, "{name:<preset_col$}")?;
    for ((_, label), w) in THEME_TABLE_COLUMNS.iter().zip(col_widths.iter()) {
        let c = theme_field(theme, label);
        let txt = theme_cell_text(c);
        write!(out, "  ")?;
        // Colour is opt-in (TTY-only); when on, wrap the cell value —
        // not the padding — in the colour escape so terminals that
        // honour background highlighting don't bleed colour across the
        // padded gap.
        if color && let Some(c) = c {
            write_ansi_fg(out, c)?;
            write!(out, "{txt}")?;
            write!(out, "{ANSI_RESET}")?;
            let pad = w.saturating_sub(txt.chars().count());
            for _ in 0..pad {
                write!(out, " ")?;
            }
        } else {
            write!(out, "{txt:<w$}")?;
        }
    }
    writeln!(out)
}

fn colour_label(c: Color) -> String {
    match c {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Reset => "reset".to_string(),
        Color::Black => "black".to_string(),
        Color::Red => "red".to_string(),
        Color::Green => "green".to_string(),
        Color::Yellow => "yellow".to_string(),
        Color::Blue => "blue".to_string(),
        Color::Magenta => "magenta".to_string(),
        Color::Cyan => "cyan".to_string(),
        Color::Gray => "gray".to_string(),
        Color::DarkGray => "darkgray".to_string(),
        Color::LightRed => "bright_red".to_string(),
        Color::LightGreen => "bright_green".to_string(),
        Color::LightYellow => "bright_yellow".to_string(),
        Color::LightBlue => "bright_blue".to_string(),
        Color::LightMagenta => "bright_magenta".to_string(),
        Color::LightCyan => "bright_cyan".to_string(),
        Color::White => "white".to_string(),
        Color::Indexed(n) => format!("ansi-{n}"),
    }
}

fn write_ansi_fg<W: Write>(out: &mut W, c: Color) -> io::Result<()> {
    match c {
        Color::Reset => write!(out, "{ANSI_RESET}"),
        Color::Black => write!(out, "\x1b[30m"),
        Color::Red => write!(out, "\x1b[31m"),
        Color::Green => write!(out, "\x1b[32m"),
        Color::Yellow => write!(out, "\x1b[33m"),
        Color::Blue => write!(out, "\x1b[34m"),
        Color::Magenta => write!(out, "\x1b[35m"),
        Color::Cyan => write!(out, "\x1b[36m"),
        Color::Gray => write!(out, "\x1b[37m"),
        Color::DarkGray => write!(out, "\x1b[90m"),
        Color::LightRed => write!(out, "\x1b[91m"),
        Color::LightGreen => write!(out, "\x1b[92m"),
        Color::LightYellow => write!(out, "\x1b[93m"),
        Color::LightBlue => write!(out, "\x1b[94m"),
        Color::LightMagenta => write!(out, "\x1b[95m"),
        Color::LightCyan => write!(out, "\x1b[96m"),
        Color::White => write!(out, "\x1b[97m"),
        Color::Rgb(r, g, b) => write!(out, "\x1b[38;2;{r};{g};{b}m"),
        Color::Indexed(n) => write!(out, "\x1b[38;5;{n}m"),
    }
}

/// Print the current resolved config state: which path agent-mux
/// loaded (or searched, if none existed) and the parsed values. This
/// is the answer to "is my config actually being read?" — kept tight
/// so the diagnostic signal isn't buried under reference scroll. The
/// config-key reference moved to `agent-mux help` 2026-05-20.
///
/// `searched` is the priority-ordered list of paths agent-mux looked
/// at; `loaded_from` is `Some(path)` when one of them existed and was
/// read, `None` when every candidate was absent. `result` is the
/// outcome of the load — `Err` paths surface what failed.
///
/// # Errors
///
/// Propagates any `io::Error` from `out`.
pub fn print_config<W: Write>(
    out: &mut W,
    searched: &[PathBuf],
    loaded_from: Option<&Path>,
    result: &Result<Config, ConfigError>,
) -> io::Result<()> {
    print_config_status(out, searched, loaded_from, result)
}

/// Print only the status block. Identical to [`print_config`] today —
/// the indirection survives from when `print_config` also emitted the
/// long reference, and stays around so a future addition to `config`'s
/// surface (e.g. flagging unknown keys) can extend `print_config`
/// without disturbing this slice.
///
/// # Errors
///
/// Propagates any `io::Error` from `out`.
pub fn print_config_status<W: Write>(
    out: &mut W,
    searched: &[PathBuf],
    loaded_from: Option<&Path>,
    result: &Result<Config, ConfigError>,
) -> io::Result<()> {
    writeln!(out, "Current configuration")?;
    writeln!(out, "─────────────────────")?;
    if let Some(p) = loaded_from {
        writeln!(out, "  loaded from: {}", p.display())?;
    } else {
        writeln!(out, "  no config file found. searched:")?;
        if searched.is_empty() {
            writeln!(out, "    (no candidate paths — $HOME unresolved)")?;
        } else {
            for p in searched {
                writeln!(out, "    - {}", p.display())?;
            }
        }
        writeln!(out, "  using built-in defaults.")?;
    }
    match result {
        Ok(cfg) => print_parsed_config(out, cfg)?,
        Err(e) => {
            writeln!(out, "  parse failed: {e}")?;
            writeln!(out, "  using built-in defaults.")?;
        }
    }
    Ok(())
}

fn print_parsed_config<W: Write>(out: &mut W, cfg: &Config) -> io::Result<()> {
    writeln!(
        out,
        "  workspace_folders ({}):",
        cfg.workspace_folders.len()
    )?;
    if cfg.workspace_folders.is_empty() {
        writeln!(out, "    (none — `n` to create a session will be disabled)")?;
    } else {
        for f in &cfg.workspace_folders {
            writeln!(out, "    - {}", f.display())?;
        }
    }
    writeln!(out, "  hosts ({}):", cfg.hosts.len())?;
    if cfg.hosts.is_empty() {
        writeln!(out, "    (none — only the local host will show sessions)")?;
    } else {
        for (name, h) in &cfg.hosts {
            writeln!(
                out,
                "    - {name}: ssh={:?}, transcript_root={}",
                h.ssh,
                h.transcript_root.display()
            )?;
        }
    }
    writeln!(
        out,
        "  notifications: enabled={}, sound={}, sound_file={}, backend={:?}, disabled_hosts={:?}",
        cfg.notifications.enabled,
        cfg.notifications.sound,
        cfg.notifications
            .sound_file
            .as_ref()
            .map_or("none".to_string(), |p| p.display().to_string()),
        cfg.notifications.backend,
        cfg.notifications.disabled_hosts,
    )?;
    let preset = cfg.theme.preset.as_deref().unwrap_or("default");
    let overrides = theme_override_count(&cfg.theme);
    writeln!(
        out,
        "  theme: preset={preset:?}, per-field overrides={overrides}",
    )?;
    writeln!(out, "  tools ({}):", cfg.tools.len())?;
    if cfg.tools.is_empty() {
        writeln!(out, "    (none)")?;
    } else {
        for t in &cfg.tools {
            let name = t.name.as_deref().unwrap_or("");
            let label = if name.is_empty() {
                String::new()
            } else {
                format!(" ({name})")
            };
            writeln!(out, "    - {} → {:?}{label}", t.key, t.command)?;
        }
    }
    let cap = cfg.ui.sessions_per_project;
    let cap_desc = if cap == 0 {
        "0 (no cap)".to_string()
    } else {
        cap.to_string()
    };
    writeln!(out, "  ui: sessions_per_project={cap_desc}")?;
    Ok(())
}

fn theme_override_count(t: &crate::config::ThemeConfig) -> usize {
    [
        &t.needs_input,
        &t.blocked,
        &t.working,
        &t.idle,
        &t.unknown,
        &t.focus_border,
        &t.selection,
        &t.background,
        &t.sidebar_bg,
    ]
    .iter()
    .filter(|f| f.is_some())
    .count()
}

/// Print a compact one-screen reference of every config key. Lives at
/// the bottom of `agent-mux help` (previously the long-form TOML
/// skeleton printed by `agent-mux config`, but dogfooding 2026-05-20
/// surfaced that anyone who needs the reference is already in "look
/// it up" mode — `help` is the natural home, and the user is browsing
/// rather than copy-pasting). `agent-mux config` now only prints the
/// live-parsed status so its diagnostic value isn't buried under
/// reference scroll.
///
/// # Errors
///
/// Propagates any `io::Error` from `out`.
pub fn print_config_reference<W: Write>(out: &mut W) -> io::Result<()> {
    let preset_list = Theme::preset_names().join(", ");
    writeln!(out, "CONFIG KEYS (~/.config/agent-mux/config.toml):")?;
    writeln!(
        out,
        "  workspace_folders = []         absolute paths; depth-1 git scan"
    )?;
    writeln!(
        out,
        "  [hosts.<name>]                 one per SSH-reachable host (`local` is reserved)"
    )?;
    writeln!(
        out,
        "    ssh = \"alias-or-user@host\"   required: ~/.ssh/config alias or user@host"
    )?;
    writeln!(
        out,
        "    transcript_root = \"...\"      default ~/.claude/projects (tilde → remote home)"
    )?;
    writeln!(
        out,
        "    workspace_folders = [...]    optional per-host; tildes → remote home"
    )?;
    writeln!(
        out,
        "  [notifications]                enabled, sound, sound_file, backend, disabled_hosts — all optional"
    )?;
    writeln!(
        out,
        "    backend = \"auto\"             one of: auto, dbus, osascript, wsl-toast"
    )?;
    writeln!(
        out,
        "    sound_file = \"/abs/path\"     audio file (absolute path; tildes rejected);"
    )?;
    writeln!(
        out,
        "                                 plays via afplay (macOS) or ffplay/paplay (Linux);"
    )?;
    writeln!(
        out,
        "                                 overrides sound=true so the file plays alone."
    )?;
    writeln!(out, "  [theme]")?;
    writeln!(
        out,
        "    preset = \"default\"           one of: {preset_list}"
    )?;
    writeln!(
        out,
        "    needs_input(done) / blocked / working / idle / unknown   accent colours"
    )?;
    writeln!(
        out,
        "    focus_border / selection / background / sidebar_bg       structural colours"
    )?;
    writeln!(
        out,
        "                                 colour: \"red\" | \"bright_red\" | \"#RRGGBB\" | \"\""
    )?;
    writeln!(out, "  [[tools]]                      custom keybinds")?;
    writeln!(
        out,
        "    key = \"g\", command = [...]   single char (not q j k J K t n N p d /);"
    )?;
    writeln!(
        out,
        "                                 {{cwd}} and {{host}} substituted at fire time."
    )?;
    writeln!(
        out,
        "    name = \"...\"                 optional launch label (not \"terminal\")"
    )?;
    writeln!(out, "  [ui]")?;
    writeln!(
        out,
        "    sessions_per_project = 5     max rows per project before `+ K more` (0 = no cap;"
    )?;
    writeln!(
        out,
        "                                 lifted while searching)"
    )?;
    writeln!(out)?;
    writeln!(
        out,
        "`agent-mux config` shows the live-parsed values + which file was loaded."
    )?;
    Ok(())
}

/// Fire one test notification using the current notifier config and
/// print a one-line confirmation describing what was sent. Lets a user
/// verify the full pipeline (backend pick, dispatcher reachability,
/// `sound_file` playback) end-to-end without provoking a real session
/// transition.
///
/// The synthetic payload uses a fixed title ("test notification"), the
/// literal host label `local`, and a placeholder project path so the
/// result is recognisable on the user's notification surface. The
/// backend label is included in the confirmation line so a misfiring
/// auto-probe is immediately visible.
///
/// # Errors
///
/// Propagates any `io::Error` from `out`, or a non-empty error string
/// from the dispatcher (which lands as an `io::Error::Other`).
pub fn print_notify_test<W: Write>(
    out: &mut W,
    notifier: &Notifier,
    backend_label: &str,
) -> io::Result<()> {
    let payload = notifier.test_payload(
        "test notification",
        "local",
        Path::new("/agent-mux/notify-test"),
    );
    write_notify_test_confirmation(out, &payload, backend_label)?;
    notifier
        .dispatch_test(payload)
        .map_err(|e| io::Error::other(format!("dispatcher: {e}")))?;
    Ok(())
}

/// Write the one-line confirmation describing the payload the
/// subcommand is about to dispatch. Lifted out so the formatting is
/// unit-testable against an in-memory buffer without actually firing.
fn write_notify_test_confirmation<W: Write>(
    out: &mut W,
    payload: &Payload,
    backend_label: &str,
) -> io::Result<()> {
    let sound = if let Some(path) = &payload.sound_file {
        format!("file {}", path.display())
    } else if payload.sound {
        "OS default".to_string()
    } else {
        "silent".to_string()
    };
    writeln!(
        out,
        "Firing test notification via backend={backend_label}, sound={sound}"
    )?;
    writeln!(out, "  title: {}", payload.title)?;
    writeln!(out, "  body:  {}", payload.body)?;
    Ok(())
}

/// One-screen overview of the available subcommands. Printed for `help`,
/// `--help`, `-h`, and (to stderr) for any unrecognised invocation.
///
/// # Errors
///
/// Propagates any `io::Error` from `out`.
pub fn print_help<W: Write>(out: &mut W) -> io::Result<()> {
    writeln!(
        out,
        "agent-mux — terminal multiplexer for Claude Code sessions"
    )?;
    writeln!(out)?;
    writeln!(out, "USAGE:")?;
    writeln!(out, "  agent-mux [--no-embed]   Launch the dashboard TUI.")?;
    writeln!(
        out,
        "  agent-mux themes         Show every built-in theme preset with colour samples."
    )?;
    writeln!(
        out,
        "  agent-mux config         Print the live-parsed config (which file was loaded, parsed values)."
    )?;
    writeln!(
        out,
        "  agent-mux notify-test    Fire one test notification using the current config."
    )?;
    writeln!(
        out,
        "  agent-mux install-hooks  Wire Claude Code's Notification hook into ~/.claude/settings.json"
    )?;
    writeln!(
        out,
        "                           (idempotent; --dry-run prints the change without writing)."
    )?;
    writeln!(
        out,
        "  agent-mux hook           Internal: invoked by Claude Code's Notification hook."
    )?;
    writeln!(out, "  agent-mux help           Show this help.")?;
    writeln!(out)?;
    writeln!(out, "FLAGS:")?;
    writeln!(
        out,
        "  --no-embed               Disable the embedded PTY pane and revert to legacy"
    )?;
    writeln!(
        out,
        "                           `tmux switch-client` / foreground `tmux attach`"
    )?;
    writeln!(
        out,
        "                           behaviour. Useful when you prefer agent-mux to"
    )?;
    writeln!(
        out,
        "                           hand off the whole terminal rather than host a pane."
    )?;
    writeln!(out)?;
    print_config_reference(out)?;
    writeln!(out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run<F>(f: F) -> String
    where
        F: FnOnce(&mut Vec<u8>) -> io::Result<()>,
    {
        let mut buf = Vec::new();
        f(&mut buf).expect("subcommand writes succeed against an in-memory buffer");
        String::from_utf8(buf).expect("subcommand output is valid UTF-8")
    }

    #[test]
    fn themes_lists_every_named_preset() {
        let out = run(|w| print_themes(w, false));
        for name in Theme::preset_names() {
            assert!(out.contains(name), "missing preset name {name:?}: {out}");
        }
    }

    #[test]
    fn themes_shows_every_colourable_element_per_preset() {
        // Element labels are part of the contract — the user should be
        // able to see needs_input / working / etc. by name so the
        // swatch maps back to the config key without guessing.
        let out = run(|w| print_themes(w, false));
        for field in ["needs_input", "blocked", "working", "idle", "unknown"] {
            assert!(out.contains(field), "missing element {field:?}: {out}");
        }
    }

    #[test]
    fn themes_emits_no_ansi_when_color_is_false() {
        let out = run(|w| print_themes(w, false));
        assert!(
            !out.contains('\x1b'),
            "color=false should produce plain text; got escape:\n{out}"
        );
    }

    #[test]
    fn themes_emits_ansi_when_color_is_true() {
        let out = run(|w| print_themes(w, true));
        assert!(
            out.contains('\x1b'),
            "color=true should emit ANSI escapes for at least one row"
        );
        // Reset must follow every coloured run — otherwise the next
        // line bleeds the previous colour.
        assert!(out.contains(ANSI_RESET));
    }

    #[test]
    fn themes_renders_solarized_hex_as_rgb_escape() {
        // Spec-pin: the solarized red maps to a truecolor escape so
        // the swatch reproduces the canonical value on terminals
        // that support 24-bit colour.
        let out = run(|w| print_themes(w, true));
        assert!(
            out.contains("38;2;220;50;47"),
            "expected truecolor escape for #dc322f (220, 50, 47):\n{out}"
        );
    }

    #[test]
    fn themes_marks_uncoloured_cells_with_em_dash() {
        // The `default` preset leaves needs_input/working/idle/unknown
        // uncoloured. The table should make that visible — the
        // user-facing meaning is "no override, terminal foreground
        // applies" — rather than silently leaving the cells empty.
        // Em-dash keeps the column narrow while still flagging the
        // absence.
        let out = run(|w| print_themes(w, false));
        assert!(
            out.contains("—"),
            "expected an em-dash marker for unset cells:\n{out}"
        );
    }

    #[test]
    fn config_includes_every_section() {
        let out = run(print_config_reference);
        for section in ["workspace_folders", "[hosts", "[notifications]", "[theme]"] {
            assert!(out.contains(section), "missing section {section:?}:\n{out}");
        }
    }

    #[test]
    fn config_lists_every_preset_name() {
        let out = run(print_config_reference);
        for name in Theme::preset_names() {
            assert!(out.contains(name), "missing preset name {name:?}:\n{out}");
        }
    }

    #[test]
    fn config_documents_every_theme_field() {
        let out = run(print_config_reference);
        for field in ["preset", "needs_input", "working", "idle", "unknown"] {
            assert!(out.contains(field), "theme.{field} not documented:\n{out}");
        }
    }

    #[test]
    fn config_documents_every_notifications_field() {
        let out = run(print_config_reference);
        for field in ["enabled", "sound", "disabled_hosts", "backend"] {
            assert!(
                out.contains(field),
                "notifications.{field} not documented:\n{out}"
            );
        }
    }

    #[test]
    fn status_reports_loaded_path_and_parsed_values() {
        // Happy path: config exists, parsed cleanly. The status block
        // names the path, the workspace folders, and each configured
        // host so a user can verify the file they edited is the one
        // agent-mux read.
        let cfg = Config {
            workspace_folders: vec![PathBuf::from("/h/work")],
            hosts: {
                let mut m = std::collections::BTreeMap::new();
                m.insert(
                    "devbox".to_string(),
                    crate::config::HostConfig {
                        ssh: "devbox.internal".to_string(),
                        transcript_root: PathBuf::from("~/.claude/projects"),
                        workspace_folders: None,
                    },
                );
                m
            },
            ..Config::default()
        };
        let path = PathBuf::from("/h/.config/agent-mux/config.toml");
        let out =
            run(|w| print_config_status(w, std::slice::from_ref(&path), Some(&path), &Ok(cfg)));
        assert!(
            out.contains("/h/.config/agent-mux/config.toml"),
            "missing loaded path:\n{out}"
        );
        assert!(out.contains("/h/work"), "missing workspace folder:\n{out}");
        assert!(out.contains("devbox"), "missing host name:\n{out}");
        assert!(
            out.contains("devbox.internal"),
            "missing ssh target:\n{out}"
        );
    }

    #[test]
    fn status_reports_when_no_config_file_exists() {
        // The bug this output is meant to catch: the user edited
        // ~/.config/agent-mux/config.toml but agent-mux was looking
        // at ~/Library/Application Support/. The status section
        // must enumerate every searched path so the mismatch is
        // visible at a glance.
        let searched = vec![
            PathBuf::from("/h/.config/agent-mux/config.toml"),
            PathBuf::from("/h/Library/Application Support/agent-mux/config.toml"),
        ];
        let out = run(|w| print_config_status(w, &searched, None, &Ok(Config::default())));
        assert!(
            out.contains("no config file found"),
            "missing not-found marker:\n{out}"
        );
        for p in &searched {
            let s = p.display().to_string();
            assert!(out.contains(&s), "missing searched path {s:?}:\n{out}");
        }
        assert!(
            out.contains("built-in defaults"),
            "missing defaults notice:\n{out}"
        );
    }

    #[test]
    fn status_reports_parse_error() {
        // A malformed TOML file is the third common failure mode
        // (after "wrong path" and "missing file"). The status block
        // should surface the parser's message so the user knows
        // where to look.
        let path = PathBuf::from("/h/.config/agent-mux/config.toml");
        let err: Result<Config, ConfigError> = match toml::from_str::<Config>("not = valid = toml")
        {
            Err(e) => Err(ConfigError::Parse(e)),
            Ok(_) => panic!("test fixture should be malformed TOML"),
        };
        let out = run(|w| print_config_status(w, std::slice::from_ref(&path), Some(&path), &err));
        assert!(
            out.contains("parse failed"),
            "missing parse-failed marker:\n{out}"
        );
    }

    #[test]
    fn config_prints_only_status_no_reference() {
        // The reference moved to `agent-mux help` 2026-05-20 — `config`
        // is the diagnostic surface ("what did agent-mux actually parse?"),
        // so it stays focused on the live state. Reference scroll was
        // burying that signal.
        let path = PathBuf::from("/h/.config/agent-mux/config.toml");
        let out = run(|w| {
            print_config(
                w,
                std::slice::from_ref(&path),
                Some(&path),
                &Ok(Config::default()),
            )
        });
        assert!(
            out.contains("Current configuration"),
            "missing status header:\n{out}"
        );
        assert!(
            !out.contains("CONFIG KEYS"),
            "reference moved to help; config should no longer print it:\n{out}"
        );
    }

    #[test]
    fn help_lists_every_subcommand() {
        let out = run(print_help);
        for cmd in [
            "themes",
            "config",
            "notify-test",
            "install-hooks",
            "hook",
            "help",
        ] {
            assert!(out.contains(cmd), "missing subcommand {cmd:?}:\n{out}");
        }
    }

    #[test]
    fn config_reference_documents_sound_file_field() {
        let out = run(print_config_reference);
        assert!(
            out.contains("sound_file"),
            "notifications.sound_file not documented:\n{out}"
        );
    }

    #[test]
    fn notify_test_confirmation_describes_sound_file_when_set() {
        let payload = Payload {
            title: "agent-mux: x".into(),
            body: "local · /p".into(),
            sound: false,
            sound_file: Some(PathBuf::from("/abs/ping.mp3")),
        };
        let out = run(|w| write_notify_test_confirmation(w, &payload, "osascript"));
        assert!(out.contains("backend=osascript"), "missing backend:\n{out}");
        assert!(out.contains("file /abs/ping.mp3"), "missing path:\n{out}");
        assert!(out.contains("title: agent-mux: x"), "missing title:\n{out}");
        assert!(out.contains("body:  local · /p"), "missing body:\n{out}");
    }

    #[test]
    fn notify_test_confirmation_reports_os_default_when_sound_true_and_no_file() {
        let payload = Payload {
            title: "x".into(),
            body: "y".into(),
            sound: true,
            sound_file: None,
        };
        let out = run(|w| write_notify_test_confirmation(w, &payload, "osascript"));
        assert!(
            out.contains("sound=OS default"),
            "expected OS-default:\n{out}"
        );
    }

    #[test]
    fn notify_test_confirmation_reports_silent_when_no_audio_at_all() {
        let payload = Payload {
            title: "x".into(),
            body: "y".into(),
            sound: false,
            sound_file: None,
        };
        let out = run(|w| write_notify_test_confirmation(w, &payload, "osascript"));
        assert!(out.contains("sound=silent"), "expected silent:\n{out}");
    }

    #[test]
    fn parsed_config_status_surfaces_sound_file_path() {
        let mut cfg = Config::default();
        cfg.notifications.sound_file = Some(PathBuf::from("/abs/Tink.aiff"));
        let path = PathBuf::from("/h/.config/agent-mux/config.toml");
        let out =
            run(|w| print_config_status(w, std::slice::from_ref(&path), Some(&path), &Ok(cfg)));
        assert!(
            out.contains("sound_file=/abs/Tink.aiff"),
            "missing sound_file in parsed config:\n{out}"
        );
    }

    #[test]
    fn help_ends_with_compact_config_reference() {
        // The reference reads at the bottom of help so the
        // subcommand/flag listing leads. Every section / theme field /
        // preset still needs to appear so users can write a config
        // without leaving this output.
        let out = run(print_help);
        for marker in [
            "CONFIG KEYS",
            "workspace_folders",
            "[hosts.<name>]",
            "[notifications]",
            "[theme]",
            "[[tools]]",
            "needs_input",
        ] {
            assert!(out.contains(marker), "missing {marker:?} in help:\n{out}");
        }
        for preset in Theme::preset_names() {
            assert!(
                out.contains(preset),
                "missing preset {preset:?} in help:\n{out}"
            );
        }
    }
}
