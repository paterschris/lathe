use alacritty_terminal::vte::ansi;
use gpui::Hsla;
use std::ops::Range;
use theme::Theme;

#[derive(Default)]
pub(super) struct ConsoleHandler {
    pub(super) output: String,
    pub(super) spans: Vec<(Range<usize>, Option<ansi::Color>)>,
    pub(super) background_spans: Vec<(Range<usize>, Option<ansi::Color>)>,
    pub(super) current_range_start: usize,
    pub(super) current_background_range_start: usize,
    pub(super) current_color: Option<ansi::Color>,
    pub(super) current_background_color: Option<ansi::Color>,
    pos: usize,
}

impl ConsoleHandler {
    fn break_span(&mut self, color: Option<ansi::Color>) {
        self.spans.push((
            self.current_range_start..self.output.len(),
            self.current_color,
        ));
        self.current_color = color;
        self.current_range_start = self.pos;
    }

    fn break_background_span(&mut self, color: Option<ansi::Color>) {
        self.background_spans.push((
            self.current_background_range_start..self.output.len(),
            self.current_background_color,
        ));
        self.current_background_color = color;
        self.current_background_range_start = self.pos;
    }
}

impl ansi::Handler for ConsoleHandler {
    fn input(&mut self, c: char) {
        self.output.push(c);
        self.pos += c.len_utf8();
    }

    fn linefeed(&mut self) {
        self.output.push('\n');
        self.pos += 1;
    }

    fn put_tab(&mut self, count: u16) {
        self.output
            .extend(std::iter::repeat('\t').take(count as usize));
        self.pos += count as usize;
    }

    fn terminal_attribute(&mut self, attr: ansi::Attr) {
        match attr {
            ansi::Attr::Foreground(color) => {
                self.break_span(Some(color));
            }
            ansi::Attr::Background(color) => {
                self.break_background_span(Some(color));
            }
            ansi::Attr::Reset => {
                self.break_span(None);
                self.break_background_span(None);
            }
            _ => {}
        }
    }
}

pub(super) fn color_fetcher(color: ansi::Color) -> fn(&Theme) -> Hsla {
    let color_fetcher: fn(&Theme) -> Hsla = match color {
        ansi::Color::Named(n) => match n {
            ansi::NamedColor::Black => |theme| theme.colors().terminal_ansi_black,
            ansi::NamedColor::Red => |theme| theme.colors().terminal_ansi_red,
            ansi::NamedColor::Green => |theme| theme.colors().terminal_ansi_green,
            ansi::NamedColor::Yellow => |theme| theme.colors().terminal_ansi_yellow,
            ansi::NamedColor::Blue => |theme| theme.colors().terminal_ansi_blue,
            ansi::NamedColor::Magenta => |theme| theme.colors().terminal_ansi_magenta,
            ansi::NamedColor::Cyan => |theme| theme.colors().terminal_ansi_cyan,
            ansi::NamedColor::White => |theme| theme.colors().terminal_ansi_white,
            ansi::NamedColor::BrightBlack => |theme| theme.colors().terminal_ansi_bright_black,
            ansi::NamedColor::BrightRed => |theme| theme.colors().terminal_ansi_bright_red,
            ansi::NamedColor::BrightGreen => |theme| theme.colors().terminal_ansi_bright_green,
            ansi::NamedColor::BrightYellow => |theme| theme.colors().terminal_ansi_bright_yellow,
            ansi::NamedColor::BrightBlue => |theme| theme.colors().terminal_ansi_bright_blue,
            ansi::NamedColor::BrightMagenta => |theme| theme.colors().terminal_ansi_bright_magenta,
            ansi::NamedColor::BrightCyan => |theme| theme.colors().terminal_ansi_bright_cyan,
            ansi::NamedColor::BrightWhite => |theme| theme.colors().terminal_ansi_bright_white,
            ansi::NamedColor::Foreground => |theme| theme.colors().terminal_foreground,
            ansi::NamedColor::Background => |theme| theme.colors().terminal_background,
            ansi::NamedColor::Cursor => |theme| theme.players().local().cursor,
            ansi::NamedColor::DimBlack => |theme| theme.colors().terminal_ansi_dim_black,
            ansi::NamedColor::DimRed => |theme| theme.colors().terminal_ansi_dim_red,
            ansi::NamedColor::DimGreen => |theme| theme.colors().terminal_ansi_dim_green,
            ansi::NamedColor::DimYellow => |theme| theme.colors().terminal_ansi_dim_yellow,
            ansi::NamedColor::DimBlue => |theme| theme.colors().terminal_ansi_dim_blue,
            ansi::NamedColor::DimMagenta => |theme| theme.colors().terminal_ansi_dim_magenta,
            ansi::NamedColor::DimCyan => |theme| theme.colors().terminal_ansi_dim_cyan,
            ansi::NamedColor::DimWhite => |theme| theme.colors().terminal_ansi_dim_white,
            ansi::NamedColor::BrightForeground => |theme| theme.colors().terminal_bright_foreground,
            ansi::NamedColor::DimForeground => |theme| theme.colors().terminal_dim_foreground,
        },
        ansi::Color::Spec(_) => |theme| theme.colors().editor_background,
        ansi::Color::Indexed(i) => match i {
            0 => |theme| theme.colors().terminal_ansi_black,
            1 => |theme| theme.colors().terminal_ansi_red,
            2 => |theme| theme.colors().terminal_ansi_green,
            3 => |theme| theme.colors().terminal_ansi_yellow,
            4 => |theme| theme.colors().terminal_ansi_blue,
            5 => |theme| theme.colors().terminal_ansi_magenta,
            6 => |theme| theme.colors().terminal_ansi_cyan,
            7 => |theme| theme.colors().terminal_ansi_white,
            8 => |theme| theme.colors().terminal_ansi_bright_black,
            9 => |theme| theme.colors().terminal_ansi_bright_red,
            10 => |theme| theme.colors().terminal_ansi_bright_green,
            11 => |theme| theme.colors().terminal_ansi_bright_yellow,
            12 => |theme| theme.colors().terminal_ansi_bright_blue,
            13 => |theme| theme.colors().terminal_ansi_bright_magenta,
            14 => |theme| theme.colors().terminal_ansi_bright_cyan,
            15 => |theme| theme.colors().terminal_ansi_bright_white,
            _ => |_| gpui::black(),
        },
    };
    color_fetcher
}
