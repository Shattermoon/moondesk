use crate::binagotchy_gen;
use image::{
    Delay, DynamicImage, Frame, ImageFormat, Rgba, RgbaImage,
    codecs::gif::{GifEncoder, Repeat},
};
use ratatui::{
    prelude::{Color, Style},
    text::{Line, Span},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{self, File},
    path::{Path, PathBuf},
};
use time::{OffsetDateTime, macros::format_description};

const MASCOT_CANVAS: u32 = 32;
const MASCOT_UPSCALE: u32 = 1;
const MASCOT_FRAME_MS: u64 = 50;
const MASCOT_SPIRIT_PERCENT: u64 = 1;
const MASCOT_SPIRIT_FRAME_WIDTH: u32 = 40;
const MASCOT_SPIRIT_FRAME_HEIGHT: u32 = 32;
pub const TUI_MASCOT_BLOCK_WIDTH: u16 = MASCOT_SPIRIT_FRAME_WIDTH as u16 + 2;
pub const TUI_MASCOT_BLOCK_HEIGHT: u16 = ((MASCOT_SPIRIT_FRAME_HEIGHT as u16) + 1) / 2 + 2;
#[cfg_attr(test, allow(dead_code))]
const MOONDESK_DIR_NAME: &str = ".moondesk";
#[cfg_attr(test, allow(dead_code))]
const BINAGOTCHY_DIR_NAME: &str = "binagotchy";
const METADATA_FILE_NAME: &str = "metadata.toml";
const CHARACTER_PNG_FILE_NAME: &str = "character.png";
const ANIMATION_GIF_FILE_NAME: &str = "animation.gif";
const ARCHIVE_OUTPUT_SIZE: u32 = 512;
const MASCOT_SEQUENCE: &[(u8, i32, u8)] = &[
    (10, 1, 7),
    (10, 0, 7),
    (10, 1, 7),
    (10, 0, 7),
    (10, 1, 2),
    (5, 1, 1),
    (0, 1, 4),
    (5, 0, 1),
    (10, 0, 6),
    (10, 1, 7),
    (10, 0, 7),
];

#[derive(Clone)]
pub struct TuiMascotCell {
    pub glyph: char,
    pub fg: Option<(u8, u8, u8)>,
    pub bg: Option<(u8, u8, u8)>,
}

#[derive(Clone)]
pub struct TuiMascotFrame {
    pub rows: Vec<Vec<TuiMascotCell>>,
}

#[derive(Clone)]
pub struct MascotPack {
    pub frame_ms: u64,
    pub tui_frames: Vec<TuiMascotFrame>,
}

#[derive(Deserialize, Serialize)]
struct StoredMascotMetadata {
    seed: String,
    created_at: String,
    generator_version: String,
    frame_ms: u64,
    spirit: bool,
    traits: StoredMascotTraits,
}

#[derive(Deserialize, Serialize)]
struct StoredMascotTraits {
    fur: String,
    eyes: String,
    headwear: String,
    special: String,
}

impl MascotPack {
    pub fn current_tui_frame(&self, now_millis: u128) -> &TuiMascotFrame {
        let idx = if self.tui_frames.is_empty() {
            0
        } else {
            ((now_millis / self.frame_ms as u128) as usize) % self.tui_frames.len()
        };
        &self.tui_frames[idx]
    }
}

pub fn build_workspace_mascot(seed: u64) -> MascotPack {
    let frames = mascot_source_frames(seed);
    let cropped = crop_frames(&frames);
    let tui_frames = cropped.iter().map(build_tui_frame).collect();
    MascotPack {
        frame_ms: MASCOT_FRAME_MS,
        tui_frames,
    }
}

#[cfg_attr(test, allow(dead_code))]
pub fn archive_startup_mascot(seed: u64) -> std::io::Result<()> {
    archive_startup_mascot_to_root(seed, &moondesk_binagotchy_root()?)
}

fn archive_startup_mascot_to_root(seed: u64, root: &Path) -> std::io::Result<()> {
    let created_at = OffsetDateTime::now_utc();
    let timestamp = archive_timestamp(created_at)?;
    let archive_dir = root.join(format!("{}_{}", timestamp, seed_hex(seed)));
    create_archive_dir(&archive_dir)?;

    let (frames, delays_ms, traits, use_spirit) = archive_sequence(seed);
    if frames.is_empty() {
        return Err(std::io::Error::other(
            "generated mascot archive has no frames",
        ));
    }
    let archive_frames = prepare_archive_frames(&frames)?;

    write_png(
        &archive_dir.join(CHARACTER_PNG_FILE_NAME),
        &archive_frames[0],
    )?;
    write_gif(
        &archive_dir.join(ANIMATION_GIF_FILE_NAME),
        &archive_frames,
        &delays_ms,
    )?;

    let metadata = StoredMascotMetadata {
        seed: seed_hex(seed),
        created_at: timestamp,
        generator_version: env!("CARGO_PKG_VERSION").to_string(),
        frame_ms: MASCOT_FRAME_MS,
        spirit: use_spirit,
        traits: StoredMascotTraits {
            fur: required_trait(&traits, "fur")?,
            eyes: required_trait(&traits, "eyes")?,
            headwear: required_trait(&traits, "headwear")?,
            special: required_trait(&traits, "special")?,
        },
    };
    write_metadata(&archive_dir.join(METADATA_FILE_NAME), &metadata)?;

    Ok(())
}

