use crate::color::is_light;
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

pub fn user_message_style_for(terminal_bg: Option<(u8, u8, u8)>) -> Style {
    Style::default()
        .bg(user_message_bg_color(terminal_bg))
        .fg(user_message_fg_color(terminal_bg))
}

pub fn proposed_plan_style_for(terminal_bg: Option<(u8, u8, u8)>) -> Style {
    Style::default()
        .bg(user_message_bg_color(terminal_bg))
        .fg(user_message_fg_color(terminal_bg))
}

pub fn user_message_bg(terminal_bg: (u8, u8, u8)) -> Color {
    user_message_bg_color(Some(terminal_bg))
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
