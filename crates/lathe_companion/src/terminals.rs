//! Terminals for paired phones: the terminals in open workspaces (above all
//! CLI agents in the agent panel's terminal threads), their screens, and
//! typing into them.

use crate::{
    CompanionSettings,
    bridge::{open_workspace_paths, workspace_id},
    crypto::{encode_url_safe, fill_random},
    protocol::{
        DesktopMessage, ErrorBody, ErrorCode, TerminalColors, TerminalCursor, TerminalKey,
        TerminalKind, TerminalStyle, TerminalSummary,
    },
};
use agent_ui::AgentPanel;
use collections::HashMap;
use gpui::{
    App, AppContext as _, Context, Entity, EntityId, EventEmitter, Global, Hsla, Keystroke, Rgba,
    SharedString,
    Subscription, Task, WeakEntity,
};
use settings::Settings as _;
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::time::Duration;
use terminal::{
    Cell, Color, CursorShape, NamedColor, Terminal, is_app_chosen_exact_color,
    is_default_background_color, terminal_settings::TerminalSettings,
};
use terminal_view::{TerminalView, terminal_element::convert_color};
use ui::{ActiveTheme as _, utils::ensure_minimum_contrast};
use util::ResultExt as _;
use workspace::item::Item as _;

/// Screens of a busy terminal go out at most this often.
const SCREEN_INTERVAL: Duration = Duration::from_millis(200);
/// List changes are batched, since opening a workspace adds many at once.
const LIST_INTERVAL: Duration = Duration::from_millis(500);
/// The most scrollback a screen carries, so it stays well inside a frame.
const MAX_SCROLLBACK_LINES: usize = 500;
const MAX_SCROLLBACK_BYTES: usize = 64 * 1024;
/// The most text one `terminal_input` may type.
pub const MAX_INPUT_BYTES: usize = 4096;

pub(crate) fn init(cx: &mut App) {
    let terminals = cx.new(|_| Terminals::default());
    cx.set_global(GlobalTerminals(terminals.clone()));
    cx.observe_new(move |_: &mut TerminalView, _, cx: &mut Context<TerminalView>| {
        let view = cx.entity();
        let terminals = terminals.clone();
        // The view is still being built; reading it waits for that to finish.
        App::defer(cx, move |cx| {
            terminals.update(cx, |terminals, cx| terminals.track(view, cx));
        });
    })
    .detach();
}

struct GlobalTerminals(Entity<Terminals>);

impl Global for GlobalTerminals {}

#[derive(Clone, Debug, PartialEq)]
pub enum TerminalsEvent {
    /// Something in the list changed; resend it.
    ListChanged,
    /// A screen for the phones following this terminal.
    Screen {
        terminal_id: String,
        message: DesktopMessage,
    },
    /// The terminal started waiting for input.
    AwaitingInput { terminal_id: String },
}

#[derive(Default)]
pub struct Terminals {
    tracked: HashMap<EntityId, Tracked>,
    /// Terminals at least one phone follows, by id.
    watched: HashMap<String, Watch>,
    list_flush: Option<Task<()>>,
}

struct Tracked {
    /// Random and unguessable, for the life of the view.
    id: String,
    view: WeakEntity<TerminalView>,
    awaiting_input: bool,
    title: String,
    _subscriptions: [Subscription; 3],
}

#[derive(Default)]
struct Watch {
    seq: u64,
    flush: Option<Task<()>>,
    /// Hash of the scrollback last sent. `None` sends it with the next screen.
    scrollback: Option<u64>,
}

impl EventEmitter<TerminalsEvent> for Terminals {}

