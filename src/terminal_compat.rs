use ratatui::{
    backend::{Backend, ClearType, WindowSize},
    buffer::Cell,
    layout::{Position, Size},
    style::Color,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalColorMode {
    Native,
    Indexed256,
}

impl TerminalColorMode {
    pub fn detect() -> Self {
        terminal_color_mode_for(
            std::env::consts::OS,
            std::env::var("TERM_PROGRAM").ok().as_deref(),
            std::env::var("TERM_PROGRAM_VERSION").ok().as_deref(),
        )
    }
}

pub fn terminal_color_mode_for(
    target_os: &str,
    term_program: Option<&str>,
    term_program_version: Option<&str>,
) -> TerminalColorMode {
    if !target_os.eq_ignore_ascii_case("macos")
        || !term_program.is_some_and(|value| value.eq_ignore_ascii_case("Apple_Terminal"))
    {
        return TerminalColorMode::Native;
    }

    // Terminal.app before build 465 (Terminal 2.15) does not reliably support 24-bit RGB.
    // Missing/unparseable version metadata falls back conservatively to the stable 256-color path.
    let build = term_program_version
        .and_then(|version| version.split('.').next())
        .and_then(|major| major.parse::<u32>().ok());
    if build.is_some_and(|build| build >= 465) {
        TerminalColorMode::Native
    } else {
        TerminalColorMode::Indexed256
    }
}

pub struct ColorCompatBackend<B> {
    inner: B,
    color_mode: TerminalColorMode,
}

impl<B> ColorCompatBackend<B> {
    pub fn new(inner: B, color_mode: TerminalColorMode) -> Self {
        Self { inner, color_mode }
    }

    pub fn for_environment(inner: B) -> Self {
        Self::new(inner, TerminalColorMode::detect())
    }
}

impl<B: Backend> Backend for ColorCompatBackend<B> {
    type Error = B::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        if self.color_mode == TerminalColorMode::Native {
            return self.inner.draw(content);
        }

        let mapped = content
            .map(|(x, y, cell)| {
                let mut cell = cell.clone();
                cell.fg = indexed_terminal_color(cell.fg);
                cell.bg = indexed_terminal_color(cell.bg);
                cell.underline_color = indexed_terminal_color(cell.underline_color);
                (x, y, cell)
            })
            .collect::<Vec<_>>();
        self.inner
            .draw(mapped.iter().map(|(x, y, cell)| (*x, *y, cell)))
    }

    fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.size()
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }
}

fn indexed_terminal_color(color: Color) -> Color {
    match color {
        // Apple Terminal already handles the standard ANSI and indexed palettes correctly. Only
        // truecolor needs downgrading on affected builds; preserving named colors also respects a
        // user's customized Terminal palette instead of freezing those slots to xterm defaults.
        Color::Rgb(r, g, b) => rgb_to_ansi256(r, g, b),
        other => other,
    }
}

fn rgb_to_ansi256(r: u8, g: u8, b: u8) -> Color {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

    let nearest_level = |value: u8| -> (u8, u8) {
        let mut best_index = 0usize;
        let mut best_distance = u16::MAX;
        for (index, level) in LEVELS.iter().enumerate() {
            let distance = i16::from(value).abs_diff(i16::from(*level));
            if distance < best_distance {
                best_distance = distance;
                best_index = index;
            }
        }
        (best_index as u8, LEVELS[best_index])
    };

    let (ri, rc) = nearest_level(r);
    let (gi, gc) = nearest_level(g);
    let (bi, bc) = nearest_level(b);
    let cube_index = 16 + (36 * ri) + (6 * gi) + bi;
    let cube_distance = color_distance_sq((r, g, b), (rc, gc, bc));

    let average = (u16::from(r) + u16::from(g) + u16::from(b)) / 3;
    let gray_step = if average <= 8 {
        0
    } else if average >= 238 {
        23
    } else {
        ((average - 8 + 5) / 10).min(23) as u8
    };
    let gray_value = 8u8.saturating_add(gray_step.saturating_mul(10));
    let gray_distance = color_distance_sq((r, g, b), (gray_value, gray_value, gray_value));

    if gray_distance < cube_distance {
        Color::Indexed(232 + gray_step)
    } else {
        Color::Indexed(cube_index)
    }
}

