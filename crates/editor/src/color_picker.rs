use std::{cell::Cell, ops::Range, rc::Rc};

use gpui::{
    Bounds, DismissEvent, DragMoveEvent, Empty, Entity, EventEmitter, FocusHandle, Focusable, Hsla,
    MouseButton, MouseDownEvent, Pixels, Point as PixelPoint, Rgba, Stateful, Subscription,
    WeakEntity, canvas, linear_color_stop, linear_gradient, relative,
};
use multi_buffer::{Anchor, MultiBufferRow, MultiBufferSnapshot, ToPoint as _};
use text::Point;
use ui::prelude::*;
use workspace::ModalView;

use crate::{Editor, EditorEvent, actions::PickColor};

#[derive(Clone, Copy, Debug, PartialEq)]
enum FunctionKind {
    Rgb,
    Hsl,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ColorSyntax {
    /// `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa`.
    Hash { digits: usize, uppercase: bool },
    /// `0xrrggbb` or `0xrrggbbaa`, the form GPUI's `rgb` and `rgba` take.
    ZeroX {
        digits: usize,
        uppercase: bool,
        uppercase_prefix: bool,
    },
    /// `rgb()`, `rgba()`, `hsl()` or `hsla()`, in either the comma-separated
    /// or the space-separated CSS syntax.
    Function {
        kind: FunctionKind,
        uppercase: bool,
        named_with_alpha: bool,
        commas: bool,
        has_alpha: bool,
        alpha_percent: bool,
        hue_degrees_suffix: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ColorLiteral {
    /// Byte range within the line the literal was found on.
    range: (usize, usize),
    color: Rgba,
    syntax: ColorSyntax,
}

impl ColorLiteral {
    pub(crate) fn range(&self) -> Range<usize> {
        self.range.0..self.range.1
    }

    pub(crate) fn color(&self) -> Rgba {
        self.color
    }

    /// `#123` and `#1234` are as likely to be issue references as colors.
    pub(crate) fn is_ambiguous_with_issue_number(&self, line: &str) -> bool {
        matches!(self.syntax, ColorSyntax::Hash { digits: 3 | 4, .. })
            && line
                .get(self.range.0 + 1..self.range.1)
                .is_some_and(|digits| digits.bytes().all(|byte| byte.is_ascii_digit()))
    }

    /// Writes `color` back out in the syntax this literal was written in, so
    /// picking a color never changes the notation the file uses. Alpha is
    /// added only when the new color needs it.
    fn format(&self, color: Rgba) -> String {
        let [red, green, blue, alpha] = to_bytes(color);
        let needs_alpha = alpha != 255;
        match self.syntax {
            ColorSyntax::Hash { digits, uppercase } => {
                let with_alpha = needs_alpha || digits == 4 || digits == 8;
                let short = (digits == 3 || digits == 4)
                    && [red, green, blue, alpha].iter().all(|byte| byte % 17 == 0);
                let text = match (short, with_alpha) {
                    (true, false) => format!("#{:x}{:x}{:x}", red / 17, green / 17, blue / 17),
                    (true, true) => format!(
                        "#{:x}{:x}{:x}{:x}",
                        red / 17,
                        green / 17,
                        blue / 17,
                        alpha / 17
                    ),
                    (false, false) => format!("#{red:02x}{green:02x}{blue:02x}"),
                    (false, true) => format!("#{red:02x}{green:02x}{blue:02x}{alpha:02x}"),
                };
                if uppercase { text.to_uppercase() } else { text }
            }
            ColorSyntax::ZeroX {
                digits,
                uppercase,
                uppercase_prefix,
            } => {
                let mut digits_text = format!("{red:02x}{green:02x}{blue:02x}");
                if needs_alpha || digits == 8 {
                    digits_text.push_str(&format!("{alpha:02x}"));
                }
                if uppercase {
                    digits_text = digits_text.to_uppercase();
                }
                let prefix = if uppercase_prefix { "0X" } else { "0x" };
                format!("{prefix}{digits_text}")
            }
            ColorSyntax::Function {
                kind,
                uppercase,
                named_with_alpha,
                commas,
                has_alpha,
                alpha_percent,
                hue_degrees_suffix,
            } => {
                let with_alpha = has_alpha || needs_alpha;
                let channels = match kind {
                    FunctionKind::Rgb => [red.to_string(), green.to_string(), blue.to_string()],
                    FunctionKind::Hsl => {
                        let hsla = Hsla::from(color);
                        let hue = (hsla.h * 360.).round() as u32 % 360;
                        let suffix = if hue_degrees_suffix { "deg" } else { "" };
                        [
                            format!("{hue}{suffix}"),
                            format!("{}%", (hsla.s * 100.).round() as u32),
                            format!("{}%", (hsla.l * 100.).round() as u32),
                        ]
                    }
                };
                let alpha_text = if alpha_percent {
                    format!("{}%", (color.a * 100.).round() as u32)
                } else {
                    format_fraction(color.a)
                };
                let base = match kind {
                    FunctionKind::Rgb => "rgb",
                    FunctionKind::Hsl => "hsl",
                };
                // The legacy comma syntax needs the `a` name to take a fourth
                // argument; the space syntax takes `/ alpha` under either name.
                let name_alpha = named_with_alpha || (commas && with_alpha);
                let mut name = format!("{base}{}", if name_alpha { "a" } else { "" });
                if uppercase {
                    name = name.to_uppercase();
                }
                let arguments = match (commas, with_alpha) {
                    (true, true) => format!("{}, {alpha_text}", channels.join(", ")),
                    (true, false) => channels.join(", "),
                    (false, true) => format!("{} / {alpha_text}", channels.join(" ")),
                    (false, false) => channels.join(" "),
                };
                format!("{name}({arguments})")
            }
        }
    }
}

fn to_bytes(color: Rgba) -> [u8; 4] {
    [color.r, color.g, color.b, color.a].map(|channel| (channel.clamp(0., 1.) * 255.).round() as u8)
}

fn format_fraction(value: f32) -> String {
    let text = format!("{:.2}", value.clamp(0., 1.));
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn hex_run(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .take_while(|byte| byte.is_ascii_hexdigit())
        .count()
}

fn parse_hex_digits(digits: &[u8]) -> Option<Rgba> {
    let nibble = |index: usize| -> Option<f32> {
        let byte = *digits.get(index)?;
        Some((byte as char).to_digit(16)? as f32)
    };
    let byte = |index: usize| -> Option<f32> { Some(nibble(index)? * 16. + nibble(index + 1)?) };
    let (red, green, blue, alpha) = match digits.len() {
        3 => (nibble(0)? * 17., nibble(1)? * 17., nibble(2)? * 17., 255.),
        4 => (
            nibble(0)? * 17.,
            nibble(1)? * 17.,
            nibble(2)? * 17.,
            nibble(3)? * 17.,
        ),
        6 => (byte(0)?, byte(2)?, byte(4)?, 255.),
        8 => (byte(0)?, byte(2)?, byte(4)?, byte(6)?),
        _ => return None,
    };
    Some(Rgba {
        r: red / 255.,
        g: green / 255.,
        b: blue / 255.,
        a: alpha / 255.,
    })
}

struct Number {
    value: f32,
    percent: bool,
    degrees: bool,
}

fn parse_number(token: &str) -> Option<Number> {
    let (token, percent) = match token.strip_suffix('%') {
        Some(token) => (token, true),
        None => (token, false),
    };
    let (token, degrees) = match token.strip_suffix("deg") {
        Some(token) => (token, true),
        None => (token, false),
    };
    let value = token.parse::<f32>().ok()?;
    value.is_finite().then_some(Number {
        value,
        percent,
        degrees,
    })
}

fn parse_function(kind: FunctionKind, name: &str, arguments: &str) -> Option<ColorLiteral> {
    let (channels, slash_alpha) = match arguments.split_once('/') {
        Some((channels, alpha)) => (channels, Some(alpha.trim())),
        None => (arguments, None),
    };
    let commas = channels.contains(',');
    let mut tokens: Vec<&str> = if commas {
        channels.split(',').map(str::trim).collect()
    } else {
        channels.split_whitespace().collect()
    };
    if tokens.iter().any(|token| token.is_empty()) {
        return None;
    }
    let alpha_token = match slash_alpha {
        Some(alpha) if !commas && tokens.len() == 3 => Some(alpha),
        Some(_) => return None,
        None if commas && tokens.len() == 4 => tokens.pop(),
        None => None,
    };
    if tokens.len() != 3 {
        return None;
    }
    let channels = tokens
        .iter()
        .map(|token| parse_number(token))
        .collect::<Option<Vec<_>>>()?;
    let alpha = match alpha_token {
        Some(token) => Some(parse_number(token)?),
        None => None,
    };

    let alpha_value = alpha.as_ref().map_or(1., |alpha| {
        if alpha.percent {
            alpha.value / 100.
        } else {
            alpha.value
        }
    });
    let color = match kind {
        FunctionKind::Rgb => {
            let channel = |number: &Number| {
                let value = if number.percent {
                    number.value / 100.
                } else {
                    number.value / 255.
                };
                value.clamp(0., 1.)
            };
            Rgba {
                r: channel(&channels[0]),
                g: channel(&channels[1]),
                b: channel(&channels[2]),
                a: alpha_value.clamp(0., 1.),
            }
        }
        FunctionKind::Hsl => Hsla {
            h: channels[0].value.rem_euclid(360.) / 360.,
            s: (channels[1].value / 100.).clamp(0., 1.),
            l: (channels[2].value / 100.).clamp(0., 1.),
            a: alpha_value.clamp(0., 1.),
        }
        .to_rgb(),
    };
    Some(ColorLiteral {
        range: (0, 0),
        color,
        syntax: ColorSyntax::Function {
            kind,
            uppercase: name.bytes().any(|byte| byte.is_ascii_uppercase()),
            named_with_alpha: name.len() == 4,
            commas,
            has_alpha: alpha.is_some(),
            alpha_percent: alpha.is_some_and(|alpha| alpha.percent),
            hue_degrees_suffix: kind == FunctionKind::Hsl && channels[0].degrees,
        },
    })
}

/// Every color literal on `line`, in order.
pub(crate) fn color_literals(line: &str) -> Vec<ColorLiteral> {
    let bytes = line.as_bytes();
    let mut literals = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let preceded_by_word = index > 0 && is_word_byte(bytes[index - 1]);
        let followed_by_word = |end: usize| bytes.get(end).copied().is_some_and(is_word_byte);

        // `&#123;` is an HTML character reference and `r#add` a Rust raw
        // identifier, neither of them a color.
        if bytes[index] == b'#' && !preceded_by_word && (index == 0 || bytes[index - 1] != b'&') {
            let digits = hex_run(&bytes[index + 1..]);
            let end = index + 1 + digits;
            if matches!(digits, 3 | 4 | 6 | 8)
                && !followed_by_word(end)
                && let Some(color) = parse_hex_digits(&bytes[index + 1..end])
            {
                literals.push(ColorLiteral {
                    range: (index, end),
                    color,
                    syntax: ColorSyntax::Hash {
                        digits,
                        uppercase: bytes[index + 1..end].iter().any(u8::is_ascii_uppercase),
                    },
                });
                index = end;
                continue;
            }
        }

        if bytes[index] == b'0'
            && !preceded_by_word
            && matches!(bytes.get(index + 1), Some(b'x' | b'X'))
        {
            let digits = hex_run(&bytes[index + 2..]);
            let end = index + 2 + digits;
            if matches!(digits, 6 | 8)
                && !followed_by_word(end)
                && let Some(color) = parse_hex_digits(&bytes[index + 2..end])
            {
                literals.push(ColorLiteral {
                    range: (index, end),
                    color,
                    syntax: ColorSyntax::ZeroX {
                        digits,
                        uppercase: bytes[index + 2..end].iter().any(u8::is_ascii_uppercase),
                        uppercase_prefix: bytes[index + 1] == b'X',
                    },
                });
                index = end;
                continue;
            }
        }

        if !preceded_by_word {
            let function = [
                ("rgba(", FunctionKind::Rgb),
                ("rgb(", FunctionKind::Rgb),
                ("hsla(", FunctionKind::Hsl),
                ("hsl(", FunctionKind::Hsl),
            ]
            .into_iter()
            .find(|(prefix, _)| {
                bytes
                    .get(index..index + prefix.len())
                    .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix.as_bytes()))
            });
            if let Some((prefix, kind)) = function {
                let open = index + prefix.len();
                let close = bytes[open..]
                    .iter()
                    .position(|byte| *byte == b')')
                    .map(|offset| open + offset);
                if let Some(close) = close
                    && let Ok(arguments) = std::str::from_utf8(&bytes[open..close])
                    && let Ok(name) = std::str::from_utf8(&bytes[index..open - 1])
                    && let Some(mut literal) = parse_function(kind, name, arguments)
                {
                    literal.range = (index, close + 1);
                    literals.push(literal);
                    index = close + 1;
                    continue;
                }
            }
        }

        index += 1;
    }
    literals
}

/// The color literal on `line` that `column` touches. A column just past the
/// end counts, so a cursor left right after typing a color still finds it.
fn color_literal_at(line: &str, column: usize) -> Option<ColorLiteral> {
    color_literals(line)
        .into_iter()
        .find(|literal| literal.range.0 <= column && column <= literal.range.1)
}

impl Editor {
    /// The color literal under the newest cursor, as an anchor range plus the
    /// parsed literal.
    pub(crate) fn color_literal_at_cursor(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<(Range<Anchor>, ColorLiteral)> {
        let display_snapshot = self.display_snapshot(cx);
        let cursor = self.selections.newest::<Point>(&display_snapshot).head();
        let snapshot = self.buffer().read(cx).snapshot(cx);
        color_literal_at_point(&snapshot, cursor)
    }

    pub(crate) fn pick_color(
        &mut self,
        _: &PickColor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((range, literal)) = self.color_literal_at_cursor(cx) else {
            return;
        };
        self.open_color_picker(range, literal, window, cx);
    }

    /// Opens the picker on the color literal at `position`: where a swatch
    /// sits, which is the literal's start.
    pub(crate) fn pick_color_at(
        &mut self,
        position: Anchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.buffer().read(cx).snapshot(cx);
        let point = position.to_point(&snapshot);
        let Some((range, literal)) = color_literal_at_point(&snapshot, point) else {
            return;
        };
        self.open_color_picker(range, literal, window, cx);
    }

    fn open_color_picker(
        &mut self,
        range: Range<Anchor>,
        literal: ColorLiteral,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only(cx) {
            return;
        }
        let Some(workspace) = self.workspace() else {
            return;
        };
        let editor = cx.entity().downgrade();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                ColorPicker::new(editor, range, literal, window, cx)
            });
        });
    }
}