impl Terminals {
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalTerminals>()
            .map(|global| global.0.clone())
    }

    fn track(&mut self, view: Entity<TerminalView>, cx: &mut Context<Self>) {
        let entity_id = view.entity_id();
        if self.tracked.contains_key(&entity_id) {
            return;
        }
        let terminal = view.read(cx).terminal().clone();
        let mut id = [0u8; 12];
        fill_random(&mut id);
        let subscriptions = [
            // The view notifies when its waiting state or title may have moved.
            cx.observe(&view, move |this, view, cx| this.view_changed(&view, cx)),
            cx.subscribe(&terminal, move |this, _, event, cx| {
                if matches!(event, terminal::Event::Wakeup | terminal::Event::TitleChanged) {
                    this.output(entity_id, cx);
                }
            }),
            cx.observe_release(&view, move |this, _, cx| {
                if let Some(tracked) = this.tracked.remove(&entity_id) {
                    this.watched.remove(&tracked.id);
                    this.schedule_list(cx);
                }
            }),
        ];
        let awaiting_input = view.read(cx).is_awaiting_input(cx);
        let title = shown_title(&view, cx);
        self.tracked.insert(
            entity_id,
            Tracked {
                id: encode_url_safe(&id),
                view: view.downgrade(),
                awaiting_input,
                title,
                _subscriptions: subscriptions,
            },
        );
        self.schedule_list(cx);
    }

    fn view_changed(&mut self, view: &Entity<TerminalView>, cx: &mut Context<Self>) {
        let Some(tracked) = self.tracked.get_mut(&view.entity_id()) else {
            return;
        };
        let awaiting_input = view.read(cx).is_awaiting_input(cx);
        let title = shown_title(view, cx);
        if awaiting_input == tracked.awaiting_input && title == tracked.title {
            return;
        }
        let started_waiting = awaiting_input && !tracked.awaiting_input;
        tracked.awaiting_input = awaiting_input;
        tracked.title = title;
        let terminal_id = tracked.id.clone();
        if started_waiting {
            cx.emit(TerminalsEvent::AwaitingInput { terminal_id });
        }
        self.schedule_list(cx);
    }

    fn output(&mut self, entity_id: EntityId, cx: &mut Context<Self>) {
        let Some(id) = self.tracked.get(&entity_id).map(|tracked| tracked.id.clone()) else {
            return;
        };
        let Some(watch) = self.watched.get_mut(&id) else {
            return;
        };
        if watch.flush.is_some() {
            return;
        }
        watch.flush = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SCREEN_INTERVAL).await;
            this.update(cx, |this, cx| {
                if let Some(watch) = this.watched.get_mut(&id) {
                    watch.flush = None;
                }
                this.send_screen(&id, cx);
            })
            .log_err();
        }));
    }

    fn schedule_list(&mut self, cx: &mut Context<Self>) {
        if self.list_flush.is_some() {
            return;
        }
        self.list_flush = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LIST_INTERVAL).await;
            this.update(cx, |this, cx| {
                this.list_flush = None;
                cx.emit(TerminalsEvent::ListChanged);
            })
            .log_err();
        }));
    }

    /// Starts screens for a terminal, beginning with the current one.
    pub fn watch(&mut self, terminal_id: &str, cx: &mut Context<Self>) -> Result<(), ErrorBody> {
        self.view(terminal_id, cx).ok_or_else(not_found)?;
        // The new follower's first screen carries the scrollback.
        self.watched.entry(terminal_id.to_string()).or_default().scrollback = None;
        self.send_screen(terminal_id, cx);
        Ok(())
    }

    pub fn unwatch(&mut self, terminal_id: &str) {
        self.watched.remove(terminal_id);
    }

    fn send_screen(&mut self, terminal_id: &str, cx: &mut Context<Self>) {
        let Some(view) = self.view(terminal_id, cx) else {
            return;
        };
        let Some(watch) = self.watched.get_mut(terminal_id) else {
            return;
        };
        watch.seq += 1;
        let theme = cx.theme().clone();
        let minimum_contrast = TerminalSettings::get_global(cx).minimum_contrast;
        let terminal = view.read(cx).terminal().read(cx);
        let color = |color: &Color| convert_color(color, &theme);
        let mut styler = Styler::new(&color, minimum_contrast);
        let mut message = screen(terminal_id, watch.seq, terminal, Some(&mut styler));
        let (history, history_spans) = scrollback(terminal, &mut styler);
        // Hashed with the styles the spans point to, since indexes differ per message.
        let mut hasher = DefaultHasher::new();
        history.hash(&mut hasher);
        for spans in &history_spans {
            for span in spans.chunks(3) {
                (span[0], span[1], &styler.styles[span[2] as usize]).hash(&mut hasher);
            }
        }
        let hash = hasher.finish();
        let send_history = watch.scrollback != Some(hash);
        watch.scrollback = Some(hash);
        if let DesktopMessage::TerminalScreen { scrollback, scrollback_styles, styles, colors, .. } = &mut message {
            if send_history {
                if history_spans.iter().any(|spans| !spans.is_empty()) {
                    *scrollback_styles = Some(history_spans);
                }
                *scrollback = Some(history);
            }
            *colors = Some(styler.colors());
            *styles = std::mem::take(&mut styler.styles);
        }
        cx.emit(TerminalsEvent::Screen {
            terminal_id: terminal_id.to_string(),
            message,
        });
    }

    fn view(&self, terminal_id: &str, _cx: &App) -> Option<Entity<TerminalView>> {
        self.tracked
            .values()
            .find(|tracked| tracked.id == terminal_id)
            .and_then(|tracked| tracked.view.upgrade())
    }

    /// Renames a terminal as renaming it in Lathe does: an agent panel
    /// terminal thread through the panel, any other through its tab.
    pub fn rename(&mut self, terminal_id: &str, title: &str, cx: &mut Context<Self>) -> Result<(), ErrorBody> {
        let view = self.view(terminal_id, cx).ok_or_else(not_found)?;
        let title = SharedString::from(title.to_string());
        let panels = open_workspace_paths(cx)
            .into_iter()
            .filter_map(|(_, workspace, _)| workspace.upgrade()?.read(cx).panel::<AgentPanel>(cx))
            .collect::<Vec<_>>();
        for panel in panels {
            if panel.update(cx, |panel, cx| panel.rename_terminal_thread(&view, title.clone(), cx)) {
                return Ok(());
            }
        }
        view.update(cx, |view, cx| view.set_custom_title(Some(title.to_string()), cx));
        Ok(())
    }

    /// Types text or presses a key in a terminal, if Lathe allows it.
    pub fn input(
        &mut self,
        terminal_id: &str,
        text: Option<String>,
        key: Option<TerminalKey>,
        cx: &mut Context<Self>,
    ) -> Result<(), ErrorBody> {
        if !CompanionSettings::get_global(cx).terminal_input {
            return Err(ErrorBody::new(
                ErrorCode::TerminalInputDisabled,
                "Typing into terminals from the phone is turned off in Lathe's companion settings.",
            ));
        }
        let view = self.view(terminal_id, cx).ok_or_else(not_found)?;
        let terminal = view.read(cx).terminal().clone();
        let result = match (text, key) {
            (Some(text), None) if !text.is_empty() && text.len() <= MAX_INPUT_BYTES => {
                terminal.update(cx, |terminal, _| terminal.input(text.into_bytes()));
                Ok(())
            }
            (None, Some(key)) => {
                let keystroke = Keystroke::parse(key.keystroke()).map_err(|_| bad_input())?;
                let handled = terminal.update(cx, |terminal, _| terminal.try_keystroke(&keystroke, false));
                if handled { Ok(()) } else { Err(bad_input()) }
            }
            _ => Err(bad_input()),
        };
        // Answered from the phone: the "needs input" marker goes, in Lathe and
        // (through the view's notify) in the phone's list.
        if result.is_ok() {
            view.update(cx, |view, cx| view.note_remote_input(cx));
        }
        result
    }

    /// The tracked terminals that live in open workspaces, with the workspace
    /// each is in and where.
    pub fn list(&self, cx: &App) -> Vec<TerminalSummary> {
        let mut placed: HashMap<EntityId, (String, TerminalKind, Option<String>)> =
            HashMap::default();
        for (paths, workspace, _) in open_workspace_paths(cx) {
            let Some(workspace) = workspace.upgrade() else {
                continue;
            };
            let workspace_id = workspace_id(&paths);
            let workspace = workspace.read(cx);
            if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                for (view, title) in panel.read(cx).terminal_threads(cx) {
                    placed.insert(
                        view.entity_id(),
                        (workspace_id.clone(), TerminalKind::Agent, Some(title.to_string())),
                    );
                }
            }
            let docked = workspace
                .all_docks()
                .into_iter()
                .flat_map(|dock| dock.read(cx).panel_panes(cx));
            let center = workspace.panes().iter().cloned();
            for (pane, kind) in docked
                .map(|pane| (pane, TerminalKind::Panel))
                .chain(center.map(|pane| (pane, TerminalKind::Editor)))
            {
                for item in pane.read(cx).items() {
                    if let Some(view) = item.downcast::<TerminalView>() {
                        placed
                            .entry(view.entity_id())
                            .or_insert((workspace_id.clone(), kind, None));
                    }
                }
            }
        }
        let mut list = self
            .tracked
            .iter()
            .filter_map(|(entity_id, tracked)| {
                let (workspace_id, kind, title) = placed.get(entity_id)?.clone();
                let view = tracked.view.upgrade()?;
                Some(TerminalSummary {
                    terminal_id: tracked.id.clone(),
                    workspace_id,
                    title: title
                        .filter(|title| !title.is_empty())
                        .unwrap_or_else(|| tracked.title.clone()),
                    kind,
                    awaiting_input: tracked.awaiting_input,
                    exited: view.read(cx).terminal().read(cx).pid().is_none(),
                })
            })
            .collect::<Vec<_>>();
        list.sort_by(|left, right| {
            (left.workspace_id.as_str(), left.kind as u8, left.title.as_str()).cmp(&(
                right.workspace_id.as_str(),
                right.kind as u8,
                right.title.as_str(),
            ))
        });
        list
    }

}

