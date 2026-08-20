use super::{
    pick_index,
    types::{ClippyMoonTraits, Color, MoonExpression, MoonPalette, MoonPhase, rgba},
};
use image::{Rgba, RgbaImage};

#[derive(Clone, Copy)]
struct Circle {
    x: i32,
    y: i32,
    radius: i32,
}

#[derive(Clone, Copy)]
struct Crater {
    x: i32,
    y: i32,
    radius: i32,
}

#[derive(Clone, Copy)]
struct Star {
    x: i32,
    y: i32,
    phase: u8,
}

// These layouts are deliberately hand-authored. A seed chooses a layout, but it
// does not invent arbitrary crater positions that can crowd the face or make a
// visually noisy mascot.
const CRATER_PATTERNS: &[&[(i8, i8, u8)]] = &[
    &[
        (-7, -6, 2),
        (7, -6, 1),
        (-8, 3, 1),
        (8, 5, 2),
        (-5, 8, 1),
        (8, 0, 1),
        (1, -9, 1),
    ],
    &[
        (-8, -2, 1),
        (7, -7, 2),
        (-6, 8, 1),
        (8, 2, 1),
        (-5, -8, 1),
        (6, 8, 1),
        (0, -9, 1),
    ],
    &[
        (-7, -7, 1),
        (8, -3, 2),
        (-8, 6, 2),
        (7, 7, 1),
        (-8, 0, 1),
        (5, -8, 1),
        (0, 9, 1),
    ],
    &[
        (-8, -5, 1),
        (8, -6, 1),
        (-9, 2, 2),
        (9, 4, 1),
        (-6, 8, 1),
        (7, 8, 2),
        (2, -9, 1),
    ],
];

// x/y are percentages of the full frame. This keeps the sparse star layouts
// balanced at different render sizes while preserving the same authored shape.
const STAR_PATTERNS: &[&[(u8, u8, u8)]] = &[
    &[
        (8, 22, 0),
        (88, 18, 2),
        (11, 76, 4),
        (91, 68, 1),
        (50, 7, 3),
        (48, 92, 5),
    ],
    &[
        (13, 12, 1),
        (90, 28, 4),
        (8, 63, 2),
        (83, 82, 5),
        (54, 8, 0),
        (45, 91, 3),
    ],
    &[
        (7, 34, 3),
        (92, 14, 0),
        (15, 86, 5),
        (89, 62, 2),
        (40, 7, 4),
        (61, 91, 1),
    ],
    &[
        (11, 16, 5),
        (86, 9, 2),
        (7, 72, 0),
        (93, 79, 4),
        (57, 6, 1),
        (38, 93, 3),
    ],
];

/// Render a single transparent-background ClippyMoon animation frame.
///
/// Identity comes from a curated trait preset and a curated crater/star layout.
/// Animation-only inputs control blinking, one-pixel bobbing, and star twinkling.
pub fn render_clippymoon(
    seed: u64,
    width: u32,
    height: u32,
    traits: ClippyMoonTraits,
    eye_openness: f32,
    bob_offset: i32,
    twinkle_frame: u8,
) -> RgbaImage {
    let mut image = RgbaImage::from_pixel(width, height, rgba(0, 0, 0, 0));
    if width < 16 || height < 16 {
        return image;
    }

    let radius = ((height as i32 - 8) / 2)
        .min((width as i32 - 14) / 2)
        .clamp(5, 12);
    let cx = width as i32 / 2;
    let cy = height as i32 / 2 + bob_offset.clamp(-1, 1);
    let palette = traits.color.palette();

    let craters = curated_craters(seed, traits.crater_count, radius);
    let stars = curated_stars(
        seed,
        traits.star_count,
        width as i32,
        height as i32,
        cx,
        height as i32 / 2,
        radius,
    );

    draw_stars(&mut image, &stars, palette, twinkle_frame);
    draw_moon_disc(&mut image, cx, cy, radius, traits.phase, palette);
    draw_craters(&mut image, cx, cy, radius, traits.phase, palette, &craters);
    draw_face(
        &mut image,
        Circle {
            x: cx,
            y: cy,
            radius,
        },
        traits.expression,
        traits.blush,
        palette,
        eye_openness,
    );

    image
}

