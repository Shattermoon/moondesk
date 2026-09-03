use ratatui::style::Color;
use ratatui::widgets::BorderType;

use super::{Palette, ThemeDef};

pub const THEME: ThemeDef = ThemeDef {
    id: "tokyo-night",
    label: "tokyo night",
    description: "Deep navy with cool blue, violet, and cyan accents.",
    palette: Palette {
        background_bg: Color::Reset,
        header_fg: Color::Rgb(122, 162, 247),
        border_fg: Color::Rgb(86, 95, 137),
        border_type: BorderType::Rounded,
        title_fg: Color::Rgb(122, 162, 247),
        key_fg: Color::Rgb(125, 207, 255),
        primary_fg: Color::Rgb(192, 202, 245),
        secondary_fg: Color::Rgb(187, 154, 247),
        muted_fg: Color::Rgb(86, 95, 137),
        success_fg: Color::Rgb(158, 206, 106),
        warning_fg: Color::Rgb(224, 175, 104),
        danger_fg: Color::Rgb(247, 118, 142),
        info_fg: Color::Rgb(125, 207, 255),
        selection_bg: Color::Rgb(54, 70, 120),
        selection_fg: Color::Rgb(192, 202, 245),
        toast_bg: Color::Rgb(122, 162, 247),
        toast_fg: Color::Rgb(26, 27, 38),
        modal_bg: Color::Rgb(26, 27, 38),
        modal_fg: Color::Rgb(192, 202, 245),
    },
};
