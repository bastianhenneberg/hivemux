//! Draws a vt100 screen into a ratatui buffer.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;

pub struct ScreenView<'a> {
    screen: &'a vt100::Screen,
}

impl<'a> ScreenView<'a> {
    pub fn new(screen: &'a vt100::Screen) -> Self {
        Self { screen }
    }

    /// Where the terminal cursor should be shown for a view drawn at `area`,
    /// or `None` if the program has hidden it.
    pub fn cursor(&self, area: Rect) -> Option<Position> {
        if self.screen.hide_cursor() {
            return None;
        }
        let (row, col) = self.screen.cursor_position();
        (row < area.height && col < area.width).then(|| Position::new(area.x + col, area.y + row))
    }
}

impl Widget for ScreenView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let (rows, cols) = self.screen.size();
        for row in 0..rows.min(area.height) {
            for col in 0..cols.min(area.width) {
                let Some(cell) = self.screen.cell(row, col) else {
                    continue;
                };
                // The right half of a wide character is covered by the left
                // half, ratatui skips it on its own.
                if cell.is_wide_continuation() {
                    continue;
                }
                let target = &mut buf[(area.x + col, area.y + row)];
                if cell.has_contents() {
                    target.set_symbol(cell.contents());
                } else {
                    target.set_symbol(" ");
                }
                target.set_style(cell_style(cell));
            }
        }
    }
}

fn cell_style(cell: &vt100::Cell) -> Style {
    let mut fg = convert_color(cell.fgcolor());
    let mut bg = convert_color(cell.bgcolor());
    if cell.inverse() {
        // Inverting the default colors has to be done by the host terminal,
        // it is the only one that knows what they are.
        if fg == Color::Reset && bg == Color::Reset {
            return Style::new().add_modifier(Modifier::REVERSED | attrs(cell));
        }
        std::mem::swap(&mut fg, &mut bg);
    }
    Style::new().fg(fg).bg(bg).add_modifier(attrs(cell))
}

fn attrs(cell: &vt100::Cell) -> Modifier {
    let mut m = Modifier::empty();
    if cell.bold() {
        m |= Modifier::BOLD;
    }
    if cell.dim() {
        m |= Modifier::DIM;
    }
    if cell.italic() {
        m |= Modifier::ITALIC;
    }
    if cell.underline() {
        m |= Modifier::UNDERLINED;
    }
    m
}

fn convert_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_text_and_colors() {
        let mut parser = vt100::Parser::new(2, 10, 0);
        parser.process(b"hi \x1b[31mred\x1b[0m");

        let area = Rect::new(0, 0, 10, 2);
        let mut buf = Buffer::empty(area);
        ScreenView::new(parser.screen()).render(area, &mut buf);

        assert_eq!(buf[(0, 0)].symbol(), "h");
        assert_eq!(buf[(3, 0)].symbol(), "r");
        assert_eq!(buf[(3, 0)].fg, Color::Indexed(1));
        assert_eq!(buf[(0, 0)].fg, Color::Reset);
    }

    #[test]
    fn cursor_follows_the_screen() {
        let mut parser = vt100::Parser::new(5, 10, 0);
        parser.process(b"ab\r\ncd");
        let view = ScreenView::new(parser.screen());
        assert_eq!(
            view.cursor(Rect::new(2, 3, 10, 5)),
            Some(Position::new(4, 4))
        );

        parser.process(b"\x1b[?25l");
        assert_eq!(
            ScreenView::new(parser.screen()).cursor(Rect::new(0, 0, 10, 5)),
            None
        );
    }
}
