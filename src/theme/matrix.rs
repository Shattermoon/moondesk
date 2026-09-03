use ratatui::style::Color;
use ratatui::widgets::BorderType;

use super::{Palette, ThemeDef};

pub const THEME: ThemeDef = ThemeDef {
    id: "matrix",
    label: "matrix",
    description: "Near-black terminal look with layered green accents.",
    palette: Palette {
        header_fg: Color::Rgb(0, 255, 102),
        border_fg: Color::Rgb(0, 128, 64),
        border_type: BorderType::Double,
        title_fg: Color::Rgb(0, 255, 102),
        key_fg: Color::Rgb(51, 255, 153),
        primary_fg: Color::Rgb(170, 255, 170),
        secondary_fg: Color::Rgb(0, 204, 102),
        muted_fg: Color::Rgb(64, 128, 64),
        success_fg: Color::Rgb(0, 255, 102),
        warning_fg: Color::Rgb(204, 255, 102),
        danger_fg: Color::Rgb(255, 85, 85),
        info_fg: Color::Rgb(51, 255, 153),
        selection_bg: Color::Rgb(0, 80, 40),
        selection_fg: Color::Rgb(204, 255, 204),
        toast_bg: Color::Rgb(0, 255, 102),
        toast_fg: Color::Black,
        modal_bg: Color::Rgb(0, 20, 10),
        modal_fg: Color::Rgb(170, 255, 170),
    },
};
