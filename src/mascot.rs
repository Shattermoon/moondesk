use crate::clippymoon_gen;
use image::{
    Delay, DynamicImage, Frame, ImageFormat, Rgba, RgbaImage,
    codecs::gif::{GifEncoder, Repeat},
};
use ratatui::{
    prelude::{Color, Style},
    text::{Line, Span},
};
use std::{
    fs::{self, File},
    io::BufWriter,
    path::{Path, PathBuf},
};

const MASCOT_FRAME_WIDTH: u32 = 40;
const MASCOT_FRAME_HEIGHT: u32 = 32;
const MASCOT_FRAME_MS: u64 = 80;
const CLIPPYMOON_EXPORT_SIZE: u32 = 512;
pub const TUI_MASCOT_BLOCK_WIDTH: u16 = MASCOT_FRAME_WIDTH as u16 + 2;
pub const TUI_MASCOT_BLOCK_HEIGHT: u16 = (MASCOT_FRAME_HEIGHT as u16).div_ceil(2) + 2;

// eye openness, vertical bob, twinkle phase, repeat count
const MASCOT_SEQUENCE: &[(u8, i32, u8, u8)] = &[
    (10, 0, 0, 6),
    (10, -1, 1, 4),
    (10, -1, 2, 4),
    (10, 0, 3, 5),
    (5, 0, 4, 1),
    (0, 0, 5, 1),
    (5, 0, 0, 1),
    (10, 1, 1, 4),
    (10, 1, 2, 4),
    (10, 0, 3, 6),
];

/// One terminal half-block cell with optional foreground and background RGB colors.
#[derive(Clone)]
pub struct TuiMascotCell {
    pub glyph: char,
    pub fg: Option<(u8, u8, u8)>,
    pub bg: Option<(u8, u8, u8)>,
}

/// A complete terminal-renderable ClippyMoon frame.
#[derive(Clone)]
pub struct TuiMascotFrame {
    pub rows: Vec<Vec<TuiMascotCell>>,
}

/// Precomputed terminal animation frames for the current session's ClippyMoon.
#[derive(Clone)]
pub struct MascotPack {
    pub frame_ms: u64,
    pub tui_frames: Vec<TuiMascotFrame>,
}

/// Paths, seed, and identity traits produced by an explicit ClippyMoon export.
pub struct ClippyMoonExport {
    pub seed: u64,
    pub png_path: PathBuf,
    pub gif_path: PathBuf,
    pub traits: clippymoon_gen::ClippyMoonTraits,
}

impl MascotPack {
    /// Select the animation frame corresponding to the supplied monotonic-ish millisecond clock.
    pub fn current_tui_frame(&self, now_millis: u128) -> &TuiMascotFrame {
        let idx = if self.tui_frames.is_empty() {
            0
        } else {
            ((now_millis / self.frame_ms as u128) as usize) % self.tui_frames.len()
        };
        &self.tui_frames[idx]
    }
}

/// Build one curated ClippyMoon entirely in memory for the current MoonDesk session.
/// The seed selects a known-good identity; animation frames only vary blink/bob/twinkle state.
pub fn build_workspace_mascot(seed: u64) -> MascotPack {
    let frames = mascot_source_frames(seed);
    let tui_frames = frames.iter().map(build_tui_frame).collect();
    MascotPack {
        frame_ms: MASCOT_FRAME_MS,
        tui_frames,
    }
}

/// Explicitly export one deterministic ClippyMoon as a static PNG and animated GIF.
/// Normal MoonDesk startup never writes mascot files.
pub fn export_clippymoon(
    seed: Option<u64>,
    output_dir: &Path,
) -> std::io::Result<ClippyMoonExport> {
    let seed = seed.unwrap_or_else(rand::random::<u64>);
    fs::create_dir_all(output_dir)?;

    let png_path = output_dir.join("clippymoon.png");
    let gif_path = output_dir.join("clippymoon.gif");
    let traits = clippymoon_gen::traits_from_seed(seed);

    let (character, _) = clippymoon_gen::create_character(
        Some(seed),
        MASCOT_FRAME_WIDTH,
        MASCOT_FRAME_HEIGHT,
        1.0,
        0,
        0,
    );
    let character = scale_for_export(&character)?;
    write_png(&png_path, &character)?;

    let animation = mascot_animation_frames(seed);
    let mut encoder = GifEncoder::new(BufWriter::new(File::create(&gif_path)?));
    encoder
        .set_repeat(Repeat::Infinite)
        .map_err(std::io::Error::other)?;
    let mut gif_frames = Vec::with_capacity(animation.len());
    for (frame, delay_ms) in animation {
        let scaled = scale_for_export(&frame)?;
        gif_frames.push(Frame::from_parts(
            scaled,
            0,
            0,
            Delay::from_numer_denom_ms(delay_ms as u32, 1),
        ));
    }
    encoder
        .encode_frames(gif_frames)
        .map_err(std::io::Error::other)?;

    Ok(ClippyMoonExport {
        seed,
        png_path,
        gif_path,
        traits,
    })
}

