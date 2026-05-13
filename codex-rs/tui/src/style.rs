use std::sync::OnceLock;

use crate::color::blend;
use crate::color::is_light;
use crate::terminal_palette::best_color;
use crate::terminal_palette::default_bg;
use crate::terminal_palette::rgb_color;
use ratatui::style::Color;
use ratatui::style::Style;

pub fn user_message_style() -> Style {
    user_message_style_for(default_bg())
}

pub fn proposed_plan_style() -> Style {
    proposed_plan_style_for(default_bg())
}

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