fn draw_moon_disc(
    image: &mut RgbaImage,
    cx: i32,
    cy: i32,
    radius: i32,
    phase: MoonPhase,
    palette: MoonPalette,
) {
    let r2 = radius * radius;
    let inner_r = radius - 1;
    let inner_r2 = inner_r * inner_r;

    for dy in -radius..=radius {
        let y = cy + dy;
        if y < 0 || y >= image.height() as i32 {
            continue;
        }
        for dx in -radius..=radius {
            let x = cx + dx;
            if x < 0 || x >= image.width() as i32 {
                continue;
            }
            let d2 = dx * dx + dy * dy;
            if d2 > r2 {
                continue;
            }

            let lit = phase_is_lit(phase, dx, dy, radius);
            let edge = d2 > inner_r2;
            let mut color = if lit { palette.lit } else { palette.shadow };

            if edge {
                color = if lit {
                    palette.rim
                } else {
                    palette.shadow_soft
                };
            } else if lit {
                // Keep volume subtle and clean. The mascot is pixel art, not a
                // noisy simulated rock texture.
                if dx + dy < -radius / 2 {
                    color = blend(color, palette.highlight, 0.24);
                } else if dx + dy > radius / 2 {
                    color = blend(color, palette.shade, 0.20);
                }
            } else if dx + dy < -radius / 2 {
                color = blend(color, palette.shadow_soft, 0.25);
            }

            put(image, x, y, color);
        }
    }

    // Sparse halo pixels use the moon's own mid-tone rather than a near-black
    // neutral, so they add depth without making the mascot look muddy.
    let halo = blend(palette.star, palette.shadow_soft, 0.45);
    for &(dx, dy) in &[
        (0, -radius - 2),
        (radius + 2, 0),
        (0, radius + 2),
        (-radius - 2, 0),
        (radius, -radius + 1),
        (-radius, radius - 1),
    ] {
        put(image, cx + dx, cy + dy, halo);
    }
}

fn phase_is_lit(phase: MoonPhase, dx: i32, dy: i32, radius: i32) -> bool {
    if matches!(phase, MoonPhase::Full) {
        return true;
    }

    let y = dy as f32 / radius as f32;
    let half_width = radius as f32 * (1.0 - y * y).max(0.0).sqrt();
    let x = dx as f32;
    let angle = phase.angle();

    if phase.is_waxing() {
        let boundary = half_width * angle.cos();
        x >= boundary
    } else {
        let waning_angle = std::f32::consts::TAU - angle;
        let boundary = -half_width * waning_angle.cos();
        x <= boundary
    }
}

fn curated_craters(seed: u64, count: u8, radius: i32) -> Vec<Crater> {
    let pattern = CRATER_PATTERNS[pick_index(seed, 0xC11F_C8A7_E250_2026, CRATER_PATTERNS.len())];
    let limit = (radius - 2).max(2);
    let mut craters = Vec::with_capacity(count as usize);

    for &(base_x, base_y, base_radius) in pattern.iter().take(count as usize) {
        let x = scale_layout_coordinate(i32::from(base_x), radius).clamp(-limit, limit);
        let y = scale_layout_coordinate(i32::from(base_y), radius).clamp(-limit, limit);
        let crater_radius = if base_radius > 1 && radius >= 9 { 2 } else { 1 };

        // Keep the eyes/mouth area visually clean even at smaller render sizes.
        let face_half_width = (radius / 2).max(2);
        if x.abs() <= face_half_width && (-radius / 4..=radius / 2).contains(&y) {
            continue;
        }
        if x * x + y * y > limit * limit {
            continue;
        }
        craters.push(Crater {
            x,
            y,
            radius: crater_radius,
        });
    }
    craters
}

fn scale_layout_coordinate(value: i32, radius: i32) -> i32 {
    // Layouts are authored against the normal radius-12 TUI sprite.
    (value * radius).div_euclid(12)
}

