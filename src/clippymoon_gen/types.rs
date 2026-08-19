use image::Rgba;

/// RGBA pixel color used by the procedural renderer.
pub type Color = Rgba<u8>;

/// Construct an RGBA color for ClippyMoon palettes and drawing helpers.
pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
    Rgba([r, g, b, a])
}

/// One of the eight major lunar phases used by the procedural phase mask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoonPhase {
    New,
    WaxingCrescent,
    FirstQuarter,
    WaxingGibbous,
    Full,
    WaningGibbous,
    LastQuarter,
    WaningCrescent,
}

impl MoonPhase {
    /// Stable snake_case name used in CLI output and generated trait metadata.
    pub const fn name(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::WaxingCrescent => "waxing_crescent",
            Self::FirstQuarter => "first_quarter",
            Self::WaxingGibbous => "waxing_gibbous",
            Self::Full => "full",
            Self::WaningGibbous => "waning_gibbous",
            Self::LastQuarter => "last_quarter",
            Self::WaningCrescent => "waning_crescent",
        }
    }

    /// Approximate synodic phase angle in radians. 0 = new moon, PI = full moon.
    pub const fn angle(self) -> f32 {
        use std::f32::consts::{FRAC_PI_2, FRAC_PI_4, PI};
        match self {
            Self::New => 0.0,
            Self::WaxingCrescent => FRAC_PI_4,
            Self::FirstQuarter => FRAC_PI_2,
            Self::WaxingGibbous => PI - FRAC_PI_4,
            Self::Full => PI,
            Self::WaningGibbous => PI + FRAC_PI_4,
            Self::LastQuarter => PI + FRAC_PI_2,
            Self::WaningCrescent => 2.0 * PI - FRAC_PI_4,
        }
    }

    /// Return whether this phase belongs to the waxing half of the lunar cycle.
    pub const fn is_waxing(self) -> bool {
        matches!(
            self,
            Self::New | Self::WaxingCrescent | Self::FirstQuarter | Self::WaxingGibbous
        )
    }
}

/// Earth-visible color mood assigned to a generated ClippyMoon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoonColor {
    PaleIvory,
    Silver,
    WarmYellow,
    HarvestOrange,
    Amber,
    Copper,
    BloodRed,
}

impl MoonColor {
    /// Stable snake_case name used in CLI output and generated trait metadata.
    pub const fn name(self) -> &'static str {
        match self {
            Self::PaleIvory => "pale_ivory",
            Self::Silver => "silver",
            Self::WarmYellow => "warm_yellow",
            Self::HarvestOrange => "harvest_orange",
            Self::Amber => "amber",
            Self::Copper => "copper",
            Self::BloodRed => "blood_red",
        }
    }

    /// Color palette used to render this moon color across light, shadow, craters, and accents.
    pub const fn palette(self) -> MoonPalette {
        match self {
            Self::PaleIvory => MoonPalette {
                lit: rgba(242, 229, 188, 255),
                highlight: rgba(255, 246, 211, 255),
                shade: rgba(199, 181, 141, 255),
                shadow: rgba(38, 43, 56, 255),
                shadow_soft: rgba(55, 59, 70, 255),
                crater: rgba(166, 151, 121, 255),
                crater_highlight: rgba(223, 208, 169, 255),
                rim: rgba(109, 103, 91, 255),
                star: rgba(255, 235, 177, 255),
                blush: rgba(223, 137, 118, 255),
            },
            Self::Silver => MoonPalette {
                lit: rgba(205, 212, 214, 255),
                highlight: rgba(235, 240, 237, 255),
                shade: rgba(158, 169, 173, 255),
                shadow: rgba(34, 42, 55, 255),
                shadow_soft: rgba(52, 62, 73, 255),
                crater: rgba(129, 143, 148, 255),
                crater_highlight: rgba(190, 201, 202, 255),
                rim: rgba(92, 105, 112, 255),
                star: rgba(216, 231, 239, 255),
                blush: rgba(203, 132, 129, 255),
            },
            Self::WarmYellow => MoonPalette {
                lit: rgba(235, 194, 91, 255),
                highlight: rgba(255, 226, 130, 255),
                shade: rgba(191, 148, 59, 255),
                shadow: rgba(50, 43, 42, 255),
                shadow_soft: rgba(68, 57, 48, 255),
                crater: rgba(164, 122, 49, 255),
                crater_highlight: rgba(220, 180, 78, 255),
                rim: rgba(118, 92, 53, 255),
                star: rgba(255, 217, 115, 255),
                blush: rgba(215, 112, 75, 255),
            },
            Self::HarvestOrange => MoonPalette {
                lit: rgba(220, 131, 56, 255),
                highlight: rgba(244, 163, 77, 255),
                shade: rgba(176, 91, 39, 255),
                shadow: rgba(58, 40, 42, 255),
                shadow_soft: rgba(80, 50, 42, 255),
                crater: rgba(145, 69, 35, 255),
                crater_highlight: rgba(205, 111, 48, 255),
                rim: rgba(105, 64, 42, 255),
                star: rgba(255, 183, 91, 255),
                blush: rgba(188, 64, 55, 255),
            },
            Self::Amber => MoonPalette {
                lit: rgba(229, 153, 78, 255),
                highlight: rgba(249, 191, 112, 255),
                shade: rgba(183, 112, 61, 255),
                shadow: rgba(56, 42, 43, 255),
                shadow_soft: rgba(75, 53, 46, 255),
                crater: rgba(151, 86, 51, 255),
                crater_highlight: rgba(213, 135, 70, 255),
                rim: rgba(108, 72, 50, 255),
                star: rgba(255, 198, 118, 255),
                blush: rgba(200, 83, 69, 255),
            },
            Self::Copper => MoonPalette {
                lit: rgba(183, 94, 59, 255),
                highlight: rgba(215, 125, 77, 255),
                shade: rgba(136, 65, 49, 255),
                shadow: rgba(51, 37, 43, 255),
                shadow_soft: rgba(68, 45, 48, 255),
                crater: rgba(111, 49, 43, 255),
                crater_highlight: rgba(169, 79, 55, 255),
                rim: rgba(91, 55, 48, 255),
                star: rgba(244, 156, 96, 255),
                blush: rgba(153, 54, 56, 255),
            },
            Self::BloodRed => MoonPalette {
                lit: rgba(151, 58, 50, 255),
                highlight: rgba(189, 74, 57, 255),
                shade: rgba(107, 40, 43, 255),
                shadow: rgba(45, 32, 42, 255),
                shadow_soft: rgba(61, 37, 44, 255),
                crater: rgba(82, 29, 36, 255),
                crater_highlight: rgba(135, 47, 44, 255),
                rim: rgba(76, 43, 45, 255),
                star: rgba(222, 116, 84, 255),
                blush: rgba(108, 28, 39, 255),
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
    /// Stable snake_case name used in CLI output and generated trait metadata.
    pub const fn name(self) -> &'static str {
        match self {
            Self::SoftSmile => "soft_smile",
            Self::TinySmile => "tiny_smile",
            Self::Cheeky => "cheeky",
        }
    }
}

/// Seed-derived identity traits that remain stable across every animation frame.
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

/// Complete renderer palette for one generated moon color mood.
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