/// The name Lathe shows on a terminal's tab: the one it was given, or else
/// the terminal's own title.
fn shown_title(view: &Entity<TerminalView>, cx: &App) -> String {
    let view = view.read(cx);
    view.custom_title()
        .map(str::to_string)
        .unwrap_or_else(|| view.terminal().read(cx).title(true))
}

fn not_found() -> ErrorBody {
    ErrorBody::new(ErrorCode::TerminalNotFound, "That terminal closed.")
}

fn bad_input() -> ErrorBody {
    ErrorBody::new(
        ErrorCode::BadMessage,
        format!("Send either some text (at most {MAX_INPUT_BYTES} bytes) or one key."),
    )
}

/// Turns terminal cells into a line of text and its styled spans, in the
/// colors Lathe's terminal draws them with, collecting the distinct styles.
pub(crate) struct Styler<'a> {
    /// Lathe's mapping from a terminal color to a theme color.
    color: &'a dyn Fn(&Color) -> Hsla,
    minimum_contrast: f32,
    foreground: String,
    pub(crate) styles: Vec<TerminalStyle>,
    index: HashMap<TerminalStyle, u32>,
}

impl<'a> Styler<'a> {
    pub(crate) fn new(color: &'a dyn Fn(&Color) -> Hsla, minimum_contrast: f32) -> Self {
        let foreground = hex(color(&Color::Named(NamedColor::Foreground)));
        Self { color, minimum_contrast, foreground, styles: Vec::new(), index: HashMap::default() }
    }

