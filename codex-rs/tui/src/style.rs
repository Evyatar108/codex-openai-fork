use std::sync::OnceLock;

use crate::color::blend;
use crate::color::is_light;
use crate::terminal_palette::StdoutColorLevel;
use crate::terminal_palette::best_color;
use crate::terminal_palette::default_bg;
<<<<<<< HEAD
use crate::terminal_palette::rgb_color;
=======
use crate::terminal_palette::default_fg;
use crate::terminal_palette::rgb_color;
use crate::terminal_palette::stdout_color_level;
>>>>>>> f4a628f40d7802947409c89ef0519a7b57b0f161
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::style::Stylize;

const LIGHT_BG_ACCENT_RGB: (u8, u8, u8) = (0, 95, 135);
// Decorative table rules should remain visible without competing with cell content.
const TABLE_SEPARATOR_FG_ALPHA: f32 = 0.20;

pub fn user_message_style() -> Style {
    user_message_style_for(default_bg())
}

pub fn proposed_plan_style() -> Style {
    proposed_plan_style_for(default_bg())
}

<<<<<<< HEAD
/// Subtle background style for popups, pickers, and overlay surfaces.
///
/// Always uses the upstream blend-against-terminal-bg behavior, regardless of
/// the `CODEX_TUI_USER_MESSAGE_STYLE` env var. Popups should retain only the
/// faint visual-hierarchy tint upstream uses; the bold `rgb(55,55,55)` /
/// `rgb(240,240,240)` Claude-Code-style background is intended for the user
/// message body only (history cells + composer textarea).
pub fn popup_style() -> Style {
    popup_style_for(default_bg())
}

pub fn popup_style_for(terminal_bg: Option<(u8, u8, u8)>) -> Style {
=======
/// Returns a low-contrast rule style for separators within markdown tables.
pub(crate) fn table_separator_style() -> Style {
    table_separator_style_for(default_fg(), default_bg(), stdout_color_level())
}

/// Returns the shared accent style for active or selected TUI controls.
pub(crate) fn accent_style() -> Style {
    accent_style_for(default_bg())
}

/// Returns the style for a user-authored message using the provided terminal background.
pub fn user_message_style_for(terminal_bg: Option<(u8, u8, u8)>) -> Style {
>>>>>>> f4a628f40d7802947409c89ef0519a7b57b0f161
    match terminal_bg {
        Some(bg) => Style::default().bg(upstream_user_message_bg(bg)),
        None => Style::default(),
    }
}

pub fn user_message_style_for(terminal_bg: Option<(u8, u8, u8)>) -> Style {
    // SANDBOX PATCH: gate the Claude-Code-style background on the launcher's
    // `style_user_messages` config, propagated via `CODEX_TUI_USER_MESSAGE_STYLE`.
    // Default ON so existing users keep the styling that v0.125.0-copilot-api.9
    // introduced. Set `style_user_messages = false` in `~/.codex-copilot/config.toml`
    // to fall back to upstream behavior (terminal-bg-blend, suppressed entirely
    // on Windows where OSC-11 probing returns None).
    if user_message_styling_enabled() {
        Style::default()
            .bg(user_message_bg_color(terminal_bg))
            .fg(user_message_fg_color(terminal_bg))
    } else {
        match terminal_bg {
            Some(bg) => Style::default().bg(upstream_user_message_bg(bg)),
            None => Style::default(),
        }
    }
}

pub fn proposed_plan_style_for(terminal_bg: Option<(u8, u8, u8)>) -> Style {
    // Mirror user_message_style_for so the two surfaces toggle together.
    if user_message_styling_enabled() {
        Style::default()
            .bg(user_message_bg_color(terminal_bg))
            .fg(user_message_fg_color(terminal_bg))
    } else {
        match terminal_bg {
            Some(bg) => Style::default().bg(upstream_user_message_bg(bg)),
            None => Style::default(),
        }
    }
}

<<<<<<< HEAD
=======
/// Returns the shared accent style for the provided terminal background.
pub(crate) fn accent_style_for(terminal_bg: Option<(u8, u8, u8)>) -> Style {
    if terminal_bg.is_some_and(is_light) {
        Style::default().fg(best_color(LIGHT_BG_ACCENT_RGB)).bold()
    } else {
        Style::default().fg(Color::Cyan).bold()
    }
}

fn table_separator_style_for(
    terminal_fg: Option<(u8, u8, u8)>,
    terminal_bg: Option<(u8, u8, u8)>,
    color_level: StdoutColorLevel,
) -> Style {
    let (Some(fg), Some(bg)) = (terminal_fg, terminal_bg) else {
        return Style::default().dim();
    };
    let separator_rgb = blend(fg, bg, TABLE_SEPARATOR_FG_ALPHA);
    match color_level {
        StdoutColorLevel::TrueColor => Style::default().fg(rgb_color(separator_rgb)),
        StdoutColorLevel::Ansi256 => Style::default().fg(best_color(separator_rgb)),
        StdoutColorLevel::Ansi16 | StdoutColorLevel::Unknown => Style::default().dim(),
    }
}

#[allow(clippy::disallowed_methods)]
>>>>>>> f4a628f40d7802947409c89ef0519a7b57b0f161
pub fn user_message_bg(terminal_bg: (u8, u8, u8)) -> Color {
    // Preserve the patched-fork helper signature for any in-tree caller, but
    // route through the same gate so disabling the fork styling does not leave
    // this helper as a hardcoded rgb-color back-door.
    if user_message_styling_enabled() {
        user_message_bg_color(Some(terminal_bg))
    } else {
        upstream_user_message_bg(terminal_bg)
    }
}

/// Reads `CODEX_TUI_USER_MESSAGE_STYLE` once per process and caches the result.
///
/// Accepted values (case-insensitive): `on`, `1`, `true`, `yes` → enabled;
/// `off`, `0`, `false`, `no` → disabled. Anything else (or unset) → enabled.
fn user_message_styling_enabled() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| match std::env::var("CODEX_TUI_USER_MESSAGE_STYLE") {
        Ok(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false" | "no"
        ),
        Err(_) => true,
    })
}

