use image::Rgba;

/// RGBA pixel color used by the ClippyMoon renderer.
pub type Color = Rgba<u8>;

/// Construct an RGBA color for ClippyMoon palettes and drawing helpers.
pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
    Rgba([r, g, b, a])
}

/// Bright lunar phase shapes supported by ClippyMoon.
///
/// New and crescent phases are intentionally not representable: at terminal scale
/// they make most of the mascot read as a dark disc rather than a friendly moon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoonPhase {
    FirstQuarter,
    WaxingGibbous,
    Full,
    WaningGibbous,
    LastQuarter,
}

impl MoonPhase {
    /// Stable snake_case name used in CLI output and generated trait metadata.
    pub const fn name(self) -> &'static str {
        match self {
            Self::FirstQuarter => "first_quarter",
            Self::WaxingGibbous => "waxing_gibbous",
            Self::Full => "full",
            Self::WaningGibbous => "waning_gibbous",
            Self::LastQuarter => "last_quarter",
        }
    }

    /// Approximate synodic phase angle in radians. 0 = new moon, PI = full moon.
    pub const fn angle(self) -> f32 {
        use std::f32::consts::{FRAC_PI_2, FRAC_PI_4, PI};
        match self {
            Self::FirstQuarter => FRAC_PI_2,
            Self::WaxingGibbous => PI - FRAC_PI_4,
            Self::Full => PI,
            Self::WaningGibbous => PI + FRAC_PI_4,
            Self::LastQuarter => PI + FRAC_PI_2,
        }
    }

    /// Return whether this phase belongs to the waxing half of the lunar cycle.
    pub const fn is_waxing(self) -> bool {
        matches!(self, Self::FirstQuarter | Self::WaxingGibbous)
    }
}

/// Bright, deliberately limited color families used by curated ClippyMoon identities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoonColor {
    PaleIvory,
    Silver,
    WarmYellow,
    HarvestOrange,
    CoralRed,
}

impl MoonColor {
    #[cfg(test)]
    pub const ALL: [Self; 5] = [
        Self::PaleIvory,
        Self::Silver,
        Self::WarmYellow,
        Self::HarvestOrange,
        Self::CoralRed,
    ];

    /// Stable snake_case name used in CLI output and generated trait metadata.
    pub const fn name(self) -> &'static str {
        match self {
            Self::PaleIvory => "pale_ivory",
            Self::Silver => "silver",
            Self::WarmYellow => "warm_yellow",
            Self::HarvestOrange => "harvest_orange",
            Self::CoralRed => "coral_red",
        }
    }

    /// Hand-tuned palette for clean, readable pixel art.
    ///
    /// In particular, `shadow` is intentionally a mid-tone of the same hue rather
    /// than a near-black fallback. Phase contrast should read as lighting, not as
    /// a mostly black mascot.
    pub const fn palette(self) -> MoonPalette {
        match self {
            Self::PaleIvory => MoonPalette {
                lit: rgba(244, 230, 188, 255),
                highlight: rgba(255, 247, 216, 255),
                shade: rgba(214, 194, 151, 255),
                shadow: rgba(174, 163, 138, 255),
                shadow_soft: rgba(196, 184, 156, 255),
                crater: rgba(179, 159, 122, 255),
                crater_highlight: rgba(231, 216, 177, 255),
                rim: rgba(194, 177, 145, 255),
                star: rgba(255, 239, 193, 255),
                blush: rgba(226, 139, 120, 255),
            },
            Self::Silver => MoonPalette {
                lit: rgba(216, 224, 232, 255),
                highlight: rgba(243, 248, 250, 255),
                shade: rgba(176, 190, 204, 255),
                shadow: rgba(139, 156, 174, 255),
                shadow_soft: rgba(164, 180, 195, 255),
                crater: rgba(148, 166, 181, 255),
                crater_highlight: rgba(205, 216, 224, 255),
                rim: rgba(160, 176, 190, 255),
                star: rgba(226, 240, 248, 255),
                blush: rgba(216, 142, 139, 255),
            },
            Self::WarmYellow => MoonPalette {
                lit: rgba(246, 202, 82, 255),
                highlight: rgba(255, 230, 132, 255),
                shade: rgba(218, 163, 55, 255),
                shadow: rgba(178, 137, 66, 255),
                shadow_soft: rgba(205, 160, 75, 255),
                crater: rgba(188, 137, 50, 255),
                crater_highlight: rgba(235, 190, 80, 255),
                rim: rgba(196, 149, 66, 255),
                star: rgba(255, 220, 119, 255),
                blush: rgba(222, 113, 75, 255),
            },
            Self::HarvestOrange => MoonPalette {
                lit: rgba(242, 143, 62, 255),
                highlight: rgba(255, 178, 91, 255),
                shade: rgba(213, 101, 47, 255),
                shadow: rgba(177, 84, 52, 255),
                shadow_soft: rgba(204, 105, 58, 255),
                crater: rgba(187, 82, 43, 255),
                crater_highlight: rgba(229, 128, 61, 255),
                rim: rgba(196, 96, 51, 255),
                star: rgba(255, 190, 101, 255),
                blush: rgba(202, 67, 61, 255),
            },
            Self::CoralRed => MoonPalette {
                lit: rgba(239, 101, 88, 255),
                highlight: rgba(255, 137, 115, 255),
                shade: rgba(216, 76, 70, 255),
                shadow: rgba(179, 84, 83, 255),
                shadow_soft: rgba(204, 101, 94, 255),
                crater: rgba(194, 70, 68, 255),
                crater_highlight: rgba(231, 96, 81, 255),
                rim: rgba(207, 90, 81, 255),
                star: rgba(255, 165, 132, 255),
                blush: rgba(158, 57, 66, 255),
            },
        }
    }
}