    pub(crate) fn colors(&self) -> TerminalColors {
        TerminalColors {
            foreground: self.foreground.clone(),
            background: hex((self.color)(&Color::Named(NamedColor::Background))),
        }
    }

    /// The style of one cell, as `TerminalElement::cell_style` draws it.
    /// `None` for the terminal's defaults. Without `text_color` the cell's
    /// text color is left out, as a space shows none.
    fn style_of(&self, cell: &Cell, text_color: bool) -> Option<TerminalStyle> {
        let mut fg = cell.foreground();
        let mut bg = cell.background();
        if cell.is_inverse() {
            std::mem::swap(&mut fg, &mut bg);
        }
        let bg_color = (self.color)(&bg);
        let mut fg_color = (self.color)(&fg);
        if !is_app_chosen_exact_color(fg) {
            fg_color = ensure_minimum_contrast(fg_color, bg_color, self.minimum_contrast);
        }
        if cell.is_dim() {
            fg_color.a *= 0.7;
        }
        let fg = hex(fg_color);
        let style = TerminalStyle {
            fg: (text_color && fg != self.foreground).then_some(fg),
            bg: (!is_default_background_color(bg)).then(|| hex(bg_color)),
            bold: cell.is_bold(),
            italic: cell.is_italic(),
            underline: cell.has_underline(),
            strikethrough: cell.has_strikeout(),
        };
        (style != TerminalStyle::default()).then_some(style)
    }