// Match Claude Code: rgb(240,240,240) light / rgb(55,55,55) dark.
// Use rgb_color directly — best_color falls back to Color::default() on
// Unknown/Ansi16 color levels, which suppresses the background entirely.
fn user_message_bg_color(terminal_bg: Option<(u8, u8, u8)>) -> Color {
    match terminal_bg {
        Some(bg) if is_light(bg) => rgb_color((240, 240, 240)),
        _ => rgb_color((55, 55, 55)),
    }
}

fn user_message_fg_color(terminal_bg: Option<(u8, u8, u8)>) -> Color {
    // Known limitation: on Windows default_bg() returns None so we assume dark
    // terminal. Light-terminal Windows users would get white-on-light-grey.
    match terminal_bg {
        Some(bg) if is_light(bg) => rgb_color((0, 0, 0)),
        _ => rgb_color((255, 255, 255)),
    }
}

// Upstream behavior preserved verbatim for the disabled branch.
#[allow(clippy::disallowed_methods)]
fn upstream_user_message_bg(terminal_bg: (u8, u8, u8)) -> Color {
    let (top, alpha) = if is_light(terminal_bg) {
        ((0, 0, 0), 0.04)
    } else {
        ((255, 255, 255), 0.12)
    };
    best_color(blend(top, terminal_bg, alpha))
}
<<<<<<< HEAD
=======

#[allow(clippy::disallowed_methods)]
pub fn proposed_plan_bg(terminal_bg: (u8, u8, u8)) -> Color {
    user_message_bg(terminal_bg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use ratatui::style::Modifier;

    #[test]
    fn accent_style_uses_darker_cyan_on_light_backgrounds() {
        let style = accent_style_for(Some((255, 255, 255)));

        assert_eq!(style.fg, Some(best_color(LIGHT_BG_ACCENT_RGB)));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn accent_style_uses_cyan_on_dark_or_unknown_backgrounds() {
        let expected = Style::default().fg(Color::Cyan).bold();

        assert_eq!(accent_style_for(Some((0, 0, 0))), expected);
        assert_eq!(accent_style_for(/*terminal_bg*/ None), expected);
    }

    #[test]
    fn table_separator_blends_toward_dark_background() {
        let style = table_separator_style_for(
            Some((255, 255, 255)),
            Some((0, 0, 0)),
            StdoutColorLevel::TrueColor,
        );

        assert_eq!(style.fg, Some(rgb_color((51, 51, 51))));
    }

    #[test]
    fn table_separator_blends_toward_light_background() {
        let style = table_separator_style_for(
            Some((0, 0, 0)),
            Some((255, 255, 255)),
            StdoutColorLevel::TrueColor,
        );

        assert_eq!(style.fg, Some(rgb_color((204, 204, 204))));
    }

    #[test]
    fn table_separator_dims_when_palette_aware_color_is_unavailable() {
        let expected = Style::default().dim();

        assert_eq!(
            table_separator_style_for(
                Some((255, 255, 255)),
                Some((0, 0, 0)),
                StdoutColorLevel::Ansi16,
            ),
            expected
        );
        assert_eq!(
            table_separator_style_for(
                /*terminal_fg*/ None,
                Some((0, 0, 0)),
                StdoutColorLevel::TrueColor,
            ),
            expected
        );
    }
}
>>>>>>> f4a628f40d7802947409c89ef0519a7b57b0f161
