use ratatui::style::Color;
use ratatui::widgets::BorderType;

use super::{Palette, ThemeDef};

pub const THEME: ThemeDef = ThemeDef {
    id: "paper",
    label: "paper",
    description: "Warm paper background with dark ink, muted print accents, and thick borders.",
    palette: Palette {
        background_bg: Color::Rgb(246, 240, 218),
        header_fg: Color::Rgb(49, 46, 40),
        border_fg: Color::Rgb(107, 75, 55),
        border_type: BorderType::Thick,
        title_fg: Color::Rgb(128, 53, 45),
        key_fg: Color::Rgb(0, 95, 90),
        primary_fg: Color::Rgb(49, 46, 40),
        secondary_fg: Color::Rgb(45, 92, 113),
        muted_fg: Color::Rgb(117, 110, 96),
        success_fg: Color::Rgb(53, 105, 74),
        warning_fg: Color::Rgb(164, 104, 29),
        danger_fg: Color::Rgb(166, 52, 43),
        info_fg: Color::Rgb(45, 92, 113),
        selection_bg: Color::Rgb(128, 53, 45),
        selection_fg: Color::Rgb(246, 240, 218),
        toast_bg: Color::Rgb(49, 46, 40),
        toast_fg: Color::Rgb(246, 240, 218),
        modal_bg: Color::Rgb(232, 222, 194),
        modal_fg: Color::Rgb(49, 46, 40),
    },
};
