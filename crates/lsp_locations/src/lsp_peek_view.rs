//! An inline peek view for LSP location results.
//!
//! Instead of navigating away to a multibuffer tab, the results are shown in a
//! block below the cursor's line: a preview of the selected location on the
//! left and the grouped list of locations on the right.

use std::any::Any;
use std::sync::Arc;

use collections::HashSet;
use editor::display_map::{
    BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId, HighlightKey,
};
use editor::scroll::Autoscroll;
use editor::{Editor, EditorSettings, RowHighlightOptions};
use gpui::{
    AnyElement, App, AppContext as _, Bounds, ClickEvent, Context, DragMoveEvent, Element, Entity,
    FocusHandle, Focusable, Global, GlobalElementId, InspectorElementId, LayoutId, MouseButton,
    MouseUpEvent, Pixels, ScrollHandle, Subscription, WeakEntity, Window, canvas, div, prelude::*,
    px,
};
use language::Capability;
use multi_buffer::MultiBuffer;
use project::{Location, Project};
use settings::Settings as _;
use text::Point;
use ui::{Divider, ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt as _;

use crate::{
    Entry, LocationMatch, LspPickerKind, SingleResult, group_entries, render_file_header,
    render_location_row, resolve_matches,
};

/// Rows of editor space the peek block occupies.
const PEEK_ROWS: u32 = 16;

/// Lines of surrounding context loaded into the preview around the selected
/// location. Generous enough that scrolling the preview a little still shows
/// real code rather than the end of the excerpt.
const PREVIEW_CONTEXT_ROWS: u32 = 64;

/// Share of the peek given to the list, leaving the rest to the code preview.
/// The peek spans everything from the gutter to the right edge of the tab, so
/// this is a share of the whole available area.
///
/// Slightly over half: a peek is for scanning the occurrences, and one line of
/// the preview is readable in far less room than the list needs to show a path
/// and a line of code per hit. Dragging the divider overrides this, and the
/// dragged width is what later peeks open at.
const DEFAULT_LIST_WIDTH_FRACTION: f32 = 0.55;

/// Bounds on the divider, so neither pane can be dragged away to nothing.
const MIN_LIST_WIDTH_FRACTION: f32 = 0.15;
const MAX_LIST_WIDTH_FRACTION: f32 = 0.75;

const DIVIDER_WIDTH: Pixels = px(4.);

/// Where the dragged divider position is persisted, so the peek reopens at the
/// width it was last left at.
const LIST_WIDTH_KEY: &str = "lsp_peek_list_width_fraction";

/// The divider position, shared by every peek in the session. Loaded from the
/// key-value store the first time it is needed and written back on each drag, so
/// the width also survives a restart.
struct ListWidthFraction(f32);

impl Global for ListWidthFraction {}

fn list_width_fraction(cx: &mut App) -> f32 {
    if let Some(fraction) = cx.try_global::<ListWidthFraction>() {
        return fraction.0;
    }
    let fraction = db::kvp::GlobalKeyValueStore::global()
        .read_kvp(LIST_WIDTH_KEY)
        .log_err()
        .flatten()
        .and_then(|value| value.parse::<f32>().ok())
        .filter(|fraction| fraction.is_finite())
        .map_or(DEFAULT_LIST_WIDTH_FRACTION, |fraction| {
            fraction.clamp(MIN_LIST_WIDTH_FRACTION, MAX_LIST_WIDTH_FRACTION)
        });
    cx.set_global(ListWidthFraction(fraction));
    fraction
}

fn set_list_width_fraction(fraction: f32, cx: &mut App) {
    cx.set_global(ListWidthFraction(fraction));
}

fn persist_list_width_fraction(fraction: f32, cx: &mut App) {
    cx.background_spawn(async move {
        db::kvp::GlobalKeyValueStore::global()
            .write_kvp(LIST_WIDTH_KEY.into(), fraction.to_string())
            .await
            .log_err();
    })
    .detach();
}

/// Paints its child after the rest of the editor, so the peek sits on top of the
/// minimap and the scrollbar rather than underneath them. Ordinary blocks are
/// painted before [`EditorElement::paint_minimap`], so nothing an ordinary block
/// draws can ever cover it.
///
/// The child keeps its place in the layout, so this changes paint order only and
/// never the peek's width.
///
/// The deferred draw is clipped to the content mask that was active during
/// prepaint, which the editor sets to its own bounds around the whole prepaint
/// pass. That is exactly the right clip and it must not be widened: a block's
/// laid-out width can exceed the visible editor (it grows with the longest line,
/// and the editor normally re-clips blocks when painting them), so without this
/// mask the peek spills over whatever is docked to the right.
struct PaintAboveMinimap {
    child: Option<AnyElement>,
}

impl PaintAboveMinimap {
    fn new(child: impl IntoElement) -> Self {
        Self {
            child: Some(child.into_any_element()),
        }
    }
}

impl IntoElement for PaintAboveMinimap {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for PaintAboveMinimap {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let Some(child) = self.child.as_mut() else {
            return (window.request_layout(gpui::Style::default(), [], cx), ());
        };
        (child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        _: &mut App,
    ) {
        let Some(child) = self.child.take() else {
            return;
        };
        let mask = window.content_mask();
        let offset = window.element_offset();
        window.defer_draw(child, offset, 0, Some(mask));
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        _: &mut Window,
        _: &mut App,
    ) {
    }
}

/// Drag payload for the divider between the preview and the list.
#[derive(Clone)]
struct PeekDivider;

struct PeekDividerGhost;

impl Render for PeekDividerGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// Row highlight marking the selected location's line in the preview.
struct PeekMatchLineHighlight;

/// Per-editor handle on the open peek, so re-invoking the action replaces the
/// previous peek and closing removes the block from the editor that owns it.
struct PeekAddon {
    open: Option<OpenPeek>,
}

struct OpenPeek {
    view: Entity<PeekView>,
    block_id: CustomBlockId,
}

impl editor::Addon for PeekAddon {
    fn to_any(&self) -> &dyn Any {
        self
    }

    fn to_any_mut(&mut self) -> Option<&mut dyn Any> {
        Some(self)
    }
}

/// Runs the LSP query for `kind` and opens the peek below the cursor. A lone
/// result is peeked like any other: jumping to it would defeat the point of
/// staying put. Empty responses are handled by [`resolve_matches`], which
/// reports them, so no peek opens for those.
pub(crate) fn open_for_editor(
    kind: LspPickerKind,
    editor: WeakEntity<Editor>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = editor
        .upgrade()
        .and_then(|editor| editor.read(cx).workspace())
    else {
        return;
    };
    let project = workspace.read(cx).project().clone();
    let workspace = workspace.downgrade();
    window
        .spawn(cx, async move |cx| {
            let Some((kind, matches)) = resolve_matches(
                kind,
                &editor,
                &workspace,
                &project,
                SingleResult::Show,
                cx,
            )
            .await
            else {
                return;
            };
            editor
                .update_in(cx, |editor, window, cx| {
                    show_peek(kind, matches, project, editor, window, cx);
                })
                .log_err();
        })
        .detach();
}

fn show_peek(
    kind: LspPickerKind,
    matches: Vec<LocationMatch>,
    project: Entity<Project>,
    editor: &mut Editor,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    close_peek(editor, window, cx);
    if editor.addon::<PeekAddon>().is_none() {
        editor.register_addon(PeekAddon { open: None });
    }

    let anchor = editor.selections.newest_anchor().head();
    let owner = cx.entity().downgrade();
    let view = cx.new(|cx| PeekView::new(kind, matches, project, owner, window, cx));

    let block_view = view.clone();
    let block_id = editor
        .insert_blocks(
            [BlockProperties {
                placement: BlockPlacement::Below(anchor),
                height: Some(PEEK_ROWS),
                style: BlockStyle::Flex,
                render: Arc::new(move |block_cx: &mut BlockContext| {
                    // A full-width block already spans to the right edge of the
                    // tab, underneath the minimap and scrollbar - that is why an
                    // undeferred peek gets painted over rather than clipped. So
                    // it needs no extra width, only a later paint; widening it
                    // here would push it out of the tab and over whatever panel
                    // is docked to the right.
                    PaintAboveMinimap::new(
                        div()
                            .h(block_cx.line_height * PEEK_ROWS as f32)
                            .w_full()
                            .pl(block_cx.margins.gutter.full_width())
                            .child(block_view.clone()),
                    )
                    .into_any_element()
                }),
                priority: 0,
            }],
            Some(Autoscroll::fit()),
            cx,
        )
        .into_iter()
        .next();
    let Some(block_id) = block_id else {
        return;
    };

    if let Some(addon) = editor.addon_mut::<PeekAddon>() {
        addon.open = Some(OpenPeek {
            view: view.clone(),
            block_id,
        });
    }
    view.read(cx).focus_handle.clone().focus(window, cx);
    cx.notify();
}

/// Removes the peek block from `editor`. A no-op when no peek is open.
fn close_peek(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    let open = editor
        .addon_mut::<PeekAddon>()
        .and_then(|addon| addon.open.take());
    let Some(open) = open else {
        return;
    };
    // Only take focus back when the peek is the thing losing it. The user may
    // have clicked into the editor and re-run the action, in which case focus
    // already belongs where it is.
    let peek_focused = open.view.read(cx).focus_handle.contains_focused(window, cx);
    editor.remove_blocks(HashSet::from_iter([open.block_id]), None, cx);
    if peek_focused {
        editor.focus_handle(cx).focus(window, cx);
    }
    cx.notify();
}

struct PeekView {
    kind: LspPickerKind,
    project: Entity<Project>,
    /// The editor the peek block was inserted into, used to close the peek and
    /// to navigate when a location is confirmed.
    owner: WeakEntity<Editor>,
    matches: Vec<LocationMatch>,
    entries: Vec<Entry>,
    selected_index: usize,
    max_line_number: u32,
    preview_editor: Entity<Editor>,
    focus_handle: FocusHandle,
    list_scroll_handle: ScrollHandle,
    /// Share of the peek's width given to the list. Adjusted by dragging the
    /// divider, and shared with later peeks through [`ListWidthFraction`].
    list_width_fraction: f32,
    /// Bounds of the row holding the preview and the list, captured during
    /// layout so a drag can be turned into a fraction of that row.
    body_bounds: Bounds<Pixels>,
    _dismiss_on_focus_out: Subscription,
}

impl PeekView {
    fn new(
        kind: LspPickerKind,
        matches: Vec<LocationMatch>,
        project: Entity<Project>,
        owner: WeakEntity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let match_indices = (0..matches.len()).collect::<Vec<_>>();
        let (entries, max_line_number) = group_entries(&matches, &match_indices);

        let preview_editor = cx.new(|cx| {
            // Narrowed per buffer as excerpts are set; the peek never writes.
            let multi_buffer = cx.new(|_| MultiBuffer::without_headers(Capability::ReadWrite));
            let mut editor = Editor::for_multibuffer(multi_buffer, None, window, cx);
            let gutter_line_numbers = EditorSettings::get_global(cx).gutter.line_numbers;
            editor.set_read_only(true);
            editor.set_input_enabled(false);
            editor.disable_scrollbars_and_minimap(window, cx);
            editor.disable_inline_diagnostics();
            editor.disable_diagnostics(cx);
            editor.disable_expand_excerpt_buttons(cx);
            editor.disable_mouse_wheel_zoom();
            editor.set_show_gutter(gutter_line_numbers, cx);
            editor.set_show_line_numbers(gutter_line_numbers, cx);
            editor.set_show_breakpoints(false, cx);
            editor.set_show_bookmarks(false, cx);
            editor.set_show_code_actions(false, cx);
            editor.set_show_runnables(false, cx);
            editor.set_show_git_diff_gutter(false, cx);
            editor.set_show_wrap_guides(false, cx);
            editor.set_show_indent_guides(false, cx);
            editor.set_show_cursor_when_unfocused(false, cx);
            editor.set_soft_wrap_mode(language::language_settings::SoftWrap::None, cx);
            editor
        });

        // Clicking back into the code (or anywhere else) dismisses the peek, so
        // it cannot be left behind occupying rows with no obvious way to close
        // it. Focus moving *into* the preview editor keeps the peek's handle on
        // the focus path, so this only fires when focus really leaves.
        let focus_handle = cx.focus_handle();
        let dismiss_on_focus_out =
            cx.on_focus_out(&focus_handle, window, |this: &mut Self, _, window, cx| {
                this.close(window, cx);
            });

        let mut this = Self {
            kind,
            project,
            owner,
            matches,
            entries,
            selected_index: 0,
            max_line_number,
            preview_editor,
            focus_handle,
            list_scroll_handle: ScrollHandle::new(),
            list_width_fraction: list_width_fraction(cx),
            body_bounds: Bounds::default(),
            _dismiss_on_focus_out: dismiss_on_focus_out,
        };
        this.selected_index = this.first_selectable_index().unwrap_or(0);
        this.update_preview(cx);
        this
    }

    fn first_selectable_index(&self) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| matches!(entry, Entry::Match(_)))
    }

    fn last_selectable_index(&self) -> Option<usize> {
        self.entries
            .iter()
            .rposition(|entry| matches!(entry, Entry::Match(_)))
    }

    fn selected_location_match(&self) -> Option<&LocationMatch> {
        match self.entries.get(self.selected_index)? {
            Entry::Match(match_index) => self.matches.get(*match_index),
            Entry::Header(_) | Entry::Separator => None,
        }
    }

    /// Number of files the results span, for the header summary.
    fn file_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| matches!(entry, Entry::Header(_)))
            .count()
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.selected_index == index {
            return;
        }
        self.selected_index = index;
        self.update_preview(cx);
        cx.notify();
    }

    /// Selects `index` and scrolls it into view. Only for keyboard navigation:
    /// scrolling on a *click* would move the row out from under the pointer
    /// between the two clicks of a double click, so the second click would miss
    /// (or hit a different row).
    fn select_and_scroll(&mut self, index: usize, cx: &mut Context<Self>) {
        self.select(index, cx);
        self.list_scroll_handle.scroll_to_item(index);
    }

    /// Moves the selection to the next selectable row in `direction`, wrapping
    /// around the ends of the list. Headers and separators are skipped.
    fn select_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let len = self.entries.len();
        if len == 0 {
            return;
        }
        let mut index = self.selected_index;
        for _ in 0..len {
            index = if forward {
                (index + 1) % len
            } else {
                (index + len - 1) % len
            };
            if matches!(self.entries.get(index), Some(Entry::Match(_))) {
                self.select_and_scroll(index, cx);
                return;
            }
        }
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.select_step(true, cx);
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_step(false, cx);
    }

    fn select_first(&mut self, _: &menu::SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.first_selectable_index() {
            self.select_and_scroll(index, cx);
        }
    }

    fn select_last(&mut self, _: &menu::SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.last_selectable_index() {
            self.select_and_scroll(index, cx);
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.open_selected(false, window, cx);
    }

    fn secondary_confirm(
        &mut self,
        _: &menu::SecondaryConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_selected(true, window, cx);
    }

    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        self.close(window, cx);
    }

    /// Escape reaches the peek as `editor::Cancel` when focus is inside the
    /// preview editor, which propagates the action once it has nothing of its
    /// own left to cancel.
    fn editor_cancel(
        &mut self,
        _: &editor::actions::Cancel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close(window, cx);
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.tear_down(None, window, cx);
    }

    /// Navigates the owning editor to the selected location and closes the peek.
    fn open_selected(&mut self, split: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(location_match) = self.selected_location_match() else {
            return;
        };
        let location = Location {
            buffer: location_match.buffer.clone(),
            range: location_match.anchor_range.clone(),
        };
        self.tear_down(Some((location, split)), window, cx);
    }

    /// Closes this peek and optionally navigates to `location` afterwards:
    /// navigating within the same editor first would leave the block anchored at
    /// the old cursor line.
    ///
    /// Deferred because callers run inside an update of this view, which the
    /// teardown reads back through the editor's addon. The identity check covers
    /// the gap that deferral opens: a peek that has already been replaced (its
    /// focus-out listener fires after the replacement took focus) must not tear
    /// down its successor.
    fn tear_down(
        &mut self,
        navigate_to: Option<(Location, bool)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        let this = cx.entity().downgrade();
        window.defer(cx, move |window, cx| {
            owner.update(cx, |editor, cx| {
                let is_current = editor
                    .addon::<PeekAddon>()
                    .and_then(|addon| addon.open.as_ref())
                    .is_some_and(|open| open.view.entity_id() == this.entity_id());
                if is_current {
                    close_peek(editor, window, cx);
                }
                if let Some((location, split)) = navigate_to {
                    editor
                        .open_location(location, split, window, cx)
                        .detach_and_log_err(cx);
                }
            });
        });
    }

    /// Points the preview editor at the selected location, highlighting the
    /// matched range and centering it.
    fn update_preview(&mut self, cx: &mut Context<Self>) {
        let Some(location_match) = self.selected_location_match() else {
            return;
        };
        let buffer = location_match.buffer.clone();
        let anchor_range = location_match.anchor_range.clone();
        let focus_row = location_match.line_number.saturating_sub(1);

        self.preview_editor.update(cx, |editor, cx| {
            let multi_buffer = editor.buffer().clone();
            multi_buffer.update(cx, |multi_buffer, cx| {
                multi_buffer.clear(cx);
                multi_buffer.set_excerpts_for_buffer(
                    buffer,
                    [Point::new(focus_row, 0)..Point::new(focus_row, 0)],
                    PREVIEW_CONTEXT_ROWS,
                    cx,
                );
            });

            editor.clear_row_highlights::<PeekMatchLineHighlight>();
            editor.clear_background_highlights(HighlightKey::PeekPreview, cx);

            let snapshot = multi_buffer.read(cx).snapshot(cx);
            let Some(range) = snapshot
                .anchor_in_excerpt(anchor_range.start)
                .zip(snapshot.anchor_in_excerpt(anchor_range.end))
                .map(|(start, end)| start..end)
            else {
                return;
            };

            editor.highlight_rows::<PeekMatchLineHighlight>(
                range.clone(),
                |cx| cx.theme().colors().editor_active_line_background,
                RowHighlightOptions::default(),
                cx,
            );
            editor.highlight_background(
                HighlightKey::PeekPreview,
                std::slice::from_ref(&range),
                |_, theme| theme.colors().search_match_background,
                cx,
            );
            editor.request_autoscroll(Autoscroll::center().for_anchor(range.start), cx);
        });
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let match_count = self.matches.len();
        let file_count = self.file_count();
        let summary = format!(
            "{match_count} {} in {file_count} {}",
            self.kind.result_noun(match_count),
            if file_count == 1 { "file" } else { "files" },
        );

        h_flex()
            .flex_none()
            .w_full()
            .px_2()
            .py_0p5()
            .gap_1()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(Label::new(summary).size(LabelSize::Small).color(Color::Muted))
            .child(
                IconButton::new("close-peek", IconName::Close)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::for_action_title("Close Peek", &menu::Cancel))
                    .on_click(cx.listener(|this, _, window, cx| this.close(window, cx))),
            )
    }

    /// Turns a divider drag into a new split. The pointer position is absolute,
    /// so it is measured against the captured bounds of the row it is dragging
    /// inside.
    fn resize(&mut self, pointer_x: Pixels, cx: &mut Context<Self>) {
        let width = self.body_bounds.size.width;
        if width <= px(0.) {
            return;
        }
        let list_width = self.body_bounds.right() - pointer_x;
        let fraction =
            (list_width / width).clamp(MIN_LIST_WIDTH_FRACTION, MAX_LIST_WIDTH_FRACTION);
        if fraction == self.list_width_fraction {
            return;
        }
        self.list_width_fraction = fraction;
        set_list_width_fraction(fraction, cx);
        cx.notify();
    }

    fn render_divider(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("lsp-peek-divider")
            .occlude()
            .flex_none()
            .w(DIVIDER_WIDTH)
            .h_full()
            .cursor_col_resize()
            .bg(cx.theme().colors().border_variant)
            .hover(|style| style.bg(cx.theme().colors().border_focused))
            .on_drag(PeekDivider, |_, _, _, cx| cx.new(|_| PeekDividerGhost))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            // The drag only updates the session-wide value; the write to disk
            // waits for the drag to finish so a single resize is one write
            // rather than one per frame.
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    persist_list_width_fraction(this.list_width_fraction, cx);
                }),
            )
    }

    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| match entry {
                Entry::Separator => div()
                    .py(DynamicSpacing::Base04.rems(cx))
                    .child(Divider::horizontal())
                    .into_any_element(),
                Entry::Header(path) => render_file_header(path, &self.project, cx),
                Entry::Match(match_index) => {
                    let Some(location_match) = self.matches.get(*match_index) else {
                        return div().into_any_element();
                    };
                    ListItem::new(index)
                        .spacing(ListItemSpacing::Sparse)
                        .inset(true)
                        .toggle_state(index == self.selected_index)
                        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                            // Single click previews, double click jumps, so the
                            // list can be browsed with the mouse without the
                            // peek closing under the pointer.
                            this.focus_handle.focus(window, cx);
                            this.select(index, cx);
                            if event.click_count() > 1 {
                                this.open_selected(false, window, cx);
                            }
                        }))
                        .child(render_location_row(location_match, self.max_line_number, cx))
                        .into_any_element()
                }
            })
            .collect::<Vec<_>>();

        div()
            .id("lsp-peek-locations")
            .flex_none()
            .w(relative(self.list_width_fraction))
            .h_full()
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll_handle)
            .children(entries)
    }
}

