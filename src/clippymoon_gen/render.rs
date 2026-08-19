use super::types::{ClippyMoonTraits, Color, MoonExpression, MoonPalette, MoonPhase, rgba};
use image::{Rgba, RgbaImage};
use rand::Rng;
use rand_mt::Mt19937GenRand32;

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

/// Render a single transparent-background ClippyMoon animation frame.
///
/// Identity comes from `traits` and `seed`; animation-only inputs control the blink,
/// one-pixel bob, and star-twinkle state without changing the moon's identity.
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

    let mut rng = Mt19937GenRand32::new_with_key(mt_key(seed ^ 0xC11F_F1E5_0A11_2026));
    let craters = generate_craters(&mut rng, traits.crater_count, radius);
    let stars = generate_stars(
        &mut rng,
        traits.star_count,
        width as i32,
        height as i32,
        cx,
        height as i32 / 2,
        radius,
    );

    draw_stars(&mut image, &stars, palette, twinkle_frame);
    draw_moon_disc(&mut image, seed, cx, cy, radius, traits.phase, palette);
    draw_craters(&mut image, cx, cy, radius, traits.phase, palette, &craters);
    draw_face(
        &mut image,
        Circle {
            x: cx,
            y: cy,
            radius,
        },
        traits.phase,
        traits.expression,
        traits.blush,
        palette,
        eye_openness,
    );

    image
}

fn draw_moon_disc(
    image: &mut RgbaImage,
    seed: u64,
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
                // A little top-left light and bottom-right shade gives the sphere volume.
                if dx + dy < -radius / 2 {
                    color = blend(color, palette.highlight, 0.30);
                } else if dx + dy > radius / 2 {
                    color = blend(color, palette.shade, 0.28);
                }
            } else if dx + dy < -radius / 2 {
                color = blend(color, palette.shadow_soft, 0.22);
            }

            // Deterministic one-pixel mottling keeps the moon organic without frame-to-frame noise.
            let noise = pixel_hash(seed, x, y) % 17;
            color = match noise {
                0 if lit => adjust(color, 9),
                1 if lit => adjust(color, -8),
                2 if !lit => adjust(color, 5),
                _ => color,
            };
            put(image, x, y, color);
        }
    }

    // Sparse outer halo pixels. They stay crisp in the TUI rather than becoming a blurry glow.
    let halo = blend(palette.star, rgba(20, 24, 34, 255), 0.58);
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
    if matches!(phase, MoonPhase::New) {
        return false;
    }
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

fn generate_craters(rng: &mut Mt19937GenRand32, count: u8, radius: i32) -> Vec<Crater> {
    let mut craters = Vec::with_capacity(count as usize);
    let safe_radius = (radius - 3).max(2);
    let mut attempts = 0;
    while craters.len() < count as usize && attempts < count as usize * 30 {
        attempts += 1;
        let x = rng.gen_range(-safe_radius..=safe_radius);
        let y = rng.gen_range(-safe_radius..=safe_radius);
        if x * x + y * y > safe_radius * safe_radius {
            continue;
        }
        // Keep the central face area comparatively clean.
        if x.abs() <= 5 && (-3..=5).contains(&y) {
            continue;
        }
        let crater_radius = if rng.gen_ratio(1, 5) { 2 } else { 1 };
        if craters.iter().any(|other: &Crater| {
            let dx = other.x - x;
            let dy = other.y - y;
            dx * dx + dy * dy <= (other.radius + crater_radius + 1).pow(2)
        }) {
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
            blend(palette.shadow, palette.crater, 0.20)
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
    phase: MoonPhase,
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
    let face_dark = if matches!(phase, MoonPhase::New) {
        palette.star
    } else {
        rgba(19, 26, 35, 255)
    };
    let eye_glint = rgba(235, 244, 246, 255);
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

    for y in -1..=2 {
        for x in -1..=1 {
            put_eye_pixel(eye_cx + x, eye_cy + y, dark);
        }
    }
    put_eye_pixel(eye_cx - 1, eye_cy - 1, glint);
    put_eye_pixel(eye_cx, eye_cy - 1, glint);
}

fn generate_stars(
    rng: &mut Mt19937GenRand32,
    count: u8,
    width: i32,
    height: i32,
    moon_cx: i32,
    moon_cy: i32,
    moon_radius: i32,
) -> Vec<Star> {
    let mut stars = Vec::with_capacity(count as usize);
    let mut attempts = 0;
    while stars.len() < count as usize && attempts < count as usize * 40 {
        attempts += 1;
        let x = rng.gen_range(2..width.saturating_sub(2).max(3));
        let y = rng.gen_range(2..height.saturating_sub(2).max(3));
        let dx = x - moon_cx;
        let dy = y - moon_cy;
        if dx * dx + dy * dy <= (moon_radius + 4).pow(2) {
            continue;
        }
        if stars.iter().any(|other: &Star| {
            let sx = other.x - x;
            let sy = other.y - y;
            sx * sx + sy * sy <= 9
        }) {
            continue;
        }
        stars.push(Star {
            x,
            y,
            phase: rng.gen_range(0..6),
        });
    }
    stars
}

fn draw_stars(image: &mut RgbaImage, stars: &[Star], palette: MoonPalette, twinkle_frame: u8) {
    let dim = blend(palette.star, rgba(40, 48, 67, 255), 0.62);
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

fn adjust(color: Color, delta: i16) -> Color {
    let apply = |v: u8| (v as i16 + delta).clamp(0, 255) as u8;
    Rgba([apply(color[0]), apply(color[1]), apply(color[2]), color[3]])
}

fn pixel_hash(seed: u64, x: i32, y: i32) -> u64 {
    let mut z = seed
        ^ (x as i64 as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (y as i64 as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z ^= z >> 30;
    z = z.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z ^= z >> 27;
    z = z.wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
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
    use super::{Circle, Star, draw_face, draw_stars, phase_is_lit};
    use crate::clippymoon_gen::types::{MoonColor, MoonExpression, MoonPhase};
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

    #[test]
    fn phase_mask_orders_illumination_sensibly() {
        let new = lit_pixels(MoonPhase::New);
        let crescent = lit_pixels(MoonPhase::WaxingCrescent);
        let quarter = lit_pixels(MoonPhase::FirstQuarter);
        let gibbous = lit_pixels(MoonPhase::WaxingGibbous);
        let full = lit_pixels(MoonPhase::Full);
        assert_eq!(new, 0);
        assert!(new < crescent);
        assert!(crescent < quarter);
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
            MoonPhase::Full,
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
}
