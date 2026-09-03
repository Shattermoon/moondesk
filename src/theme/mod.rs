mod concise;
mod dracula;
mod gruvbox;
mod matrix;
mod neon;
mod paper;
mod tokyo_night;

use ratatui::style::Color;
use ratatui::widgets::BorderType;

#[derive(Clone, Copy)]
pub struct Palette {
    pub background_bg: Color,
    pub header_fg: Color,
    pub border_fg: Color,
    pub border_type: BorderType,
    pub title_fg: Color,
    pub key_fg: Color,
    pub primary_fg: Color,
    pub secondary_fg: Color,
    pub muted_fg: Color,
    pub success_fg: Color,
    pub warning_fg: Color,
    pub danger_fg: Color,
    pub info_fg: Color,
    pub selection_bg: Color,
    pub selection_fg: Color,
    pub toast_bg: Color,
    pub toast_fg: Color,
    pub modal_bg: Color,
    pub modal_fg: Color,
}

#[derive(Clone, Copy)]
pub struct ThemeDef {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub palette: Palette,
}

pub const DEFAULT_THEME_ID: &str = concise::THEME.id;

const THEMES: [ThemeDef; 7] = [
    concise::THEME,
    neon::THEME,
    tokyo_night::THEME,
    dracula::THEME,
    gruvbox::THEME,
    matrix::THEME,
    paper::THEME,
];

pub fn all() -> &'static [ThemeDef] {
    &THEMES
}

pub fn get(id: &str) -> Option<&'static ThemeDef> {
    THEMES.iter().find(|theme| theme.id == id)
}

pub fn resolve(id: &str) -> &'static ThemeDef {
    get(id).unwrap_or(&THEMES[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn registered_themes_have_unique_ids_and_resolve() {
        let mut ids = HashSet::new();
        for theme in all() {
            assert!(!theme.id.is_empty());
            assert!(!theme.label.is_empty());
            assert!(!theme.description.is_empty());
            assert!(ids.insert(theme.id), "duplicate theme id: {}", theme.id);
            assert_eq!(resolve(theme.id).id, theme.id);
        }
    }

    #[test]
    fn default_theme_is_registered() {
        assert_eq!(resolve(DEFAULT_THEME_ID).id, DEFAULT_THEME_ID);
    }

    #[test]
    fn unknown_theme_falls_back_to_default() {
        assert_eq!(resolve("not-a-theme").id, DEFAULT_THEME_ID);
    }

    #[test]
    fn paper_is_the_only_full_background_theme_and_stays_last() {
        let themes = all();
        let paper = themes.last().expect("paper theme is registered last");
        assert_eq!(paper.id, "paper");
        assert_ne!(paper.palette.background_bg, Color::Reset);
        assert_eq!(paper.palette.border_type, BorderType::Thick);
        assert!(
            themes[..themes.len() - 1]
                .iter()
                .all(|theme| theme.palette.background_bg == Color::Reset)
        );
    }
}