impl Focusable for PeekView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PeekView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("LspPeekView")
            .track_focus(&self.focus_handle)
            .block_mouse_except_scroll()
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::secondary_confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::editor_cancel))
            .size_full()
            .overflow_hidden()
            .border_y_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().editor_background)
            .child(self.render_header(cx))
            .child(
                h_flex()
                    .relative()
                    .flex_1()
                    .w_full()
                    .min_h_0()
                    .on_drag_move(cx.listener(
                        |this, event: &DragMoveEvent<PeekDivider>, _, cx| {
                            this.resize(event.event.position.x, cx);
                        },
                    ))
                    .child(
                        canvas(
                            {
                                let this = cx.entity().downgrade();
                                move |bounds, _, cx| {
                                    this.update(cx, |this, _| this.body_bounds = bounds).ok();
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_hidden()
                            .child(self.preview_editor.clone()),
                    )
                    .child(self.render_divider(cx))
                    .child(self.render_list(cx)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor::test::editor_lsp_test_context::EditorLspTestContext;
    use gpui::{TestAppContext, point, size};
    use indoc::indoc;
    use workspace::Item as _;

    const SOURCE: &str = indoc! {r#"
        fn main() {
            let aˇbc = 123;
            let xyz = abc;
        }
    "#};

    async fn references_cx(cx: &mut TestAppContext) -> EditorLspTestContext {
        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                references_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            cx,
        )
        .await;
        cx.set_state(SOURCE);
        cx
    }

    fn respond_with(cx: &mut EditorLspTestContext, ranges: &'static [(u32, u32, u32)]) {
        cx.lsp
            .set_request_handler::<lsp::request::References, _, _>(async move |params, _| {
                let uri = params.text_document_position.text_document.uri;
                Ok(Some(
                    ranges
                        .iter()
                        .map(|&(row, start, end)| lsp::Location {
                            uri: uri.clone(),
                            range: lsp::Range::new(
                                lsp::Position::new(row, start),
                                lsp::Position::new(row, end),
                            ),
                        })
                        .collect(),
                ))
            });
    }

    fn peek(cx: &mut EditorLspTestContext, kind: LspPickerKind) {
        let editor = cx.editor.downgrade();
        cx.update(|window, cx| open_for_editor(kind, editor, window, cx));
        cx.run_until_parked();
    }

    fn open_peek_view(cx: &mut EditorLspTestContext) -> Option<Entity<PeekView>> {
        let editor = cx.editor.clone();
        cx.update(|_, cx| {
            let addon = editor.read(cx).addon::<PeekAddon>()?;
            Some(addon.open.as_ref()?.view.clone())
        })
    }

    #[gpui::test]
    async fn test_multiple_references_open_peek(cx: &mut TestAppContext) {
        let mut cx = references_cx(cx).await;
        respond_with(&mut cx, &[(1, 8, 11), (2, 14, 17)]);

        peek(&mut cx, LspPickerKind::References);

        let view = open_peek_view(&mut cx).expect("multiple references should open a peek");
        cx.update(|_, cx| {
            let view = view.read(cx);
            assert_eq!(view.matches.len(), 2);
            assert!(
                matches!(view.entries.first(), Some(Entry::Header(_))),
                "the list should start with the file header"
            );
            assert!(
                view.selected_location_match().is_some(),
                "the first location should be selected"
            );
        });
    }

    #[gpui::test]
    async fn test_single_result_is_peeked_rather_than_jumped_to(cx: &mut TestAppContext) {
        let mut cx = references_cx(cx).await;
        respond_with(&mut cx, &[(2, 14, 17)]);

        peek(&mut cx, LspPickerKind::References);

        let view = open_peek_view(&mut cx)
            .expect("a lone result should still peek, not navigate the editor away");
        cx.update(|_, cx| assert_eq!(view.read(cx).matches.len(), 1));
        // The cursor has not moved off its starting position.
        cx.assert_editor_state(SOURCE);
    }

    #[gpui::test]
    async fn test_no_results_does_not_open_peek(cx: &mut TestAppContext) {
        let mut cx = references_cx(cx).await;
        cx.lsp
            .set_request_handler::<lsp::request::References, _, _>(async move |_params, _| {
                Ok(Some(Vec::new()))
            });

        peek(&mut cx, LspPickerKind::References);

        assert!(
            open_peek_view(&mut cx).is_none(),
            "an empty result should not open a peek"
        );
    }

    #[gpui::test]
    async fn test_selection_moves_through_locations_and_wraps(cx: &mut TestAppContext) {
        let mut cx = references_cx(cx).await;
        respond_with(&mut cx, &[(1, 8, 11), (2, 14, 17)]);

        peek(&mut cx, LspPickerKind::References);
        let view = open_peek_view(&mut cx).expect("multiple references should open a peek");

        let selected_line = |cx: &mut EditorLspTestContext| {
            let view = view.clone();
            cx.update(|_, cx| {
                view.read(cx)
                    .selected_location_match()
                    .map(|location_match| location_match.line_number)
            })
        };

        assert_eq!(selected_line(&mut cx), Some(2));

        let step = |cx: &mut EditorLspTestContext, forward: bool| {
            let view = view.clone();
            cx.update(|_, cx| view.update(cx, |view, cx| view.select_step(forward, cx)));
        };

        step(&mut cx, true);
        assert_eq!(selected_line(&mut cx), Some(3));

        // Past the end, selection wraps back to the first location rather than
        // landing on the file header.
        step(&mut cx, true);
        assert_eq!(selected_line(&mut cx), Some(2));

        step(&mut cx, false);
        assert_eq!(selected_line(&mut cx), Some(3));
    }

    #[gpui::test]
    async fn test_dragging_the_divider_resizes_and_clamps(cx: &mut TestAppContext) {
        let mut cx = references_cx(cx).await;
        respond_with(&mut cx, &[(1, 8, 11), (2, 14, 17)]);

        peek(&mut cx, LspPickerKind::References);
        let view = open_peek_view(&mut cx).expect("multiple references should open a peek");

        let fraction = |cx: &mut EditorLspTestContext| {
            let view = view.clone();
            cx.update(|_, cx| view.read(cx).list_width_fraction)
        };
        // The bounds are set in the same update as the drag: the layout pass
        // captures the real ones on every render, so a value written in its own
        // update would be overwritten before the drag ran.
        let drag_to = |cx: &mut EditorLspTestContext, x: f32| {
            let view = view.clone();
            cx.update(|_, cx| {
                view.update(cx, |view, cx| {
                    view.body_bounds = Bounds {
                        origin: point(px(100.), px(0.)),
                        size: size(px(1000.), px(400.)),
                    };
                    view.resize(px(x), cx);
                })
            });
        };

        // The row spans x 100..1100, so a divider at 800 leaves 300 of 1000 for
        // the list.
        drag_to(&mut cx, 800.);
        assert_eq!(fraction(&mut cx), 0.3);

        // Dragging past either end clamps rather than collapsing a pane.
        drag_to(&mut cx, 1099.);
        assert_eq!(fraction(&mut cx), MIN_LIST_WIDTH_FRACTION);
        drag_to(&mut cx, 101.);
        assert_eq!(fraction(&mut cx), MAX_LIST_WIDTH_FRACTION);
    }

    #[gpui::test]
    async fn test_divider_width_is_remembered_by_later_peeks(cx: &mut TestAppContext) {
        let mut cx = references_cx(cx).await;
        respond_with(&mut cx, &[(1, 8, 11), (2, 14, 17)]);

        peek(&mut cx, LspPickerKind::References);
        let first = open_peek_view(&mut cx).expect("multiple references should open a peek");
        cx.update(|_, cx| {
            first.update(cx, |view, cx| {
                view.body_bounds = Bounds {
                    origin: point(px(0.), px(0.)),
                    size: size(px(1000.), px(400.)),
                };
                view.resize(px(700.), cx);
            })
        });
        assert_eq!(cx.update(|_, cx| first.read(cx).list_width_fraction), 0.3);

        peek(&mut cx, LspPickerKind::References);
        let second = open_peek_view(&mut cx).expect("re-invoking should leave a peek open");
        assert_eq!(
            cx.update(|_, cx| second.read(cx).list_width_fraction),
            0.3,
            "a new peek should open at the width the last one was dragged to"
        );
    }

    #[gpui::test]
    async fn test_cancel_closes_peek(cx: &mut TestAppContext) {
        let mut cx = references_cx(cx).await;
        respond_with(&mut cx, &[(1, 8, 11), (2, 14, 17)]);

        peek(&mut cx, LspPickerKind::References);
        let view = open_peek_view(&mut cx).expect("multiple references should open a peek");

        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.cancel(&menu::Cancel, window, cx));
        });
        cx.run_until_parked();

        assert!(
            open_peek_view(&mut cx).is_none(),
            "cancelling should remove the peek block"
        );
        let editor = cx.editor.clone();
        assert!(
            cx.update(|window, cx| editor.read(cx).focus_handle(cx).is_focused(window)),
            "cancelling should return focus to the editor"
        );
    }

    #[gpui::test]
    async fn test_cmd_click_opens_the_peek(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                definition_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            cx,
        )
        .await;
        // The cursor sits on the *usage*: a definition that points back at the
        // cursor is filtered out before it ever reaches the peek.
        cx.set_state(indoc! {r#"
            fn main() {
                let abc = 123;
                let xyz = aˇbc;
            }
        "#});

        cx.lsp
            .set_request_handler::<lsp::request::GotoDefinition, _, _>(async move |params, _| {
                let uri = params.text_document_position_params.text_document.uri;
                Ok(Some(lsp::GotoDefinitionResponse::Scalar(lsp::Location {
                    uri,
                    range: lsp::Range::new(
                        lsp::Position::new(1, 8),
                        lsp::Position::new(1, 11),
                    ),
                })))
            });

        let screen_coord = cx
            .editor(|editor, _, cx| editor.pixel_position_of_cursor(cx))
            .unwrap();
        cx.simulate_click(screen_coord, gpui::Modifiers::secondary_key());
        cx.run_until_parked();

        let view = open_peek_view(&mut cx)
            .expect("cmd-click should peek the definition instead of navigating to it");
        cx.update(|_, cx| {
            let view = view.read(cx);
            assert_eq!(view.kind, LspPickerKind::Definition);
            assert_eq!(
                view.selected_location_match().map(|it| it.line_number),
                Some(2),
                "the peek should be showing the definition on row 1"
            );
        });
    }

    #[gpui::test]
    async fn test_focusing_the_editor_dismisses_the_peek(cx: &mut TestAppContext) {
        let mut cx = references_cx(cx).await;
        respond_with(&mut cx, &[(1, 8, 11), (2, 14, 17)]);

        peek(&mut cx, LspPickerKind::References);
        assert!(open_peek_view(&mut cx).is_some());

        let editor = cx.editor.clone();
        cx.update(|window, cx| editor.read(cx).focus_handle(cx).focus(window, cx));
        cx.run_until_parked();

        assert!(
            open_peek_view(&mut cx).is_none(),
            "clicking back into the editor should dismiss the peek"
        );
    }

    #[gpui::test]
    async fn test_confirm_navigates_and_closes_the_peek(cx: &mut TestAppContext) {
        let mut cx = references_cx(cx).await;
        respond_with(&mut cx, &[(1, 8, 11), (2, 14, 17)]);

        peek(&mut cx, LspPickerKind::References);
        let view = open_peek_view(&mut cx).expect("multiple references should open a peek");

        // Move off the first location so the jump is observable.
        cx.update(|_, cx| view.update(cx, |view, cx| view.select_step(true, cx)));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.confirm(&menu::Confirm, window, cx));
        });
        cx.run_until_parked();

        assert!(
            open_peek_view(&mut cx).is_none(),
            "confirming should close the peek"
        );
        cx.assert_editor_state(indoc! {r#"
            fn main() {
                let abc = 123;
                let xyz = «abcˇ»;
            }
        "#});
    }

    /// Covers the "bring me to it" half of confirming: a location in a file that
    /// is not the one being peeked has to open as a workspace item.
    #[gpui::test]
    async fn test_confirm_opens_a_location_in_another_file(cx: &mut TestAppContext) {
        let mut cx = references_cx(cx).await;

        let other_path = EditorLspTestContext::root_path().join("dir").join("other.rs");
        let workspace = cx.workspace.clone();
        let fs = cx.update(|_, cx| workspace.read(cx).project().read(cx).fs().clone());
        fs.as_fake()
            .insert_file(&other_path, b"fn other() {\n    let q = abc;\n}\n".to_vec())
            .await;
        cx.run_until_parked();

        let other_uri = lsp::Uri::from_file_path(&other_path).unwrap();
        cx.lsp
            .set_request_handler::<lsp::request::References, _, _>(move |params, _| {
                let this_uri = params.text_document_position.text_document.uri;
                let other_uri = other_uri.clone();
                async move {
                    Ok(Some(vec![
                        lsp::Location {
                            uri: this_uri,
                            range: lsp::Range::new(
                                lsp::Position::new(1, 8),
                                lsp::Position::new(1, 11),
                            ),
                        },
                        lsp::Location {
                            uri: other_uri,
                            range: lsp::Range::new(
                                lsp::Position::new(1, 12),
                                lsp::Position::new(1, 15),
                            ),
                        },
                    ]))
                }
            });

        peek(&mut cx, LspPickerKind::References);
        let view = open_peek_view(&mut cx).expect("two references should open a peek");

        // Select the reference in the other file, then confirm it.
        cx.update(|_, cx| view.update(cx, |view, cx| view.select_step(true, cx)));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.confirm(&menu::Confirm, window, cx));
        });
        cx.run_until_parked();

        assert!(
            open_peek_view(&mut cx).is_none(),
            "confirming should close the peek"
        );
        let active_path = cx.update(|_, cx| {
            workspace
                .read(cx)
                .active_item(cx)
                .and_then(|item| item.act_as::<Editor>(cx))
                .and_then(|editor| {
                    editor
                        .read(cx)
                        .buffer()
                        .read(cx)
                        .as_singleton()?
                        .read(cx)
                        .file()
                        .map(|file| file.path().as_unix_str().to_string())
                })
        });
        assert_eq!(
            active_path.as_deref(),
            Some("dir/other.rs"),
            "confirming a location in another file should open that file"
        );
    }

    #[gpui::test]
    async fn test_reinvoking_replaces_the_open_peek(cx: &mut TestAppContext) {
        let mut cx = references_cx(cx).await;
        respond_with(&mut cx, &[(1, 8, 11), (2, 14, 17)]);

        peek(&mut cx, LspPickerKind::References);
        let first = open_peek_view(&mut cx).expect("multiple references should open a peek");

        peek(&mut cx, LspPickerKind::References);
        let second = open_peek_view(&mut cx).expect("re-invoking should leave a peek open");

        assert_ne!(
            first.entity_id(),
            second.entity_id(),
            "re-invoking should replace the previous peek rather than stack a second block"
        );
    }
}
