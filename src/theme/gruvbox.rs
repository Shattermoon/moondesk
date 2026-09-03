use ratatui::style::Color;
use ratatui::widgets::BorderType;

use super::{Palette, ThemeDef};

pub const THEME: ThemeDef = ThemeDef {
    id: "gruvbox",
    label: "gruvbox",
    description: "Warm retro browns with orange, yellow, and green accents.",
    palette: Palette {
        header_fg: Color::Rgb(250, 189, 47),
        border_fg: Color::Rgb(146, 131, 116),
        border_type: BorderType::Plain,
        title_fg: Color::Rgb(250, 189, 47),
        key_fg: Color::Rgb(254, 128, 25),
        primary_fg: Color::Rgb(235, 219, 178),
        secondary_fg: Color::Rgb(131, 165, 152),
        muted_fg: Color::Rgb(146, 131, 116),
        success_fg: Color::Rgb(184, 187, 38),
        warning_fg: Color::Rgb(250, 189, 47),
        danger_fg: Color::Rgb(251, 73, 52),
        info_fg: Color::Rgb(131, 165, 152),
        selection_bg: Color::Rgb(80, 73, 69),
        selection_fg: Color::Rgb(235, 219, 178),
        toast_bg: Color::Rgb(250, 189, 47),
        toast_fg: Color::Rgb(40, 40, 40),
        modal_bg: Color::Rgb(40, 40, 40),
        modal_fg: Color::Rgb(235, 219, 178),
    },
};