fn color_literal_at_point(
    snapshot: &MultiBufferSnapshot,
    point: Point,
) -> Option<(Range<Anchor>, ColorLiteral)> {
    let line_start = Point::new(point.row, 0);
    let line_end = Point::new(point.row, snapshot.line_len(MultiBufferRow(point.row)));
    let line: String = snapshot.text_for_range(line_start..line_end).collect();
    let literal = color_literal_at(&line, point.column as usize)?;
    let start = snapshot.anchor_before(Point::new(point.row, literal.range.0 as u32));
    let end = snapshot.anchor_after(Point::new(point.row, literal.range.1 as u32));
    Some((start..end, literal))
}

/// Hue, saturation, value and alpha, each in `0.0..=1.0`. The picker keeps its
/// own HSV state rather than round-tripping through RGB, which would lose the
/// hue whenever saturation or value reaches zero.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Hsva {
    h: f32,
    s: f32,
    v: f32,
    a: f32,
}

impl Hsva {
    fn to_rgba(self) -> Rgba {
        let hue = self.h.rem_euclid(1.) * 6.;
        let chroma = self.v * self.s;
        let second = chroma * (1. - (hue % 2. - 1.).abs());
        let (red, green, blue) = match hue as u32 {
            0 => (chroma, second, 0.),
            1 => (second, chroma, 0.),
            2 => (0., chroma, second),
            3 => (0., second, chroma),
            4 => (second, 0., chroma),
            _ => (chroma, 0., second),
        };
        let offset = self.v - chroma;
        Rgba {
            r: red + offset,
            g: green + offset,
            b: blue + offset,
            a: self.a,
        }
    }

