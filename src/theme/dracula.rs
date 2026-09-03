use ratatui::style::Color;
use ratatui::widgets::BorderType;

use super::{Palette, ThemeDef};

pub const THEME: ThemeDef = ThemeDef {
    id: "dracula",
    label: "dracula",
    description: "Dark purple with vivid pink, cyan, and green accents.",
    palette: Palette {
        header_fg: Color::Rgb(189, 147, 249),
        border_fg: Color::Rgb(98, 114, 164),
        border_type: BorderType::Rounded,
        title_fg: Color::Rgb(189, 147, 249),
        key_fg: Color::Rgb(255, 121, 198),
        primary_fg: Color::Rgb(248, 248, 242),
        secondary_fg: Color::Rgb(139, 233, 253),
        muted_fg: Color::Rgb(98, 114, 164),
        success_fg: Color::Rgb(80, 250, 123),
        warning_fg: Color::Rgb(241, 250, 140),
        danger_fg: Color::Rgb(255, 85, 85),
        info_fg: Color::Rgb(139, 233, 253),
        selection_bg: Color::Rgb(68, 71, 90),
        selection_fg: Color::Rgb(248, 248, 242),
        toast_bg: Color::Rgb(189, 147, 249),
        toast_fg: Color::Rgb(40, 42, 54),
        modal_bg: Color::Rgb(40, 42, 54),
        modal_fg: Color::Rgb(248, 248, 242),
    },
};
