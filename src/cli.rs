//! Subcommand handlers for the non-TUI entry points.
//!
//! `agent-mux themes` — coloured browser of every built-in preset, so
//! the user can see the actual palette before editing `config.toml`.
//!
//! `agent-mux config` — printable reference of every config key, its
//! default, and a one-line description. Doubles as the answer to
//! "what can I tune here?" without spelunking through README/SPEC.
//!
//! Both write to a caller-supplied `Write` so the output is unit-testable
//! against an in-memory buffer. Production wiring in `main.rs` passes
//! `io::stdout()`.

use std::io::{self, Write};

use ratatui::style::Color;

use crate::config::Theme;

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
    writeln!(
        out,
        "Available theme presets ({}):",
        Theme::preset_names().len()
    )?;
    writeln!(out)?;
    for name in Theme::preset_names() {
        // .unwrap() is safe: every name in `preset_names()` is matched
        // in `preset()` (and the matching is enforced by a test).
        let theme = Theme::preset(name).expect("preset_names() out of sync with preset()");
        writeln!(out, "  {name}")?;
        print_theme_row(out, color, "●", "needs_input    ", theme.needs_input)?;
        print_theme_row(out, color, "◐", "working        ", theme.working)?;
        print_theme_row(out, color, "○", "idle           ", theme.idle)?;
        print_theme_row(out, color, "·", "unknown        ", theme.unknown)?;
        print_theme_row(out, color, "⚒", "tool_use       ", theme.tool_use)?;
        print_theme_row(out, color, "↳", "tool_result_ok ", theme.tool_result_ok)?;
        print_theme_row(out, color, "↳", "tool_result_err", theme.tool_result_err)?;
        writeln!(out)?;
    }
    writeln!(out, "Set in ~/.config/agent-mux/config.toml:")?;
    writeln!(out)?;
    writeln!(out, "  [theme]")?;
    writeln!(out, "  preset = \"<name>\"        # one of the names above")?;
    writeln!(
        out,
        "  needs_input = \"red\"      # optional per-field override"
    )?;
    writeln!(out)?;
    writeln!(out, "See `agent-mux config` for the full key reference.")?;
    Ok(())
}