    fn from_rgba(color: Rgba) -> Self {
        let max = color.r.max(color.g).max(color.b);
        let min = color.r.min(color.g).min(color.b);
        let delta = max - min;
        let hue = if delta == 0. {
            0.
        } else if max == color.r {
            ((color.g - color.b) / delta).rem_euclid(6.)
        } else if max == color.g {
            (color.b - color.r) / delta + 2.
        } else {
            (color.r - color.g) / delta + 4.
        };
        Self {
            h: hue / 6.,
            s: if max == 0. { 0. } else { delta / max },
            v: max,
            a: color.a,
        }
    }

    fn with_alpha(self, alpha: f32) -> Rgba {
        Rgba {
            a: alpha,
            ..self.to_rgba()
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ColorChannel {
    SaturationValue,
    Hue,
    Alpha,
}

struct ColorDrag(ColorChannel);

/// A color picker drawn in the workspace's modal layer, which sits above every
/// pane, dock and border in the window. Edits the literal it was opened on
/// only when confirmed, so the change lands as a single undo step.
pub struct ColorPicker {
    editor: WeakEntity<Editor>,
    range: Range<Anchor>,
    literal: ColorLiteral,
    color: Hsva,
    input: Entity<Editor>,
    input_invalid: bool,
    syncing_input: bool,
    saturation_value_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    hue_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    alpha_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    _subscriptions: Vec<Subscription>,
}

impl ModalView for ColorPicker {}

impl EventEmitter<DismissEvent> for ColorPicker {}

impl Focusable for ColorPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl ColorPicker {
    fn new(
        editor: WeakEntity<Editor>,
        range: Range<Anchor>,
        literal: ColorLiteral,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let text = literal.format(literal.color);
        let input = cx.new(|cx| {
            let mut input = Editor::single_line(window, cx);
            input.set_text(text, window, cx);
            input.select_all(&Default::default(), window, cx);
            input
        });
        let subscription = cx.subscribe(&input, |this, _, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                this.input_edited(cx);
            }
        });
        Self {
            editor,
            range,
            literal,
            color: Hsva::from_rgba(literal.color),
            input,
            input_invalid: false,
            syncing_input: false,
            saturation_value_bounds: Rc::default(),
            hue_bounds: Rc::default(),
            alpha_bounds: Rc::default(),
            _subscriptions: vec![subscription],
        }
    }

    fn input_edited(&mut self, cx: &mut Context<Self>) {
        if self.syncing_input {
            return;
        }
        let text = self.input.read(cx).text(cx);
        let text = text.trim();
        let parsed = color_literals(text)
            .into_iter()
            .find(|literal| literal.range == (0, text.len()));
        self.input_invalid = parsed.is_none();
        if let Some(parsed) = parsed {
            let mut color = Hsva::from_rgba(parsed.color);
            // Keep the hue the user had when the typed color is a gray, which
            // has no hue of its own.
            if color.s == 0. || color.v == 0. {
                color.h = self.color.h;
            }
            self.color = color;
        }
        cx.notify();
    }

    fn set_color(&mut self, color: Hsva, window: &mut Window, cx: &mut Context<Self>) {
        self.color = color;
        self.input_invalid = false;
        let text = self.literal.format(color.to_rgba());
        self.syncing_input = true;
        self.input
            .update(cx, |input, cx| input.set_text(text, window, cx));
        self.syncing_input = false;
        cx.notify();
    }

    fn set_from_position(
        &mut self,
        channel: ColorChannel,
        position: PixelPoint<Pixels>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fraction = |offset: Pixels, length: Pixels| {
            if length <= px(0.) {
                0.
            } else {
                (offset / length).clamp(0., 1.)
            }
        };
        let x = fraction(position.x - bounds.left(), bounds.size.width);
        let y = fraction(position.y - bounds.top(), bounds.size.height);
        let mut color = self.color;
        match channel {
            ColorChannel::SaturationValue => {
                color.s = x;
                color.v = 1. - y;
            }
            // Stop just short of 1.0 so the far end of the bar stays red at
            // the end rather than wrapping back to the start.
            ColorChannel::Hue => color.h = x.min(0.9999),
            ColorChannel::Alpha => color.a = x,
        }
        self.set_color(color, window, cx);
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        if self.input_invalid {
            return;
        }
        let text = self.literal.format(self.color.to_rgba());
        let range = self.range.clone();
        self.editor
            .update(cx, |editor, cx| editor.edit([(range, text)], cx))
            .ok();
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn render_track(
        &self,
        channel: ColorChannel,
        bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let id = match channel {
            ColorChannel::SaturationValue => "color-picker-saturation-value",
            ColorChannel::Hue => "color-picker-hue",
            ColorChannel::Alpha => "color-picker-alpha",
        };
        div()
            .id(id)
            .relative()
            .w_full()
            .rounded_sm()
            .border_1()
            .border_color(cx.theme().colors().border)
            .cursor_crosshair()
            .child(
                canvas(
                    {
                        let bounds = bounds.clone();
                        move |element_bounds, _, _| bounds.set(Some(element_bounds))
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    if let Some(bounds) = bounds.get() {
                        this.set_from_position(channel, event.position, bounds, window, cx);
                    }
                }),
            )
            .on_drag(ColorDrag(channel), |_, _, _, cx| cx.new(|_| Empty))
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<ColorDrag>, window, cx| {
                    if event.drag(cx).0 == channel {
                        this.set_from_position(
                            channel,
                            event.event.position,
                            event.bounds,
                            window,
                            cx,
                        );
                    }
                }),
            )
    }
}