fn draw_craters(
    image: &mut RgbaImage,
    cx: i32,
    cy: i32,
    moon_radius: i32,
    phase: MoonPhase,
    palette: MoonPalette,
    craters: &[Crater],
) {
    for crater in craters {
        let center_x = cx + crater.x;
        let center_y = cy + crater.y;
        let lit = phase_is_lit(phase, crater.x, crater.y, moon_radius);
        let base = if lit {
            palette.crater
        } else {
            blend(palette.shadow, palette.crater, 0.34)
        };
        let hi = if lit {
            palette.crater_highlight
        } else {
            palette.shadow_soft
        };
        fill_disc_clipped_to_moon(
            image,
            Circle {
                x: center_x,
                y: center_y,
                radius: crater.radius,
            },
            Circle {
                x: cx,
                y: cy,
                radius: moon_radius,
            },
            base,
        );
        put_if_inside_moon(
            image,
            center_x - 1,
            center_y - crater.radius,
            cx,
            cy,
            moon_radius,
            hi,
        );
    }
}

fn draw_face(
    image: &mut RgbaImage,
    moon: Circle,
    expression: MoonExpression,
    blush: bool,
    palette: MoonPalette,
    eye_openness: f32,
) {
    let Circle {
        x: cx,
        y: cy,
        radius,
    } = moon;

    // This is intentionally the only near-black family in the mascot: a small
    // amount of deep navy gives the face enough contrast without darkening the moon.
    let face_dark = rgba(31, 39, 52, 255);
    let eye_glint = rgba(244, 249, 251, 255);
    let left_eye_x = cx - 4;
    let right_eye_x = cx + 4;
    let eye_y = cy;

    draw_eye(
        image,
        left_eye_x,
        eye_y,
        moon,
        eye_openness,
        face_dark,
        eye_glint,
    );
    draw_eye(
        image,
        right_eye_x,
        eye_y,
        moon,
        eye_openness,
        face_dark,
        eye_glint,
    );

    if blush && eye_openness > 0.2 {
        put_if_inside_moon(image, cx - 7, cy + 3, cx, cy, radius, palette.blush);
        put_if_inside_moon(image, cx - 6, cy + 3, cx, cy, radius, palette.blush);
        put_if_inside_moon(image, cx + 6, cy + 3, cx, cy, radius, palette.blush);
        put_if_inside_moon(image, cx + 7, cy + 3, cx, cy, radius, palette.blush);
    }

    let mut put_face_pixel = |x: i32, y: i32| {
        put_if_inside_moon(image, x, y, cx, cy, radius, face_dark);
    };
    match expression {
        MoonExpression::SoftSmile => {
            put_face_pixel(cx - 2, cy + 3);
            put_face_pixel(cx - 1, cy + 4);
            put_face_pixel(cx, cy + 4);
            put_face_pixel(cx + 1, cy + 4);
            put_face_pixel(cx + 2, cy + 3);
        }
        MoonExpression::TinySmile => {
            put_face_pixel(cx - 1, cy + 4);
            put_face_pixel(cx, cy + 5);
            put_face_pixel(cx + 1, cy + 4);
        }
        MoonExpression::Cheeky => {
            put_face_pixel(cx - 2, cy + 4);
            put_face_pixel(cx - 1, cy + 5);
            put_face_pixel(cx, cy + 5);
            put_face_pixel(cx + 1, cy + 5);
            put_face_pixel(cx + 2, cy + 4);
            put_face_pixel(cx + 1, cy + 6);
        }
    }
}

