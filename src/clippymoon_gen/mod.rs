mod render;
mod types;

use image::RgbaImage;
use std::collections::HashMap;

pub use types::{ClippyMoonTraits, MoonColor, MoonExpression, MoonPhase};

/// A small hand-curated identity library. Seeds select from these known-good
/// combinations instead of independently randomizing phase/color/face traits.
///
/// This intentionally excludes new/crescent moons and muddy color families: the
/// mascot should stay bright and recognizable at terminal scale for every seed.
const CURATED_STYLES: &[ClippyMoonTraits] = &[
    ClippyMoonTraits {
        phase: MoonPhase::Full,
        color: MoonColor::PaleIvory,
        expression: MoonExpression::SoftSmile,
        crater_count: 6,
        star_count: 5,
        blush: true,
    },
    ClippyMoonTraits {
        phase: MoonPhase::WaxingGibbous,
        color: MoonColor::Silver,
        expression: MoonExpression::TinySmile,
        crater_count: 5,
        star_count: 6,
        blush: true,
    },
    ClippyMoonTraits {
        phase: MoonPhase::WaningGibbous,
        color: MoonColor::WarmYellow,
        expression: MoonExpression::SoftSmile,
        crater_count: 6,
        star_count: 4,
        blush: true,
    },
    ClippyMoonTraits {
        phase: MoonPhase::FirstQuarter,
        color: MoonColor::PaleIvory,
        expression: MoonExpression::Cheeky,
        crater_count: 5,
        star_count: 5,
        blush: true,
    },
    ClippyMoonTraits {
        phase: MoonPhase::LastQuarter,
        color: MoonColor::Silver,
        expression: MoonExpression::SoftSmile,
        crater_count: 5,
        star_count: 5,
        blush: false,
    },
    ClippyMoonTraits {
        phase: MoonPhase::Full,
        color: MoonColor::WarmYellow,
        expression: MoonExpression::TinySmile,
        crater_count: 7,
        star_count: 4,
        blush: true,
    },
    ClippyMoonTraits {
        phase: MoonPhase::WaxingGibbous,
        color: MoonColor::HarvestOrange,
        expression: MoonExpression::SoftSmile,
        crater_count: 5,
        star_count: 5,
        blush: true,
    },
    ClippyMoonTraits {
        phase: MoonPhase::WaningGibbous,
        color: MoonColor::CoralRed,
        expression: MoonExpression::TinySmile,
        crater_count: 5,
        star_count: 6,
        blush: false,
    },
    ClippyMoonTraits {
        phase: MoonPhase::Full,
        color: MoonColor::HarvestOrange,
        expression: MoonExpression::Cheeky,
        crater_count: 6,
        star_count: 5,
        blush: true,
    },
    ClippyMoonTraits {
        phase: MoonPhase::FirstQuarter,
        color: MoonColor::WarmYellow,
        expression: MoonExpression::SoftSmile,
        crater_count: 4,
        star_count: 6,
        blush: true,
    },
    ClippyMoonTraits {
        phase: MoonPhase::LastQuarter,
        color: MoonColor::HarvestOrange,
        expression: MoonExpression::TinySmile,
        crater_count: 4,
        star_count: 5,
        blush: true,
    },
    ClippyMoonTraits {
        phase: MoonPhase::Full,
        color: MoonColor::CoralRed,
        expression: MoonExpression::SoftSmile,
        crater_count: 6,
        star_count: 4,
        blush: false,
    },
    ClippyMoonTraits {
        phase: MoonPhase::WaxingGibbous,
        color: MoonColor::PaleIvory,
        expression: MoonExpression::Cheeky,
        crater_count: 5,
        star_count: 6,
        blush: true,
    },
    ClippyMoonTraits {
        phase: MoonPhase::WaningGibbous,
        color: MoonColor::Silver,
        expression: MoonExpression::SoftSmile,
        crater_count: 6,
        star_count: 5,
        blush: true,
    },
    ClippyMoonTraits {
        phase: MoonPhase::Full,
        color: MoonColor::PaleIvory,
        expression: MoonExpression::TinySmile,
        crater_count: 4,
        star_count: 6,
        blush: true,
    },
];

/// Render one ClippyMoon frame and return the deterministic curated traits selected for its seed.
///
/// Passing `None` creates a fresh random seed; callers that need reproducibility should
/// provide an explicit seed.
pub fn create_character(
    seed: Option<u64>,
    width: u32,
    height: u32,
    eye_openness: f32,
    bob_offset: i32,
    twinkle_frame: u8,
) -> (RgbaImage, HashMap<String, String>) {
    let seed = seed.unwrap_or_else(rand::random::<u64>);
    let traits = traits_from_seed(seed);
    let image = render::render_clippymoon(
        seed,
        width,
        height,
        traits,
        eye_openness,
        bob_offset,
        twinkle_frame,
    );
    (image, traits.to_map())
}

/// Select one known-good ClippyMoon identity from a 64-bit seed.
pub fn traits_from_seed(seed: u64) -> ClippyMoonTraits {
    CURATED_STYLES[pick_index(seed, 0x6D6F_6F6E_6465_736B, CURATED_STYLES.len())]
}

pub(super) fn pick_index(seed: u64, salt: u64, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (mix_seed(seed ^ salt) % len as u64) as usize
}

fn mix_seed(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use super::{CURATED_STYLES, create_character, traits_from_seed};
    use crate::clippymoon_gen::MoonColor;

    #[test]
    fn same_seed_produces_same_traits_and_sprite() {
        let a = traits_from_seed(42);
        let b = traits_from_seed(42);
        assert_eq!(a, b);

        let (image_a, traits_a) = create_character(Some(42), 40, 32, 1.0, 0, 0);
        let (image_b, traits_b) = create_character(Some(42), 40, 32, 1.0, 0, 0);
        assert_eq!(traits_a, traits_b);
        assert_eq!(image_a.as_raw(), image_b.as_raw());
    }

    #[test]
    fn animation_changes_frame_without_changing_identity() {
        let (a, traits_a) = create_character(Some(7), 40, 32, 1.0, 0, 0);
        let (b, traits_b) = create_character(Some(7), 40, 32, 0.0, 1, 3);
        assert_eq!(traits_a, traits_b);
        assert_ne!(a.as_raw(), b.as_raw());
    }

    #[test]
    fn curated_styles_use_only_supported_bright_color_families() {
        assert!(!CURATED_STYLES.is_empty());
        for style in CURATED_STYLES {
            assert!(MoonColor::ALL.contains(&style.color));
        }
    }

    #[test]
    fn seed_space_reaches_every_curated_phase_and_color_family() {
        let expected_phases = CURATED_STYLES
            .iter()
            .map(|style| style.phase.name())
            .collect::<std::collections::BTreeSet<_>>();
        let expected_colors = CURATED_STYLES
            .iter()
            .map(|style| style.color.name())
            .collect::<std::collections::BTreeSet<_>>();
        let mut phases = std::collections::BTreeSet::new();
        let mut colors = std::collections::BTreeSet::new();
        for seed in 0..20_000_u64 {
            let traits = traits_from_seed(seed);
            phases.insert(traits.phase.name());
            colors.insert(traits.color.name());
            if phases == expected_phases && colors == expected_colors {
                break;
            }
        }
        assert_eq!(phases, expected_phases);
        assert_eq!(colors, expected_colors);
    }
}