fn print_theme_row<W: Write>(
    out: &mut W,
    color: bool,
    glyph: &str,
    name: &str,
    c: Option<Color>,
) -> io::Result<()> {
    write!(out, "    ")?;
    match c {
        Some(c) => {
            // Glyph in the preset's colour, label also in the colour so
            // the user can read "this is what tool_result_ok looks like
            // on my terminal" with one glance.
            if color {
                write_ansi_fg(out, c)?;
            }
            write!(out, "{glyph}")?;
            if color {
                write!(out, "{ANSI_RESET}")?;
            }
            write!(out, "  {name}  ")?;
            if color {
                write_ansi_fg(out, c)?;
            }
            write!(out, "{}", colour_label(c))?;
            if color {
                write!(out, "{ANSI_RESET}")?;
            }
        }
        None => {
            write!(out, "{glyph}  {name}  (terminal default)")?;
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

/// Print a reference of every config key with its default and a
/// one-line description. The output is a self-contained TOML skeleton
/// the user can copy verbatim into `config.toml` and tune from.
///
/// # Errors
///
/// Propagates any `io::Error` from `out`.
pub fn print_config<W: Write>(out: &mut W) -> io::Result<()> {
    writeln!(out, "Configuration: ~/.config/agent-mux/config.toml")?;
    writeln!(
        out,
        "Missing file or missing keys fall back to the defaults below."
    )?;
    writeln!(out)?;
    writeln!(
        out,
        "# ─── Workspaces (M1) ────────────────────────────────"
    )?;
    writeln!(
        out,
        "workspace_folders = []  # depth-1 scanned for git repos at startup;"
    )?;
    writeln!(
        out,
        "                        # tilde-expanded. Env-var expansion is post-M5."
    )?;
    writeln!(out)?;
    writeln!(
        out,
        "# ─── Remote SSH hosts (M2) ──────────────────────────"
    )?;
    writeln!(out, "# One table per host. `local` is reserved.")?;
    writeln!(out, "# [hosts.alpenglow]")?;
    writeln!(
        out,
        "# ssh = \"alpenglow\"                       # required: ~/.ssh/config alias or user@host"
    )?;
    writeln!(
        out,
        "# transcript_root = \"~/.claude/projects\"  # default; tilde resolves on the remote"
    )?;
    writeln!(out)?;
    writeln!(out, "# ─── Notifications (M4 + M5) ───────────────────────")?;
    writeln!(out, "[notifications]")?;
    writeln!(
        out,
        "enabled = true          # master on/off — when false, every dispatch is suppressed"
    )?;
    writeln!(
        out,
        "sound = false           # request the OS \"default\" notification sound"
    )?;
    writeln!(
        out,
        "disabled_hosts = []     # silence specific hosts by their [hosts.<name>] label"
    )?;
    writeln!(out)?;
    writeln!(
        out,
        "# ─── Theme (M5) ─────────────────────────────────────"
    )?;
    writeln!(out, "[theme]")?;
    let preset_list = Theme::preset_names()
        .iter()
        .map(|s| format!("\"{s}\""))
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(out, "preset = \"default\"      # one of: {preset_list}")?;
    writeln!(
        out,
        "                         # see `agent-mux themes` for a coloured browser."
    )?;
    writeln!(out, "# Per-field overrides apply on top of the preset.")?;
    writeln!(out, "# Each value: named ANSI (\"red\"), bright_* variant")?;
    writeln!(
        out,
        "# (\"bright_red\"), #RRGGBB hex, or empty for terminal default."
    )?;
    writeln!(
        out,
        "needs_input     = \"\"    # ● glyph for needs-input sessions"
    )?;
    writeln!(
        out,
        "working         = \"\"    # ◐ glyph for working sessions"
    )?;
    writeln!(out, "idle            = \"\"    # ○ glyph for idle sessions")?;
    writeln!(
        out,
        "unknown         = \"\"    # · glyph for unknown-state sessions"
    )?;
    writeln!(
        out,
        "tool_use        = \"\"    # ⚒ Tool: … lines in preview pane"
    )?;
    writeln!(
        out,
        "tool_result_ok  = \"\"    # ↳ ok lines in preview pane"
    )?;
    writeln!(
        out,
        "tool_result_err = \"\"    # ↳ error lines in preview pane"
    )?;
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
    writeln!(out, "  agent-mux                Launch the dashboard TUI.")?;
    writeln!(
        out,
        "  agent-mux themes         Show every built-in theme preset with colour samples."
    )?;
    writeln!(
        out,
        "  agent-mux config         Print every config key, its default, and a description."
    )?;
    writeln!(out, "  agent-mux help           Show this help.")?;
    writeln!(out)?;
    writeln!(
        out,
        "Configuration lives at ~/.config/agent-mux/config.toml."
    )?;
    writeln!(
        out,
        "See SPEC.md / ARCHITECTURE.md / PROCESS.md for the canonical project docs."
    )?;
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
        // able to see needs_input / tool_result_ok / etc. by name so
        // the swatch maps back to the config key without guessing.
        let out = run(|w| print_themes(w, false));
        for field in [
            "needs_input",
            "working",
            "idle",
            "unknown",
            "tool_use",
            "tool_result_ok",
            "tool_result_err",
        ] {
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
    fn themes_labels_uncoloured_elements_as_terminal_default() {
        // The `default` preset leaves needs_input/working/idle/unknown
        // uncoloured. The browser should make that visible rather than
        // silently omitting the row.
        let out = run(|w| print_themes(w, false));
        assert!(
            out.contains("terminal default"),
            "expected an explicit 'terminal default' marker:\n{out}"
        );
    }

    #[test]
    fn config_includes_every_section() {
        let out = run(print_config);
        for section in ["workspace_folders", "[hosts", "[notifications]", "[theme]"] {
            assert!(out.contains(section), "missing section {section:?}:\n{out}");
        }
    }

    #[test]
    fn config_lists_every_preset_name() {
        let out = run(print_config);
        for name in Theme::preset_names() {
            assert!(out.contains(name), "missing preset name {name:?}:\n{out}");
        }
    }

    #[test]
    fn config_documents_every_theme_field() {
        let out = run(print_config);
        for field in [
            "preset",
            "needs_input",
            "working",
            "idle",
            "unknown",
            "tool_use",
            "tool_result_ok",
            "tool_result_err",
        ] {
            assert!(out.contains(field), "theme.{field} not documented:\n{out}");
        }
    }

    #[test]
    fn config_documents_every_notifications_field() {
        let out = run(print_config);
        for field in ["enabled", "sound", "disabled_hosts"] {
            assert!(
                out.contains(field),
                "notifications.{field} not documented:\n{out}"
            );
        }
    }

    #[test]
    fn help_lists_every_subcommand() {
        let out = run(print_help);
        for cmd in ["themes", "config", "help"] {
            assert!(out.contains(cmd), "missing subcommand {cmd:?}:\n{out}");
        }
    }
}