/// Convert a terminal mascot frame into centered Ratatui lines for the mascot panel.
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

fn mascot_sequence_frames(seed: u64) -> Vec<(RgbaImage, u8)> {
    MASCOT_SEQUENCE
        .iter()
        .map(|&(eye_openness, bob_offset, twinkle_frame, repeat)| {
            let (frame, _) = clippymoon_gen::create_character(
                Some(seed),
                MASCOT_FRAME_WIDTH,
                MASCOT_FRAME_HEIGHT,
                openness_value(eye_openness),
                bob_offset,
                twinkle_frame,
            );
            (frame, repeat)
        })
        .collect()
}

fn mascot_source_frames(seed: u64) -> Vec<RgbaImage> {
    mascot_sequence_frames(seed)
        .into_iter()
        .flat_map(|(frame, repeat)| std::iter::repeat_n(frame, repeat as usize))
        .collect()
}

fn mascot_animation_frames(seed: u64) -> Vec<(RgbaImage, u64)> {
    mascot_sequence_frames(seed)
        .into_iter()
        .map(|(frame, repeat)| (frame, repeat as u64 * MASCOT_FRAME_MS))
        .collect()
}

fn scale_for_export(frame: &RgbaImage) -> std::io::Result<RgbaImage> {
    let max_dim = frame.width().max(frame.height());
    if max_dim == 0 {
        return Err(std::io::Error::other(
            "ClippyMoon export frame has zero size",
        ));
    }
    let scale = CLIPPYMOON_EXPORT_SIZE / max_dim;
    if scale == 0 {
        return Err(std::io::Error::other(format!(
            "ClippyMoon frame {}x{} exceeds export size {}x{}",
            frame.width(),
            frame.height(),
            CLIPPYMOON_EXPORT_SIZE,
            CLIPPYMOON_EXPORT_SIZE
        )));
    }

    let width = frame.width() * scale;
    let height = frame.height() * scale;
    let scaled =
        image::imageops::resize(frame, width, height, image::imageops::FilterType::Nearest);
    let mut canvas = RgbaImage::from_pixel(
        CLIPPYMOON_EXPORT_SIZE,
        CLIPPYMOON_EXPORT_SIZE,
        Rgba([0, 0, 0, 0]),
    );
    let x = (CLIPPYMOON_EXPORT_SIZE - width) / 2;
    let y = (CLIPPYMOON_EXPORT_SIZE - height) / 2;
    image::imageops::overlay(&mut canvas, &scaled, x.into(), y.into());
    Ok(canvas)
}

fn write_png(path: &Path, image: &RgbaImage) -> std::io::Result<()> {
    let mut file = File::create(path)?;
    DynamicImage::ImageRgba8(image.clone())
        .write_to(&mut file, ImageFormat::Png)
        .map_err(std::io::Error::other)
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

fn openness_value(value: u8) -> f32 {
    f32::from(value.min(10)) / 10.0
}

#[cfg(test)]
mod tests {
    use super::{CLIPPYMOON_EXPORT_SIZE, MASCOT_SEQUENCE, export_clippymoon};
    use image::{AnimationDecoder, codecs::gif::GifDecoder};
    use std::{fs::File, io::BufReader};

    #[test]
    fn explicit_export_writes_png_and_gif_without_startup_archiving() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let output_dir = std::env::temp_dir().join(format!("moondesk-clippymoon-export-{unique}"));
        let export = export_clippymoon(Some(0x42), &output_dir).expect("export ClippyMoon");

        assert_eq!(export.seed, 0x42);
        assert_eq!(export.png_path, output_dir.join("clippymoon.png"));
        assert_eq!(export.gif_path, output_dir.join("clippymoon.gif"));
        assert!(export.png_path.is_file());
        assert!(export.gif_path.is_file());

        let png = image::open(&export.png_path)
            .expect("open exported png")
            .to_rgba8();
        assert_eq!(
            png.dimensions(),
            (CLIPPYMOON_EXPORT_SIZE, CLIPPYMOON_EXPORT_SIZE)
        );
        assert!(
            std::fs::metadata(&export.gif_path)
                .expect("gif metadata")
                .len()
                > 0
        );
        let decoder = GifDecoder::new(BufReader::new(
            File::open(&export.gif_path).expect("open exported gif"),
        ))
        .expect("decode exported gif");
        let gif_frames = decoder
            .into_frames()
            .collect_frames()
            .expect("collect exported gif frames");
        assert_eq!(gif_frames.len(), MASCOT_SEQUENCE.len());

        let _ = std::fs::remove_dir_all(output_dir);
    }
}