    /// One row as text, trailing spaces trimmed and wide characters once, and
    /// its spans as flat `[start, length, style]` triples in characters.
    pub(crate) fn row<'c>(&mut self, cells: impl IntoIterator<Item = &'c Cell>) -> (String, Vec<u32>) {
        let mut text = String::new();
        let mut spans: Vec<u32> = Vec::new();
        let mut at = 0u32;
        for cell in cells {
            // The second column of a wide character is a spacer; keeping it
            // would split "你好" into "你 好".
            if cell.is_wide_char_spacer() {
                continue;
            }
            text.push(cell.character());
            let mut width = 1;
            if let Some(zerowidth) = cell.zerowidth() {
                text.extend(zerowidth);
                width += zerowidth.len() as u32;
            }
            // A space continues the run before it if it would look the same
            // there; otherwise its text color doesn't matter.
            let full = self.style_of(cell, true);
            let continues = |spans: &[u32], styles: &[TerminalStyle]| match spans {
                [.., start, length, index] => {
                    start + length == at && Some(&styles[*index as usize]) == full.as_ref()
                }
                _ => false,
            };
            let style = if cell.character() == ' ' && !continues(&spans, &self.styles) {
                self.style_of(cell, false)
            } else {
                full
            };
            if let Some(style) = style {
                let index = self.intern(style);
                match spans.len() {
                    len if len >= 3 && spans[len - 1] == index && spans[len - 3] + spans[len - 2] == at => {
                        spans[len - 2] += width;
                    }
                    _ => spans.extend([at, width, index]),
                }
            }
            at += width;
        }
        let trimmed = text.trim_end().chars().count() as u32;
        text.truncate(text.trim_end().len());
        let mut kept = Vec::with_capacity(spans.len());
        for span in spans.chunks(3) {
            let end = (span[0] + span[1]).min(trimmed);
            if end > span[0] {
                kept.extend([span[0], end - span[0], span[2]]);
            }
        }
        (text, kept)
    }

    fn intern(&mut self, style: TerminalStyle) -> u32 {
        if let Some(index) = self.index.get(&style) {
            return *index;
        }
        let index = self.styles.len() as u32;
        self.styles.push(style.clone());
        self.index.insert(style, index);
        index
    }
}

fn hex(color: Hsla) -> String {
    let rgba = Rgba::from(color);
    let byte = |value: f32| (value.clamp(0., 1.) * 255.).round() as u8;
    let mut out = format!("#{:02x}{:02x}{:02x}", byte(rgba.r), byte(rgba.g), byte(rgba.b));
    if rgba.a < 0.995 {
        out.push_str(&format!("{:02x}", byte(rgba.a)));
    }
    out
}

/// The visible screen: one line per row, with its styled spans when a
/// `styler` is given.
pub(crate) fn screen(
    terminal_id: &str,
    seq: u64,
    terminal: &Terminal,
    styler: Option<&mut Styler>,
) -> DesktopMessage {
    // Read live: a terminal that isn't on screen never refreshes `last_content`.
    let content = terminal.current_content();
    let rows = content.screen_lines;
    let columns = content.columns;
    let offset = content.display_offset as i32;
    let blank = Cell::default();
    let mut grid: Vec<Vec<&Cell>> = vec![vec![&blank; columns]; rows];
    for cell in &content.cells {
        let row = cell.point.line + offset;
        let column = cell.point.column;
        if row < 0 || row as usize >= rows || column >= columns {
            continue;
        }
        grid[row as usize][column] = cell;
    }
    let mut own = Styler::new(&|_| Hsla::default(), 0.);
    let styled = styler.is_some();
    let styler = styler.unwrap_or(&mut own);
    let (lines, spans): (Vec<String>, Vec<Vec<u32>>) =
        grid.into_iter().map(|row| styler.row(row)).unzip();
    let line_styles = (styled && spans.iter().any(|spans| !spans.is_empty())).then_some(spans);
    let cursor_row = content.cursor.point.line + offset;
    let cursor = (content.cursor.shape != CursorShape::Hidden
        && cursor_row >= 0
        && (cursor_row as usize) < rows)
        .then(|| TerminalCursor {
            row: cursor_row as u16,
            column: content.cursor.point.column.min(columns.saturating_sub(1)) as u16,
        });
    DesktopMessage::TerminalScreen {
        terminal_id: terminal_id.to_string(),
        seq,
        columns: columns as u16,
        rows: rows as u16,
        lines,
        cursor,
        exited: terminal.pid().is_none(),
        scrollback: None,
        styles: Vec::new(),
        line_styles,
        scrollback_styles: None,
        colors: None,
    }
}