fn draw_eye(
    image: &mut RgbaImage,
    eye_cx: i32,
    eye_cy: i32,
    moon: Circle,
    openness: f32,
    dark: Color,
    glint: Color,
) {
    let Circle {
        x: moon_cx,
        y: moon_cy,
        radius: moon_radius,
    } = moon;
    let mut put_eye_pixel = |x: i32, y: i32, color: Color| {
        put_if_inside_moon(image, x, y, moon_cx, moon_cy, moon_radius, color);
    };
    let openness = openness.clamp(0.0, 1.0);
    if openness <= 0.15 {
        for x in -1..=1 {
            put_eye_pixel(eye_cx + x, eye_cy + 1, dark);
        }
        return;
    }
    if openness < 0.65 {
        for &(x, y) in &[
            (eye_cx - 1, eye_cy),
            (eye_cx, eye_cy),
            (eye_cx + 1, eye_cy),
            (eye_cx, eye_cy + 1),
        ] {
            put_eye_pixel(x, y, dark);
        }
        return;
    }

    // Open eyes are compact 3x3 blocks; the old 3x4 eyes consumed too much of
    // the small sprite and amplified the amount of near-black pixels.
    for y in -1..=1 {
        for x in -1..=1 {
            put_eye_pixel(eye_cx + x, eye_cy + y, dark);
        }
    }
    put_eye_pixel(eye_cx - 1, eye_cy - 1, glint);
    put_eye_pixel(eye_cx, eye_cy - 1, glint);
}

fn curated_stars(
    seed: u64,
    count: u8,
    width: i32,
    height: i32,
    moon_cx: i32,
    moon_cy: i32,
    moon_radius: i32,
) -> Vec<Star> {
    let pattern = STAR_PATTERNS[pick_index(seed, 0x57A2_5A11_C11F_2026, STAR_PATTERNS.len())];
    let mut stars = Vec::with_capacity(count as usize);

    for &(x_percent, y_percent, phase) in pattern.iter().take(count as usize) {
        let x = (width * i32::from(x_percent) / 100).clamp(1, width.saturating_sub(2));
        let y = (height * i32::from(y_percent) / 100).clamp(1, height.saturating_sub(2));
        let dx = x - moon_cx;
        let dy = y - moon_cy;
        if dx * dx + dy * dy <= (moon_radius + 3).pow(2) {
            continue;
        }
        stars.push(Star { x, y, phase });
    }
    stars
}

fn draw_stars(image: &mut RgbaImage, stars: &[Star], palette: MoonPalette, twinkle_frame: u8) {
    let dim = blend(palette.star, palette.shadow_soft, 0.42);
    for star in stars {
        let age = (twinkle_frame % 6 + 6 - star.phase) % 6;
        match age {
            0 => {
                put(image, star.x, star.y, palette.star);
                put(image, star.x - 1, star.y, dim);
                put(image, star.x + 1, star.y, dim);
                put(image, star.x, star.y - 1, dim);
                put(image, star.x, star.y + 1, dim);
            }
            1 | 5 => put(image, star.x, star.y, palette.star),
            2 | 4 => put(image, star.x, star.y, dim),
            _ => {}
        }
    }
}

fn fill_disc_clipped_to_moon(image: &mut RgbaImage, disc: Circle, moon: Circle, color: Color) {
    let Circle {
        x: cx,
        y: cy,
        radius,
    } = disc;
    let Circle {
        x: moon_cx,
        y: moon_cy,
        radius: moon_radius,
    } = moon;
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            if dx * dx + dy * dy <= radius * radius {
                put_if_inside_moon(
                    image,
                    cx + dx,
                    cy + dy,
                    moon_cx,
                    moon_cy,
                    moon_radius,
                    color,
                );
            }
        }
    }
}

fn put_if_inside_moon(
    image: &mut RgbaImage,
    x: i32,
    y: i32,
    moon_cx: i32,
    moon_cy: i32,
    moon_radius: i32,
    color: Color,
) {
    let dx = x - moon_cx;
    let dy = y - moon_cy;
    if dx * dx + dy * dy <= moon_radius * moon_radius {
        put(image, x, y, color);
    }
}

fn put(image: &mut RgbaImage, x: i32, y: i32, color: Color) {
    if x >= 0 && y >= 0 && x < image.width() as i32 && y < image.height() as i32 {
        image.put_pixel(x as u32, y as u32, color);
    }
}

fn blend(a: Color, b: Color, amount_b: f32) -> Color {
    let t = amount_b.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| ((x as f32 * (1.0 - t)) + (y as f32 * t)).round() as u8;
    Rgba([mix(a[0], b[0]), mix(a[1], b[1]), mix(a[2], b[2]), 255])
}