pub fn render_tui_lines(frame: &TuiMascotFrame, area_height: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let target_height = area_height as usize;
    let top_padding = target_height.saturating_sub(frame.rows.len()) / 2;
    for _ in 0..top_padding {
        lines.push(Line::from(""));
    }
    for row in &frame.rows {
        let spans: Vec<Span<'static>> = row
            .iter()
            .map(|cell| {
                let mut style = Style::default();
                if let Some((r, g, b)) = cell.fg {
                    style = style.fg(Color::Rgb(r, g, b));
                }
                if let Some((r, g, b)) = cell.bg {
                    style = style.bg(Color::Rgb(r, g, b));
                }
                Span::styled(cell.glyph.to_string(), style)
            })
            .collect();
        lines.push(Line::from(spans));
    }
    while lines.len() < target_height {
        lines.push(Line::from(""));
    }
    lines
}

fn mascot_source_frames(seed: u64) -> Vec<RgbaImage> {
    let use_spirit = mascot_use_spirit(seed);
    let headwear_pref = mascot_headwear_preference(use_spirit);
    MASCOT_SEQUENCE
        .iter()
        .flat_map(|&(eye_openness, tail_state, repeat)| {
            let (frame, _) = binagotchy_gen::create_character(
                Some(seed),
                MASCOT_CANVAS,
                MASCOT_UPSCALE,
                "normal",
                headwear_pref,
                0.0,
                openness_value(eye_openness),
                tail_state,
            );
            let frame = if use_spirit {
                binagotchy_gen::apply_mascot_spirit_frame(
                    seed,
                    &frame,
                    MASCOT_SPIRIT_FRAME_WIDTH,
                    MASCOT_SPIRIT_FRAME_HEIGHT,
                )
            } else {
                frame
            };
            std::iter::repeat_n(frame, repeat as usize)
        })
        .collect()
}

fn mascot_use_spirit(seed: u64) -> bool {
    seed % 100 < MASCOT_SPIRIT_PERCENT
}

fn mascot_headwear_preference(use_spirit: bool) -> &'static str {
    if use_spirit { "none" } else { "random" }
}

fn archive_sequence(seed: u64) -> (Vec<RgbaImage>, Vec<u64>, HashMap<String, String>, bool) {
    let use_spirit = mascot_use_spirit(seed);
    let headwear_pref = mascot_headwear_preference(use_spirit);
    let mut traits: Option<HashMap<String, String>> = None;
    let mut frames = Vec::with_capacity(MASCOT_SEQUENCE.len());
    let mut delays_ms = Vec::with_capacity(MASCOT_SEQUENCE.len());

    for &(eye_openness, tail_state, repeat) in MASCOT_SEQUENCE {
        let (frame, frame_traits) = binagotchy_gen::create_character(
            Some(seed),
            MASCOT_CANVAS,
            MASCOT_UPSCALE,
            "normal",
            headwear_pref,
            0.0,
            openness_value(eye_openness),
            tail_state,
        );
        if traits.is_none() {
            traits = Some(frame_traits);
        }
        let frame = if use_spirit {
            binagotchy_gen::apply_mascot_spirit_frame(
                seed,
                &frame,
                MASCOT_SPIRIT_FRAME_WIDTH,
                MASCOT_SPIRIT_FRAME_HEIGHT,
            )
        } else {
            frame
        };
        frames.push(frame);
        delays_ms.push(repeat as u64 * MASCOT_FRAME_MS);
    }

    let mut traits = traits.unwrap_or_default();
    if use_spirit {
        traits.insert("special".to_string(), "spirit".to_string());
    }
    (frames, delays_ms, traits, use_spirit)
}

#[cfg_attr(test, allow(dead_code))]
pub(crate) fn moondesk_binagotchy_root() -> std::io::Result<PathBuf> {
    Ok(crate::state::user_home_dir()?
        .join(MOONDESK_DIR_NAME)
        .join(BINAGOTCHY_DIR_NAME))
}