fn thumb(color: Hsla) -> Div {
    div()
        .absolute()
        .size_3()
        .rounded_full()
        .border_2()
        .border_color(gpui::white())
        .shadow_sm()
        .bg(color)
}

fn swatch(color: Rgba, cx: &App) -> Div {
    // A light backdrop so a translucent color still reads as translucent.
    div()
        .flex_1()
        .h_8()
        .bg(gpui::rgb(0xcccccc))
        .child(div().size_full().bg(color))
        .border_1()
        .border_color(cx.theme().colors().border)
}

impl Render for ColorPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let color = self.color.to_rgba();
        let pure_hue = Hsva {
            h: self.color.h,
            s: 1.,
            v: 1.,
            a: 1.,
        }
        .to_rgba();
        let opaque = self.color.with_alpha(1.);
        let thumb_inset = rems(0.375);

        let saturation_value = self
            .render_track(
                ColorChannel::SaturationValue,
                self.saturation_value_bounds.clone(),
                cx,
            )
            .h(rems(10.))
            .bg(linear_gradient(
                90.,
                linear_color_stop(gpui::white(), 0.),
                linear_color_stop(pure_hue, 1.),
            ))
            .child(div().absolute().size_full().bg(linear_gradient(
                180.,
                linear_color_stop(gpui::transparent_black(), 0.),
                linear_color_stop(gpui::black(), 1.),
            )))
            .child(
                thumb(opaque.into())
                    .left(relative(self.color.s))
                    .top(relative(1. - self.color.v))
                    .ml(-thumb_inset)
                    .mt(-thumb_inset),
            );

        let hue_stops: Vec<Rgba> = (0..=6)
            .map(|index| {
                Hsva {
                    h: index as f32 / 6.,
                    s: 1.,
                    v: 1.,
                    a: 1.,
                }
                .to_rgba()
            })
            .collect();
        let hue = self
            .render_track(ColorChannel::Hue, self.hue_bounds.clone(), cx)
            .h_4()
            .child(
                h_flex()
                    .size_full()
                    .children(hue_stops.windows(2).map(|pair| {
                        div().h_full().flex_1().bg(linear_gradient(
                            90.,
                            linear_color_stop(pair[0], 0.),
                            linear_color_stop(pair[1], 1.),
                        ))
                    })),
            )
            .child(
                thumb(pure_hue.into())
                    .left(relative(self.color.h))
                    .top(px(1.))
                    .ml(-thumb_inset),
            );

        let alpha = self
            .render_track(ColorChannel::Alpha, self.alpha_bounds.clone(), cx)
            .h_4()
            .bg(gpui::rgb(0xcccccc))
            .child(div().absolute().size_full().bg(linear_gradient(
                90.,
                linear_color_stop(self.color.with_alpha(0.), 0.),
                linear_color_stop(opaque, 1.),
            )))
            .child(
                thumb(color.into())
                    .left(relative(self.color.a))
                    .top(px(1.))
                    .ml(-thumb_inset),
            );

        let original = self.literal.color;
        v_flex()
            .key_context("ColorPicker")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .w(rems(20.))
            .elevation_3(cx)
            .p_3()
            .gap_3()
            .child(
                h_flex()
                    .justify_between()
                    .child(Label::new("Pick Color"))
                    .child(
                        Label::new("Enter to apply, Escape to cancel")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(saturation_value)
            .child(hue)
            .child(alpha)
            .child(
                h_flex()
                    .gap_2()
                    .child(swatch(original, cx))
                    .child(swatch(color, cx)),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(if self.input_invalid {
                        cx.theme().status().error_border
                    } else {
                        cx.theme().colors().border
                    })
                    .child(self.input.clone()),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("color-picker-cancel", "Cancel").on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&menu::Cancel, window, cx)),
                    ))
                    .child(
                        Button::new("color-picker-apply", "Apply")
                            .style(ButtonStyle::Filled)
                            .disabled(self.input_invalid)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx)
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_tests::init_test;
    use gpui::{TestAppContext, VisualTestContext};
    use project::{FakeFs, Project};
    use util::{path, rel_path::rel_path};
    use workspace::MultiWorkspace;

    /// What clicking a swatch does: open the picker on the color the swatch
    /// sits in front of, wherever the cursor happens to be.
    #[gpui::test]
    async fn pick_color_at_opens_the_picker_for_that_color(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/a"),
            serde_json::json!({ "style.css": "a { color: #ff0000; }\nb { color: red; }\n" }),
        )
        .await;
        let project = Project::test(fs, [path!("/a").as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let worktree_id = workspace.update_in(cx, |workspace, _, cx| {
            workspace.project().update(cx, |project, cx| {
                project.worktrees(cx).next().unwrap().read(cx).id()
            })
        });
        let editor = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_path((worktree_id, rel_path("style.css")), None, true, window, cx)
            })
            .await
            .unwrap()
            .downcast::<Editor>()
            .unwrap();

        // The cursor is on the second line, away from any color.
        editor.update_in(cx, |editor, window, cx| {
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_ranges([Point::new(1, 0)..Point::new(1, 0)])
            });
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let swatch = snapshot.anchor_before(Point::new(0, 11));
            editor.pick_color_at(swatch, window, cx);
        });
        let picker = workspace.update(cx, |workspace, cx| {
            workspace.active_modal::<ColorPicker>(cx)
        });
        let picker = picker.expect("the picker opens");
        picker.read_with(cx, |picker, _| {
            assert_eq!(to_bytes(picker.literal.color), [255, 0, 0, 255]);
        });

        // Nothing opens where there is no color.
        cx.dispatch_action(menu::Cancel);
        editor.update_in(cx, |editor, window, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let not_a_color = snapshot.anchor_before(Point::new(1, 11));
            editor.pick_color_at(not_a_color, window, cx);
        });
        let picker = workspace.update(cx, |workspace, cx| {
            workspace.active_modal::<ColorPicker>(cx)
        });
        assert!(picker.is_none());
    }

    fn literal(text: &str) -> ColorLiteral {
        let literals = color_literals(text);
        assert_eq!(literals.len(), 1, "expected one literal in {text:?}");
        literals[0]
    }

    fn round_trip(text: &str) -> String {
        let literal = literal(text);
        literal.format(literal.color)
    }

    #[test]
    fn finds_literals_and_skips_lookalikes() {
        let line = r##"color: #fff; bg: rgba(0, 0, 0, 0.5); gpui::rgb(0x1e1e2e); id = 0x1234;"##;
        let found: Vec<&str> = color_literals(line)
            .iter()
            .map(|literal| &line[literal.range.0..literal.range.1])
            .collect();
        assert_eq!(found, ["#fff", "rgba(0, 0, 0, 0.5)", "0x1e1e2e"]);

        assert!(color_literals("&#123; #12345 r#add a0x112233 #ffffffz").is_empty());
        assert!(color_literals("myrgb(1, 2, 3) rgb(1, 2)").is_empty());
    }

    #[test]
    fn cursor_just_after_a_literal_still_finds_it() {
        let line = "a: #ff0000;";
        assert!(color_literal_at(line, 3).is_some());
        assert!(color_literal_at(line, 10).is_some());
        assert!(color_literal_at(line, 11).is_none());
        assert!(color_literal_at(line, 1).is_none());
    }

    #[test]
    fn formats_preserve_the_original_notation() {
        for text in [
            "#fff",
            "#ABCD",
            "#1e1e2e",
            "#1E1E2E80",
            "0x1e1e2e",
            "0X1E1E2EFF",
            "rgb(30, 30, 46)",
            "rgba(30, 30, 46, 0.5)",
            "rgb(30 30 46 / 50%)",
            "hsl(240deg 21% 15%)",
            "HSLA(0, 100%, 50%, 0.25)",
        ] {
            assert_eq!(round_trip(text), text);
        }
    }

    #[test]
    fn alpha_is_added_only_when_needed() {
        let translucent = Rgba {
            r: 1.,
            g: 0.,
            b: 0.,
            a: 0.5,
        };
        assert_eq!(literal("#f00").format(translucent), "#ff000080");
        assert_eq!(literal("0xff0000").format(translucent), "0xff000080");
        assert_eq!(
            literal("rgb(255, 0, 0)").format(translucent),
            "rgba(255, 0, 0, 0.5)"
        );
        assert_eq!(
            literal("rgb(255 0 0)").format(translucent),
            "rgb(255 0 0 / 0.5)"
        );
        assert_eq!(
            literal("#123").format(literal("#112233").color),
            "#123",
            "a color expressible in short hex keeps the short form"
        );
    }

    #[test]
    fn hsv_round_trips_through_rgb() {
        for text in ["#ff0000", "#1e1e2e", "#89b4fa", "#000000", "#ffffff"] {
            let color = literal(text).color;
            let back = Hsva::from_rgba(color).to_rgba();
            assert_eq!(to_bytes(back), to_bytes(color), "{text}");
        }
    }
}