#[cfg(test)]
mod tests {
    use super::{
        CRATER_PATTERNS, Circle, STAR_PATTERNS, Star, draw_face, draw_stars, phase_is_lit,
        render_clippymoon,
    };
    use crate::clippymoon_gen::{
        CURATED_STYLES, traits_from_seed,
        types::{MoonColor, MoonExpression, MoonPhase},
    };
    use image::RgbaImage;

    fn lit_pixels(phase: MoonPhase) -> usize {
        let radius = 10;
        let mut count = 0;
        for y in -radius..=radius {
            for x in -radius..=radius {
                if x * x + y * y <= radius * radius && phase_is_lit(phase, x, y, radius) {
                    count += 1;
                }
            }
        }
        count
    }

    fn luminance(pixel: &image::Rgba<u8>) -> f32 {
        0.2126 * f32::from(pixel[0]) + 0.7152 * f32::from(pixel[1]) + 0.0722 * f32::from(pixel[2])
    }

    #[test]
    fn phase_mask_orders_supported_bright_phases_sensibly() {
        let quarter = lit_pixels(MoonPhase::FirstQuarter);
        let gibbous = lit_pixels(MoonPhase::WaxingGibbous);
        let full = lit_pixels(MoonPhase::Full);
        assert!(quarter < gibbous);
        assert!(gibbous < full);
    }

    #[test]
    fn face_pixels_stay_inside_minimum_moon_disc() {
        let mut image = RgbaImage::new(16, 16);
        let cx = 8;
        let cy = 8;
        let radius = 5;
        draw_face(
            &mut image,
            Circle {
                x: cx,
                y: cy,
                radius,
            },
            MoonExpression::Cheeky,
            true,
            MoonColor::PaleIvory.palette(),
            1.0,
        );

        for (x, y, pixel) in image.enumerate_pixels() {
            if pixel[3] == 0 {
                continue;
            }
            let dx = x as i32 - cx;
            let dy = y as i32 - cy;
            assert!(
                dx * dx + dy * dy <= radius * radius,
                "face pixel ({x}, {y}) escaped the moon disc"
            );
        }
    }

    #[test]
    fn twinkle_frame_accepts_entire_u8_range() {
        let mut image = RgbaImage::new(8, 8);
        let stars = [Star {
            x: 4,
            y: 4,
            phase: 5,
        }];
        draw_stars(&mut image, &stars, MoonColor::PaleIvory.palette(), u8::MAX);
    }

    #[test]
    fn authored_layouts_cover_every_curated_identity_count() {
        let max_craters = CURATED_STYLES
            .iter()
            .map(|style| usize::from(style.crater_count))
            .max()
            .unwrap_or(0);
        let max_stars = CURATED_STYLES
            .iter()
            .map(|style| usize::from(style.star_count))
            .max()
            .unwrap_or(0);

        assert!(
            CRATER_PATTERNS
                .iter()
                .all(|pattern| pattern.len() >= max_craters),
            "every authored crater pattern must cover the largest curated crater count"
        );
        assert!(
            STAR_PATTERNS
                .iter()
                .all(|pattern| pattern.len() >= max_stars),
            "every authored star pattern must cover the largest curated star count"
        );
    }

    #[test]
    fn curated_mascots_keep_near_black_pixels_to_small_face_details() {
        for seed in 0..512_u64 {
            let traits = traits_from_seed(seed);
            let image = render_clippymoon(seed, 40, 32, traits, 1.0, 0, 0);
            let mut opaque = 0usize;
            let mut very_dark = 0usize;
            for pixel in image.pixels().filter(|pixel| pixel[3] > 0) {
                opaque += 1;
                if luminance(pixel) < 70.0 {
                    very_dark += 1;
                }
            }
            assert!(opaque > 0);
            assert!(
                very_dark * 100 <= opaque * 12,
                "seed {seed:#x} produced too many near-black pixels: {very_dark}/{opaque}"
            );
        }
    }
}