/// The scrollback, oldest first, within the line and byte limits, and its
/// spans. Rows are counted from the newest, so the limits drop the oldest.
pub(crate) fn scrollback(terminal: &Terminal, styler: &mut Styler) -> (Vec<String>, Vec<Vec<u32>>) {
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut bytes = 0;
    for row in terminal.scrollback_rows(MAX_SCROLLBACK_LINES) {
        let (text, row_spans) = styler.row(&row);
        // About what a triple costs in JSON.
        bytes += text.len() + 1 + row_spans.len() * 4;
        if bytes > MAX_SCROLLBACK_BYTES {
            break;
        }
        lines.push(text);
        spans.push(row_spans);
    }
    lines.reverse();
    spans.reverse();
    (lines, spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use pretty_assertions::assert_eq;
    use settings::SettingsStore;
    use terminal::{
        TerminalBuilder,
        terminal_settings::{AlternateScroll, CursorShape as SettingsCursorShape},
    };
    use util::paths::PathStyle;

    fn test_terminal(cx: &mut TestAppContext) -> Entity<Terminal> {
        cx.new(|cx| {
            TerminalBuilder::new_display_only(
                SettingsCursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        })
    }

    fn render(terminal: &Entity<Terminal>, cx: &mut TestAppContext) -> DesktopMessage {
        terminal.read_with(cx, |terminal, _| screen("t1", 3, terminal, None))
    }

    /// A stand-in for the theme: red, green and blue for those, white text on black.
    fn test_color(color: &Color) -> Hsla {
        match color {
            Color::Named(NamedColor::Red) => gpui::rgb(0xff0000).into(),
            Color::Named(NamedColor::Green) => gpui::rgb(0x00ff00).into(),
            Color::Named(NamedColor::Blue) => gpui::rgb(0x0000ff).into(),
            Color::Named(NamedColor::Background) => gpui::rgb(0x000000).into(),
            _ => gpui::rgb(0xffffff).into(),
        }
    }

    fn lines(message: &DesktopMessage) -> Vec<String> {
        let DesktopMessage::TerminalScreen { lines, .. } = message else { panic!() };
        lines.iter().filter(|line| !line.is_empty()).cloned().collect()
    }

    #[gpui::test]
    fn renders_the_screen_as_text(cx: &mut TestAppContext) {
        let terminal = test_terminal(cx);
        // Written straight to the grid; nothing ever draws this terminal,
        // like a CLI thread in the background on the desk.
        terminal.update(cx, |terminal, cx| {
            terminal.write_output(b"$ claude\r\nDo you want to proceed?\r\n\x1b[1m> 1. Yes\x1b[0m   \r\n  2. No\r\n\xe4\xbd\xa0\xe5\xa5\xbd ok", cx)
        });
        let message = render(&terminal, cx);
        assert_eq!(
            lines(&message),
            vec!["$ claude", "Do you want to proceed?", "> 1. Yes", "  2. No", "你好 ok"]
        );
        let DesktopMessage::TerminalScreen { seq, rows, columns, cursor, terminal_id, .. } = &message else {
            panic!()
        };
        assert_eq!((terminal_id.as_str(), *seq), ("t1", 3));
        assert!(*rows > 0 && *columns > 0);
        // After `你好 ok`: two wide characters take four columns.
        assert_eq!(*cursor, Some(TerminalCursor { row: 4, column: 7 }));
    }

    #[gpui::test]
    fn renders_full_screen_programs(cx: &mut TestAppContext) {
        let terminal = test_terminal(cx);
        terminal.update(cx, |terminal, cx| {
            terminal.write_output(b"shell history\r\n", cx);
            // Switch to the alternate screen, as `top` or a TUI does, clear
            // it, draw at row 3, and hide the cursor.
            terminal.write_raw_output(b"\x1b[?1049h\x1b[2J\x1b[3;5HTUI view\x1b[?25l", cx);
        });
        let message = render(&terminal, cx);
        assert_eq!(lines(&message), vec!["    TUI view"]);
        let DesktopMessage::TerminalScreen { lines: all, cursor, .. } = &message else { panic!() };
        assert_eq!(all[2], "    TUI view");
        assert_eq!(*cursor, None);
    }

    #[gpui::test]
    fn reads_the_lines_above_the_screen(cx: &mut TestAppContext) {
        let terminal = test_terminal(cx);
        let output: String = (1..=200).map(|n| format!("line {n}\r\n")).collect();
        terminal.update(cx, |terminal, cx| terminal.write_output(output.as_bytes(), cx));
        let (rows, history) = terminal.read_with(cx, |terminal, _| {
            let DesktopMessage::TerminalScreen { rows, .. } = screen("t1", 1, terminal, None) else {
                panic!()
            };
            let mut styler = Styler::new(&test_color, 0.);
            (rows as usize, scrollback(terminal, &mut styler).0)
        });
        // Scrollback and screen together hold every line, oldest first, with nothing repeated.
        let screen_lines = lines(&render(&terminal, cx));
        let all: Vec<String> = history.iter().cloned().chain(screen_lines).collect();
        assert_eq!(all, (1..=200).map(|n| format!("line {n}")).collect::<Vec<_>>());
        assert_eq!(history.len(), 200 + 1 - rows);
    }

    #[gpui::test]
    fn sends_colors_as_styled_spans(cx: &mut TestAppContext) {
        let terminal = test_terminal(cx);
        let output: String = (1..=60).map(|n| format!("\x1b[31mold {n}\x1b[0m\r\n")).collect();
        terminal.update(cx, |terminal, cx| {
            terminal.write_output(output.as_bytes(), cx);
            // Plain, then green, bold blue, reversed (black on white), and an
            // underlined space, which is trailing whitespace and gets trimmed.
            terminal.write_output(b"ok \x1b[32m+add\x1b[0m \x1b[1;34mB\x1b[0m \x1b[7mR\x1b[0m\x1b[4m \x1b[0m", cx);
        });
        let (message, history, history_spans, styles) = terminal.read_with(cx, |terminal, _| {
            let mut styler = Styler::new(&test_color, 0.);
            let message = screen("t1", 1, terminal, Some(&mut styler));
            let (history, spans) = scrollback(terminal, &mut styler);
            (message, history, spans, styler.styles)
        });
        let DesktopMessage::TerminalScreen { lines, line_styles, cursor, .. } = &message else {
            panic!()
        };
        let row = cursor.unwrap().row as usize;
        assert_eq!(lines[row], "ok +add B R");
        let spans = &line_styles.as_ref().unwrap()[row];
        let runs: Vec<(u32, u32, TerminalStyle)> = spans
            .chunks(3)
            .map(|span| (span[0], span[1], styles[span[2] as usize].clone()))
            .collect();
        let green = TerminalStyle { fg: Some("#00ff00".into()), ..Default::default() };
        let bold_blue = TerminalStyle { fg: Some("#0000ff".into()), bold: true, ..Default::default() };
        let reversed = TerminalStyle { fg: Some("#000000".into()), bg: Some("#ffffff".into()), ..Default::default() };
        assert_eq!(runs, vec![(3, 4, green), (8, 1, bold_blue), (10, 1, reversed)]);
        // Scrollback carries its colors too: each red line is one span.
        let red = TerminalStyle { fg: Some("#ff0000".into()), ..Default::default() };
        let first = history.iter().position(|line| line == "old 1").unwrap();
        assert_eq!(history_spans[first][..2], [0, 5]);
        assert_eq!(styles[history_spans[first][2] as usize], red);
    }

    #[test]
    fn every_key_parses() {
        for key in [
            TerminalKey::Enter,
            TerminalKey::Escape,
            TerminalKey::Tab,
            TerminalKey::Backspace,
            TerminalKey::Up,
            TerminalKey::Down,
            TerminalKey::Left,
            TerminalKey::Right,
            TerminalKey::CtrlC,
            TerminalKey::CtrlD,
            TerminalKey::CtrlZ,
            TerminalKey::CtrlL,
        ] {
            assert!(Keystroke::parse(key.keystroke()).is_ok(), "{key:?}");
        }
        // Anything else is refused when parsing the phone's message.
        assert!(serde_json::from_value::<TerminalKey>(serde_json::json!("ctrl-shift-q")).is_err());
    }

    #[gpui::test]
    fn input_needs_the_setting(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
        });
        let terminals = cx.new(|_| Terminals::default());
        let refused = terminals.update(cx, |terminals, cx| {
            terminals.input("nope", Some("rm -rf ~".into()), None, cx)
        });
        assert_eq!(refused.unwrap_err().code, ErrorCode::TerminalInputDisabled);

        cx.update(|cx| {
            use gpui::UpdateGlobal as _;
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.companion.get_or_insert_default().terminal_input = Some(true);
                });
            });
        });
        let missing = terminals.update(cx, |terminals, cx| {
            terminals.input("nope", Some("ls".into()), None, cx)
        });
        assert_eq!(missing.unwrap_err().code, ErrorCode::TerminalNotFound);

        // Renaming isn't gated by the typing setting, only by the terminal existing.
        let missing = terminals.update(cx, |terminals, cx| terminals.rename("nope", "Build", cx));
        assert_eq!(missing.unwrap_err().code, ErrorCode::TerminalNotFound);
    }
}