fn color_distance_sq(a: (u8, u8, u8), b: (u8, u8, u8)) -> u32 {
    let dr = i32::from(a.0) - i32::from(b.0);
    let dg = i32::from(a.1) - i32::from(b.1);
    let db = i32::from(a.2) - i32::from(b.2);
    (dr * dr + dg * dg + db * db) as u32
}

#[cfg(test)]
mod tests {
    use super::{ColorCompatBackend, TerminalColorMode, rgb_to_ansi256, terminal_color_mode_for};
    use ratatui::{
        backend::{Backend, TestBackend},
        buffer::Cell,
        style::Color,
    };

    #[test]
    fn apple_terminal_uses_indexed_color_compatibility() {
        assert_eq!(
            terminal_color_mode_for("macos", Some("Apple_Terminal"), None),
            TerminalColorMode::Indexed256
        );
        assert_eq!(
            terminal_color_mode_for("macos", Some("Apple_Terminal"), Some("464.9")),
            TerminalColorMode::Indexed256
        );
        assert_eq!(
            terminal_color_mode_for("macos", Some("Apple_Terminal"), Some("465")),
            TerminalColorMode::Native
        );
        assert_eq!(
            terminal_color_mode_for("macos", Some("Apple_Terminal"), Some("465.1")),
            TerminalColorMode::Native
        );
        assert_eq!(
            terminal_color_mode_for("macos", Some("iTerm.app"), Some("1")),
            TerminalColorMode::Native
        );
        assert_eq!(
            terminal_color_mode_for("windows", Some("Apple_Terminal"), Some("1")),
            TerminalColorMode::Native
        );
    }

    #[test]
    fn rgb_mapping_uses_stable_xterm_256_colors() {
        assert_eq!(rgb_to_ansi256(255, 0, 0), Color::Indexed(196));
        assert_eq!(rgb_to_ansi256(0, 255, 255), Color::Indexed(51));
        assert_eq!(rgb_to_ansi256(255, 255, 255), Color::Indexed(231));
        assert_eq!(rgb_to_ansi256(128, 128, 128), Color::Indexed(244));
    }

    #[test]
    fn indexed_backend_rewrites_rgb_cells_before_terminal_output() {
        let mut backend =
            ColorCompatBackend::new(TestBackend::new(1, 1), TerminalColorMode::Indexed256);
        let mut cell = Cell::default();
        cell.set_symbol("X");
        cell.fg = Color::Rgb(255, 0, 0);
        cell.bg = Color::Rgb(0, 255, 255);
        backend
            .draw(std::iter::once((0, 0, &cell)))
            .expect("draw mapped terminal cell");

        let rendered = &backend.inner.buffer()[(0, 0)];
        assert_eq!(rendered.fg, Color::Indexed(196));
        assert_eq!(rendered.bg, Color::Indexed(51));
    }

    #[test]
    fn indexed_backend_preserves_standard_ansi_colors() {
        let mut backend =
            ColorCompatBackend::new(TestBackend::new(1, 1), TerminalColorMode::Indexed256);
        let mut cell = Cell::default();
        cell.set_symbol("X");
        cell.fg = Color::Red;
        cell.bg = Color::Indexed(17);
        backend
            .draw(std::iter::once((0, 0, &cell)))
            .expect("draw ANSI terminal cell");

        let rendered = &backend.inner.buffer()[(0, 0)];
        assert_eq!(rendered.fg, Color::Red);
        assert_eq!(rendered.bg, Color::Indexed(17));
    }

    #[test]
    fn native_backend_preserves_truecolor_cells() {
        let mut backend =
            ColorCompatBackend::new(TestBackend::new(1, 1), TerminalColorMode::Native);
        let mut cell = Cell::default();
        cell.set_symbol("X");
        cell.fg = Color::Rgb(247, 118, 142);
        backend
            .draw(std::iter::once((0, 0, &cell)))
            .expect("draw native terminal cell");

        assert_eq!(backend.inner.buffer()[(0, 0)].fg, Color::Rgb(247, 118, 142));
    }
}
