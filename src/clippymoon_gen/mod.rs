mod render;
mod types;

use image::RgbaImage;
use rand::Rng;
use rand_mt::Mt19937GenRand32;
use std::collections::HashMap;

pub use types::{ClippyMoonTraits, MoonColor, MoonExpression, MoonPhase};

/// Render one ClippyMoon frame and return the deterministic traits selected for its seed.
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

/// Derive ClippyMoon's stable identity traits from a 64-bit seed.
pub fn traits_from_seed(seed: u64) -> ClippyMoonTraits {
    let mut rng = Mt19937GenRand32::new_with_key(mt_key(seed ^ 0x6D6F_6F6E_6465_736B));

    // Full moons are a little more common because they read best in small TUIs,
    // while all eight major phases remain reachable and reproducible.
    let phase_roll = rng.gen_range(0..100);
    let phase = match phase_roll {
        0..=7 => MoonPhase::New,
        8..=18 => MoonPhase::WaxingCrescent,
        19..=29 => MoonPhase::FirstQuarter,
        30..=42 => MoonPhase::WaxingGibbous,
        43..=62 => MoonPhase::Full,
        63..=75 => MoonPhase::WaningGibbous,
        76..=86 => MoonPhase::LastQuarter,
        _ => MoonPhase::WaningCrescent,
    };

    // Pale/neutral moons dominate, while warm harvest/blood variants are rarer.
    let color_roll = rng.gen_range(0..100);
    let color = match color_roll {
        0..=31 => MoonColor::PaleIvory,
        32..=43 => MoonColor::Silver,
        44..=62 => MoonColor::WarmYellow,
        63..=76 => MoonColor::HarvestOrange,
        77..=86 => MoonColor::Amber,
        87..=93 => MoonColor::Copper,
        _ => MoonColor::BloodRed,
    };

    let expression = match rng.gen_range(0..100) {
        0..=59 => MoonExpression::SoftSmile,
        60..=84 => MoonExpression::TinySmile,
        _ => MoonExpression::Cheeky,
    };

    ClippyMoonTraits {
        phase,
        color,
        expression,
        crater_count: rng.gen_range(5..=10),
        star_count: rng.gen_range(3..=8),
        blush: !matches!(phase, MoonPhase::New) && rng.gen_bool(0.72),
    }
}

fn mt_key(seed: u64) -> Vec<u32> {
    if seed >> 32 == 0 {
        vec![seed as u32]
    } else {
        vec![seed as u32, (seed >> 32) as u32]
    }
}

#[cfg(test)]
mod tests {
    use super::{create_character, traits_from_seed};

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
    fn seed_space_reaches_every_phase_and_color_family() {
        let mut phases = std::collections::BTreeSet::new();
        let mut colors = std::collections::BTreeSet::new();
        for seed in 0..10_000_u64 {
            let traits = traits_from_seed(seed);
            phases.insert(traits.phase.name());
            colors.insert(traits.color.name());
            if phases.len() == 8 && colors.len() == 7 {
                break;
            }
        }
        assert_eq!(
            phases.len(),
            8,
            "all eight major moon phases should be reachable"
        );
        assert_eq!(
            colors.len(),
            7,
            "all configured Earth-visible color moods should be reachable"
        );
    }
}