/// Small facial-expression variants used by the ClippyMoon mascot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoonExpression {
    SoftSmile,
    TinySmile,
    Cheeky,
}

impl MoonExpression {
    pub const fn name(self) -> &'static str {
        match self {
            Self::SoftSmile => "soft_smile",
            Self::TinySmile => "tiny_smile",
            Self::Cheeky => "cheeky",
        }
    }
}

/// Seed-selected identity traits that remain stable across every animation frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClippyMoonTraits {
    pub phase: MoonPhase,
    pub color: MoonColor,
    pub expression: MoonExpression,
    pub crater_count: u8,
    pub star_count: u8,
    pub blush: bool,
}

impl ClippyMoonTraits {
    /// Convert stable traits to printable string metadata without animation-only state.
    pub fn to_map(self) -> std::collections::HashMap<String, String> {
        let mut traits = std::collections::HashMap::new();
        traits.insert("phase".to_string(), self.phase.name().to_string());
        traits.insert("color".to_string(), self.color.name().to_string());
        traits.insert("expression".to_string(), self.expression.name().to_string());
        traits.insert("crater_count".to_string(), self.crater_count.to_string());
        traits.insert("star_count".to_string(), self.star_count.to_string());
        traits.insert("blush".to_string(), self.blush.to_string());
        traits
    }
}

/// Complete renderer palette for one curated moon color family.
#[derive(Clone, Copy)]
pub struct MoonPalette {
    pub lit: Color,
    pub highlight: Color,
    pub shade: Color,
    pub shadow: Color,
    pub shadow_soft: Color,
    pub crater: Color,
    pub crater_highlight: Color,
    pub rim: Color,
    pub star: Color,
    pub blush: Color,
}

#[cfg(test)]
mod tests {
    use super::MoonColor;

    fn luminance(color: image::Rgba<u8>) -> f32 {
        0.2126 * f32::from(color[0]) + 0.7152 * f32::from(color[1]) + 0.0722 * f32::from(color[2])
    }

    #[test]
    fn every_clippymoon_palette_keeps_phase_shadows_out_of_near_black_range() {
        for color in MoonColor::ALL {
            let palette = color.palette();
            assert!(
                luminance(palette.shadow) >= 95.0,
                "{} shadow is too dark: {:?}",
                color.name(),
                palette.shadow
            );
            assert!(
                luminance(palette.shadow_soft) >= luminance(palette.shadow),
                "{} soft shadow should not be darker than the base shadow",
                color.name()
            );
        }
    }
}