fn archive_timestamp(created_at: OffsetDateTime) -> std::io::Result<String> {
    created_at
        .format(format_description!(
            "[year][month][day]T[hour][minute][second][subsecond digits:3]Z"
        ))
        .map_err(std::io::Error::other)
}

fn seed_hex(seed: u64) -> String {
    format!("{seed:016x}")
}

fn required_trait(traits: &HashMap<String, String>, key: &'static str) -> std::io::Result<String> {
    traits
        .get(key)
        .cloned()
        .ok_or_else(|| std::io::Error::other(format!("missing mascot trait: {key}")))
}

fn create_archive_dir(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn prepare_archive_frames(frames: &[RgbaImage]) -> std::io::Result<Vec<RgbaImage>> {
    let Some((frame_width, frame_height)) = frames.first().map(RgbaImage::dimensions) else {
        return Ok(Vec::new());
    };
    let max_dim = frame_width.max(frame_height);
    if max_dim == 0 {
        return Err(std::io::Error::other("archive frame has zero size"));
    }

    let scale = ARCHIVE_OUTPUT_SIZE / max_dim;
    if scale == 0 {
        return Err(std::io::Error::other(format!(
            "archive frame {frame_width}x{frame_height} exceeds {ARCHIVE_OUTPUT_SIZE}x{ARCHIVE_OUTPUT_SIZE}"
        )));
    }

    let scaled_width = frame_width * scale;
    let scaled_height = frame_height * scale;
    let offset_x = (ARCHIVE_OUTPUT_SIZE - scaled_width) / 2;
    let offset_y = (ARCHIVE_OUTPUT_SIZE - scaled_height) / 2;

    Ok(frames
        .iter()
        .map(|frame| {
            let scaled = image::imageops::resize(
                frame,
                scaled_width,
                scaled_height,
                image::imageops::FilterType::Nearest,
            );
            let mut canvas =
                RgbaImage::from_pixel(ARCHIVE_OUTPUT_SIZE, ARCHIVE_OUTPUT_SIZE, Rgba([0, 0, 0, 0]));
            image::imageops::overlay(&mut canvas, &scaled, offset_x.into(), offset_y.into());
            canvas
        })
        .collect())
}

fn write_png(path: &Path, image: &RgbaImage) -> std::io::Result<()> {
    let mut file = File::create(path)?;
    DynamicImage::ImageRgba8(image.clone())
        .write_to(&mut file, ImageFormat::Png)
        .map_err(std::io::Error::other)
}

fn write_gif(path: &Path, frames: &[RgbaImage], delays_ms: &[u64]) -> std::io::Result<()> {
    if frames.len() != delays_ms.len() {
        return Err(std::io::Error::other(
            "gif frame count does not match delay count",
        ));
    }
    let file = File::create(path)?;
    let mut encoder = GifEncoder::new(file);
    encoder
        .set_repeat(Repeat::Infinite)
        .map_err(std::io::Error::other)?;

    let animation_frames =
        frames
            .iter()
            .cloned()
            .zip(delays_ms.iter().copied())
            .map(|(frame, delay_ms)| {
                Frame::from_parts(frame, 0, 0, Delay::from_numer_denom_ms(delay_ms as u32, 1))
            });
    encoder
        .encode_frames(animation_frames)
        .map_err(std::io::Error::other)
}

fn write_metadata(path: &Path, metadata: &StoredMascotMetadata) -> std::io::Result<()> {
    let text = toml::to_string_pretty(metadata).map_err(std::io::Error::other)?;
    fs::write(path, text)
}

fn crop_frames(frames: &[RgbaImage]) -> Vec<RgbaImage> {
    let Some((frame_width, frame_height)) = frames.first().map(RgbaImage::dimensions) else {
        return Vec::new();
    };
    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x = 0_u32;
    let mut max_y = 0_u32;

    for frame in frames {
        let (width, height) = frame.dimensions();
        for y in 0..height {
            for x in 0..width {
                if frame.get_pixel(x, y)[3] == 0 {
                    continue;
                }
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }

    if min_x == u32::MAX {
        return frames.to_vec();
    }

    min_x = min_x.saturating_sub(2);
    min_y = min_y.saturating_sub(2);
    max_x = max_x.saturating_add(2).min(frame_width.saturating_sub(1));
    max_y = max_y.saturating_add(2).min(frame_height.saturating_sub(1));

    let width = max_x.saturating_sub(min_x).saturating_add(1);
    let height = max_y.saturating_sub(min_y).saturating_add(1);
    frames
        .iter()
        .map(|frame| image::imageops::crop_imm(frame, min_x, min_y, width, height).to_image())
        .collect()
}

fn build_tui_frame(frame: &RgbaImage) -> TuiMascotFrame {
    let (width, height) = frame.dimensions();
    let mut rows = Vec::new();
    let mut y = 0;
    while y < height {
        let mut row = Vec::new();
        for x in 0..width {
            let top = *frame.get_pixel(x, y);
            let bottom = if y + 1 < height {
                *frame.get_pixel(x, y + 1)
            } else {
                Rgba([0, 0, 0, 0])
            };
            row.push(build_tui_cell(top, bottom));
        }
        rows.push(row);
        y += 2;
    }

    TuiMascotFrame { rows }
}

fn build_tui_cell(top: image::Rgba<u8>, bottom: image::Rgba<u8>) -> TuiMascotCell {
    let top_alpha = top[3] > 0;
    let bottom_alpha = bottom[3] > 0;

    match (top_alpha, bottom_alpha) {
        (false, false) => TuiMascotCell {
            glyph: ' ',
            fg: None,
            bg: None,
        },
        (true, false) => TuiMascotCell {
            glyph: '▀',
            fg: Some((top[0], top[1], top[2])),
            bg: None,
        },
        (false, true) => TuiMascotCell {
            glyph: '▄',
            fg: Some((bottom[0], bottom[1], bottom[2])),
            bg: None,
        },
        (true, true) => {
            let top_rgb = (top[0], top[1], top[2]);
            let bottom_rgb = (bottom[0], bottom[1], bottom[2]);
            if top_rgb == bottom_rgb {
                TuiMascotCell {
                    glyph: '█',
                    fg: Some(top_rgb),
                    bg: None,
                }
            } else {
                TuiMascotCell {
                    glyph: '▀',
                    fg: Some(top_rgb),
                    bg: Some(bottom_rgb),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        archive_startup_mascot_to_root, mascot_headwear_preference, mascot_use_spirit,
        openness_value,
    };
    use crate::binagotchy_gen;

    #[test]
    fn mascot_non_spirit_can_generate_headwear() {
        let seed = (1..10_000_u64)
            .find(|seed| {
                if mascot_use_spirit(*seed) {
                    return false;
                }
                let (_, traits) = binagotchy_gen::create_character(
                    Some(*seed),
                    super::MASCOT_CANVAS,
                    super::MASCOT_UPSCALE,
                    "normal",
                    mascot_headwear_preference(false),
                    0.0,
                    openness_value(10),
                    1,
                );
                traits.get("headwear").is_some_and(|value| value != "none")
            })
            .expect("expected a non-spirit mascot seed that generates headwear");

        let (_, traits) = binagotchy_gen::create_character(
            Some(seed),
            super::MASCOT_CANVAS,
            super::MASCOT_UPSCALE,
            "normal",
            mascot_headwear_preference(false),
            0.0,
            openness_value(10),
            1,
        );
        assert_ne!(traits.get("headwear").map(String::as_str), Some("none"));
    }

    #[test]
    fn archive_startup_mascot_writes_expected_files() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let archive_root = std::env::temp_dir().join(format!("moondesk-binagotchy-{unique}"));
        archive_startup_mascot_to_root(1, &archive_root).expect("archive mascot");

        let mut entries = std::fs::read_dir(&archive_root)
            .expect("read archive root")
            .map(|entry| entry.expect("dir entry").path())
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 1);

        let archive_dir = entries.pop().expect("archive dir");
        assert!(archive_dir.join(super::METADATA_FILE_NAME).is_file());
        assert!(archive_dir.join(super::CHARACTER_PNG_FILE_NAME).is_file());
        assert!(archive_dir.join(super::ANIMATION_GIF_FILE_NAME).is_file());

        let metadata_text = std::fs::read_to_string(archive_dir.join(super::METADATA_FILE_NAME))
            .expect("read metadata");
        assert!(metadata_text.contains("seed = \"0000000000000001\""));
        let expected_version = format!("generator_version = \"{}\"", env!("CARGO_PKG_VERSION"));
        assert!(metadata_text.contains(&expected_version));

        let archive_png = image::open(archive_dir.join(super::CHARACTER_PNG_FILE_NAME))
            .expect("open archive png")
            .to_rgba8();
        assert_eq!(
            archive_png.dimensions(),
            (super::ARCHIVE_OUTPUT_SIZE, super::ARCHIVE_OUTPUT_SIZE)
        );

        let _ = std::fs::remove_dir_all(&archive_root);
    }
}

fn openness_value(value: u8) -> f32 {
    match value {
        10 => 1.0,
        5 => 0.5,
        0 => 0.0,
        _ => panic!("unsupported mascot eye openness"),
    }
}
