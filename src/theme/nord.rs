use ratatui::style::Color;
use ratatui::widgets::BorderType;

use super::{Palette, ThemeDef};

pub const THEME: ThemeDef = ThemeDef {
    id: "nord",
    label: "nord",
    description: "Cool arctic blue-gray with restrained, readable accents.",
    palette: Palette {
        header_fg: Color::Rgb(136, 192, 208),
        border_fg: Color::Rgb(76, 86, 106),
        border_type: BorderType::Plain,
        title_fg: Color::Rgb(136, 192, 208),
        key_fg: Color::Rgb(129, 161, 193),
        primary_fg: Color::Rgb(216, 222, 233),
        secondary_fg: Color::Rgb(143, 188, 187),
        muted_fg: Color::Rgb(76, 86, 106),
        success_fg: Color::Rgb(163, 190, 140),
        warning_fg: Color::Rgb(235, 203, 139),
        danger_fg: Color::Rgb(191, 97, 106),
        info_fg: Color::Rgb(136, 192, 208),
        selection_bg: Color::Rgb(67, 76, 94),
        selection_fg: Color::Rgb(216, 222, 233),
        toast_bg: Color::Rgb(136, 192, 208),
        toast_fg: Color::Rgb(46, 52, 64),
        modal_bg: Color::Rgb(46, 52, 64),
        modal_fg: Color::Rgb(216, 222, 233),
    },
};
