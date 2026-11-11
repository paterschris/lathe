//! Draws extension views with Lathe's own components, so they follow the
//! theme. Views arrive validated (see `extension_host::wasm_host::lathe_views`),
//! so this walks them without bounds or cycle checks.
//!
//! [`ExtensionViewHost`] is the entity a surface (panel, preview, status bar)
//! embeds to show one view. It owns the state a view can't carry itself: the
//! editors behind text inputs and the focus handles for keyboard navigation.
//!
//! File previews (`lathe_preview`) embed it; panels will too.

use std::str::FromStr as _;
use std::sync::Arc;
use std::time::Duration;

use collections::{HashMap, HashSet};
use editor::{Editor, EditorEvent};
use extension_host::ExtensionStore;
use extension_host::wasm_host::lathe_views::{
    Disclosure, LatheView, MenuEntry, NodeKind, ROWS_PER_REQUEST, SelectOption, TextStyle, Tone,
    ViewEvent,
};
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    SharedString, Styled, Subscription, Task, UniformListScrollHandle, Window, div, px,
    uniform_list,
};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use ui::{
    Button, Checkbox, Chip, ContextMenu, Divider, DropdownMenu, Headline, HeadlineSize, Icon,
    IconName, IconSize, Label, LabelCommon as _, LabelSize, ListItem, ListItemSpacing, ProgressBar,
    SpinnerLabel, ToggleState, Tooltip, h_flex, prelude::*, right_click_menu, v_flex,
};
use util::ResultExt as _;

use crate::lathe_ui::ExtensionViews;

/// Width of one tree indent level; matches the 1rem the arrow is drawn left of
/// a row's content.
const TREE_INDENT_STEP: gpui::Pixels = px(16.);

/// How long typing must pause before an input sends `input-changed`.
pub const INPUT_DEBOUNCE: Duration = Duration::from_millis(150);

/// Where virtual lists get their rows: (extension, view, list, start, count).
/// Production asks the extension; tests answer directly.
pub type RowSource = Arc<
    dyn Fn(Arc<str>, Arc<str>, Arc<str>, u32, u32, &mut App) -> Task<anyhow::Result<Vec<NodeKind>>>,
>;

fn rows_from_extension(
    extension_id: Arc<str>,
    view_id: Arc<str>,
    list_id: Arc<str>,
    start: u32,
    count: u32,
    cx: &mut App,
) -> Task<anyhow::Result<Vec<NodeKind>>> {
    let Some(store) = ExtensionStore::try_global(cx) else {
        return Task::ready(Err(anyhow::anyhow!("extensions are not available")));
    };
    store.update(cx, |store, cx| {
        store.lathe_provide_rows(&extension_id, view_id, list_id, start, count, cx)
    })
}

/// Rows of one virtual list fetched so far, in pages of `ROWS_PER_REQUEST`.
struct VirtualListState {
    revision: u32,
    pages: HashMap<u32, Vec<NodeKind>>,
    /// Pages asked for and not answered yet, or that failed (so a broken
    /// extension isn't asked again on every frame).
    requested: HashSet<u32>,
    scroll: UniformListScrollHandle,
}

/// Where a view's events go. Production sends them to the extension; tests
/// record them.
pub type ViewEventSink = Arc<dyn Fn(Arc<str>, Arc<str>, Arc<str>, ViewEvent, &mut App)>;

fn send_to_extension(
    extension_id: Arc<str>,
    view_id: Arc<str>,
    element_id: Arc<str>,
    event: ViewEvent,
    cx: &mut App,
) {
    let Some(store) = ExtensionStore::try_global(cx) else {
        return;
    };
    let task = store.update(cx, |store, cx| {
        store.dispatch_lathe_view_event(&extension_id, view_id, element_id, event, cx)
    });
    cx.background_spawn(async move { task.await.log_err() })
        .detach();
}

struct InputState {
    editor: Entity<Editor>,
    /// Where the input sits in Tab order. Applied by the wrapper element,
    /// which tracks the editor's focus handle.
    tab_index: isize,
    disabled: bool,
    /// The value the extension last sent, so its own echo of what the user
    /// typed doesn't reset the field.
    last_value: Arc<str>,
    pending_change: Option<Task<()>>,
    _subscription: Subscription,
}

/// Shows one extension view and turns what the user does into view events.
pub struct ExtensionViewHost {
    extension_id: Arc<str>,
    view_id: Arc<str>,
    views: Entity<ExtensionViews>,
    sink: ViewEventSink,
    row_source: RowSource,
    virtual_lists: HashMap<Arc<str>, VirtualListState>,
    focus_handle: FocusHandle,
    inputs: HashMap<Arc<str>, InputState>,
    element_focus: HashMap<Arc<str>, FocusHandle>,
    /// Markdown nodes, by node index, so their parsed form survives redraws.
    markdown: HashMap<u32, Entity<Markdown>>,
    /// Set at the start of each render, for markdown nodes.
    markdown_style: Option<MarkdownStyle>,
    _views_subscription: Subscription,
    _release_subscription: Subscription,
}

impl ExtensionViewHost {
    /// A host that sends events to the extension and asks it for rows.
    pub fn for_extension(
        extension_id: Arc<str>,
        view_id: Arc<str>,
        views: Entity<ExtensionViews>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_sink(
            extension_id,
            view_id,
            views,
            Arc::new(send_to_extension),
            window,
            cx,
        )
        .with_row_source(Arc::new(rows_from_extension))
    }

    pub fn with_sink(
        extension_id: Arc<str>,
        view_id: Arc<str>,
        views: Entity<ExtensionViews>,
        sink: ViewEventSink,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe(&views, |_, _, cx| cx.notify());
        // Count this host as showing the view, so the extension hears when its
        // view becomes visible and when nothing shows it anymore.
        views.update(cx, |views, cx| {
            views.attach(extension_id.clone(), view_id.clone(), cx)
        });
        let release_subscription = cx.on_release(|this, cx| {
            let (extension_id, view_id) = (this.extension_id.clone(), this.view_id.clone());
            this.views
                .update(cx, |views, cx| views.detach(extension_id, view_id, cx));
        });
        Self {
            extension_id,
            view_id,
            views,
            sink,
            row_source: Arc::new(|_, _, _, _, _, _| {
                Task::ready(Err(anyhow::anyhow!("no row source")))
            }),
            virtual_lists: HashMap::default(),
            focus_handle: cx.focus_handle(),
            inputs: HashMap::default(),
            element_focus: HashMap::default(),
            markdown: HashMap::default(),
            markdown_style: None,
            _views_subscription: subscription,
            _release_subscription: release_subscription,
        }
    }

    /// (extension id, view id) of the view this host shows.
    pub fn key(&self) -> (Arc<str>, Arc<str>) {
        (self.extension_id.clone(), self.view_id.clone())
    }

    pub fn with_row_source(mut self, row_source: RowSource) -> Self {
        self.row_source = row_source;
        self
    }

    fn current_view(&self, cx: &App) -> Option<LatheView> {
        self.views
            .read(cx)
            .view(&self.extension_id, &self.view_id)
            .cloned()
    }

    /// Creates editors and focus handles for new elements, drops ones for
    /// elements that are gone, and syncs input values the extension changed.
    fn prepare(&mut self, view: &LatheView, window: &mut Window, cx: &mut Context<Self>) {
        let mut seen_inputs = Vec::new();
        let mut seen_focusable = Vec::new();
        let mut seen_markdown = Vec::new();
        let mut seen_lists = Vec::new();
        let mut tab_index = 0_isize;
        for index in 0..view.len() as u32 {
            match &view.node(index).kind {
                NodeKind::TextInput {
                    id,
                    value,
                    placeholder,
                    disabled,
                } => {
                    tab_index += 1;
                    seen_inputs.push(id.clone());
                    self.prepare_input(
                        id,
                        value,
                        placeholder.as_deref(),
                        *disabled,
                        tab_index,
                        window,
                        cx,
                    );
                }
                NodeKind::Button { id, .. }
                | NodeKind::ListItem { id, .. }
                | NodeKind::Checkbox { id, .. } => {
                    tab_index += 1;
                    seen_focusable.push(id.clone());
                    let handle = self
                        .element_focus
                        .entry(id.clone())
                        .or_insert_with(|| cx.focus_handle());
                    *handle = handle.clone().tab_index(tab_index).tab_stop(true);
                }
                NodeKind::Select { .. } => tab_index += 1,
                NodeKind::VirtualList { id, revision, .. } => {
                    seen_lists.push(id.clone());
                    let state =
                        self.virtual_lists
                            .entry(id.clone())
                            .or_insert_with(|| VirtualListState {
                                revision: *revision,
                                pages: HashMap::default(),
                                requested: HashSet::default(),
                                scroll: UniformListScrollHandle::new(),
                            });
                    if state.revision != *revision {
                        state.revision = *revision;
                        state.pages.clear();
                        state.requested.clear();
                    }
                }
                NodeKind::Markdown(source) => {
                    seen_markdown.push(index);
                    let source = SharedString::from(source.to_string());
                    match self.markdown.get(&index) {
                        Some(markdown) => {
                            if markdown.read(cx).source().as_ref() != source.as_ref() {
                                markdown.update(cx, |markdown, cx| markdown.reset(source, cx));
                            }
                        }
                        None => {
                            let markdown = cx.new(|cx| Markdown::new(source, None, None, cx));
                            self.markdown.insert(index, markdown);
                        }
                    }
                }
                _ => {}
            }
        }
        self.markdown
            .retain(|index, _| seen_markdown.contains(index));
        self.virtual_lists.retain(|id, _| seen_lists.contains(id));
        self.inputs.retain(|id, _| seen_inputs.contains(id));
        self.element_focus
            .retain(|id, _| seen_focusable.contains(id));
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_input(
        &mut self,
        id: &Arc<str>,
        value: &Arc<str>,
        placeholder: Option<&str>,
        disabled: bool,
        tab_index: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.inputs.contains_key(id) {
            let editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_text(value.to_string(), window, cx);
                editor
            });
            let subscription = cx.subscribe(&editor, {
                let id = id.clone();
                move |this, editor, event, cx| {
                    if matches!(event, EditorEvent::BufferEdited) {
                        this.schedule_input_changed(id.clone(), editor, cx);
                    }
                }
            });
            self.inputs.insert(
                id.clone(),
                InputState {
                    editor,
                    tab_index,
                    disabled,
                    last_value: value.clone(),
                    pending_change: None,
                    _subscription: subscription,
                },
            );
        }
        let Some(input) = self.inputs.get_mut(id) else {
            return;
        };
        input.tab_index = tab_index;
        input.disabled = disabled;
        let editor = input.editor.clone();
        let focused = editor.focus_handle(cx).is_focused(window);
        // Only take a new value from the extension while the user isn't typing
        // in the field, so a slow echo can't undo their keystrokes.
        if input.last_value != *value && !focused {
            input.last_value = value.clone();
            editor.update(cx, |editor, cx| {
                editor.set_text(value.to_string(), window, cx)
            });
        }
        editor.update(cx, |editor, cx| {
            editor.set_read_only(disabled);
            editor.set_placeholder_text(placeholder.unwrap_or_default(), window, cx);
        });
    }

    fn schedule_input_changed(
        &mut self,
        id: Arc<str>,
        editor: Entity<Editor>,
        cx: &mut Context<Self>,
    ) {
        let task = cx.spawn({
            let id = id.clone();
            async move |this, cx| {
                cx.background_executor().timer(INPUT_DEBOUNCE).await;
                this.update(cx, |this, cx| {
                    let text = editor.read(cx).text(cx);
                    if let Some(input) = this.inputs.get_mut(&id) {
                        input.pending_change = None;
                    }
                    this.emit(id, ViewEvent::InputChanged(text), cx);
                })
                .log_err();
            }
        });
        if let Some(input) = self.inputs.get_mut(&id) {
            input.pending_change = Some(task);
        }
    }

    fn emit(&self, element_id: Arc<str>, event: ViewEvent, cx: &mut App) {
        (self.sink)(
            self.extension_id.clone(),
            self.view_id.clone(),
            element_id,
            event,
            cx,
        );
    }

    fn submit_input(&mut self, id: Arc<str>, cx: &mut Context<Self>) {
        let Some(input) = self.inputs.get_mut(&id) else {
            return;
        };
        let text = input.editor.read(cx).text(cx);
        // Flush a pending change first, so the extension sees the final text
        // as `input-changed` before `submitted`.
        if input.pending_change.take().is_some() {
            self.emit(id.clone(), ViewEvent::InputChanged(text.clone()), cx);
        }
        self.emit(id, ViewEvent::Submitted(text), cx);
    }

    #[cfg(test)]
    fn element_focus_handle(&self, element_id: &str, cx: &App) -> Option<FocusHandle> {
        if let Some(input) = self.inputs.get(element_id) {
            return Some(input.editor.focus_handle(cx));
        }
        self.element_focus.get(element_id).cloned()
    }

    fn render_node(
        &self,
        view: &LatheView,
        index: u32,
        in_row: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let node = view.node(index);
        match &node.kind {
            NodeKind::Column => v_flex()
                .w_full()
                .when(index == 0, |root| root.size_full())
                .gap_1()
                .children(
                    node.children
                        .iter()
                        .map(|child| self.render_node(view, *child, false, window, cx)),
                )
                .into_any_element(),
            NodeKind::Row => h_flex()
                .w_full()
                .gap_1()
                .items_center()
                .children(
                    node.children
                        .iter()
                        .map(|child| self.render_node(view, *child, true, window, cx)),
                )
                .into_any_element(),
            // Grows into free space and shrinks to scroll when its parent has a
            // bounded height. `flex_1` would give it zero height inside a
            // column that sizes to its content.
            // Scrolls both ways. The inner column is as wide as its widest
            // row (and at least as wide as the scroll area), so long rows
            // scroll sideways instead of being cut off, and short rows still
            // fill the width.
            NodeKind::Scroll => v_flex()
                .id(("lathe-view-scroll", index as usize))
                .debug_selector(move || format!("lathe-view-scroll-{index}"))
                .w_full()
                .when(index == 0, |root| root.size_full())
                .flex_auto()
                .min_h_0()
                .items_start()
                .overflow_scroll()
                .child(
                    v_flex()
                        .debug_selector(move || format!("lathe-view-scroll-content-{index}"))
                        .min_w_full()
                        .flex_none()
                        .gap_1()
                        .whitespace_nowrap()
                        .children(
                            node.children
                                .iter()
                                .map(|child| self.render_node(view, *child, false, window, cx)),
                        ),
                )
                .into_any_element(),
            NodeKind::Text { text, style, tone } => {
                let color = tone_color(*tone);
                match style {
                    TextStyle::Heading => Headline::new(text.to_string())
                        .size(HeadlineSize::XSmall)
                        .color(color)
                        .into_any_element(),
                    TextStyle::Body => Label::new(text.to_string()).color(color).into_any_element(),
                    TextStyle::Muted => Label::new(text.to_string())
                        .color(if *tone == Tone::Neutral {
                            Color::Muted
                        } else {
                            color
                        })
                        .into_any_element(),
                    TextStyle::Code => Label::new(text.to_string())
                        .buffer_font(cx)
                        .color(color)
                        .into_any_element(),
                }
            }
            NodeKind::Badge { text, tone } => Chip::new(text.to_string())
                .label_color(tone_color(*tone))
                .into_any_element(),
            NodeKind::Button {
                id,
                label,
                icon,
                tooltip,
                disabled,
            } => {
                let mut button = Button::new(element_id(&self.view_id, id), label.to_string())
                    .disabled(*disabled)
                    .on_click(cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| this.emit(id.clone(), ViewEvent::Clicked, cx)
                    }));
                if let Some(icon) = icon.as_deref().and_then(icon_name) {
                    button = button.start_icon(Icon::new(icon).size(IconSize::Small));
                }
                if let Some(tooltip) = tooltip {
                    button = button.tooltip(Tooltip::text(tooltip.to_string()));
                }
                self.focusable_wrapper(
                    id,
                    *disabled,
                    ViewEvent::Clicked,
                    button.into_any_element(),
                    cx,
                )
            }
            kind @ NodeKind::ListItem { .. } => self.render_list_item(kind, cx),
            NodeKind::Checkbox {
                id,
                label,
                checked,
                disabled,
            } => {
                let checked = *checked;
                let checkbox = Checkbox::new(
                    element_id(&self.view_id, id),
                    if checked {
                        ToggleState::Selected
                    } else {
                        ToggleState::Unselected
                    },
                )
                .label(label.to_string())
                .disabled(*disabled)
                .on_click(cx.listener({
                    let id = id.clone();
                    move |this, _, _, cx| this.emit(id.clone(), ViewEvent::Toggled(!checked), cx)
                }));
                self.focusable_wrapper(
                    id,
                    *disabled,
                    ViewEvent::Toggled(!checked),
                    checkbox.into_any_element(),
                    cx,
                )
            }
            NodeKind::TextInput { id, .. } => {
                let Some(input) = self.inputs.get(id) else {
                    return div().into_any_element();
                };
                let focus = input
                    .editor
                    .focus_handle(cx)
                    .tab_index(input.tab_index)
                    .tab_stop(!input.disabled);
                div()
                    .track_focus(&focus)
                    .debug_selector({
                        let id = id.clone();
                        move || format!("lathe-view-element-{id}")
                    })
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .on_action(cx.listener({
                        let id = id.clone();
                        move |this, _: &menu::Confirm, _, cx| this.submit_input(id.clone(), cx)
                    }))
                    .child(input.editor.clone())
                    .into_any_element()
            }
            NodeKind::Select {
                id,
                options,
                selected,
                placeholder,
                disabled,
            } => self.render_select(
                id,
                options,
                selected.as_deref(),
                placeholder.as_deref(),
                *disabled,
                window,
                cx,
            ),
            NodeKind::VirtualList { id, row_count, .. } => {
                let Some(state) = self.virtual_lists.get(id) else {
                    return div().into_any_element();
                };
                let list_id = id.clone();
                v_flex()
                    .debug_selector({
                        let id = id.clone();
                        move || format!("lathe-view-element-{id}")
                    })
                    .w_full()
                    .flex_auto()
                    .min_h_0()
                    .child(
                        uniform_list(
                            gpui::ElementId::Name(
                                format!("lathe-view-list:{}:{id}", self.view_id).into(),
                            ),
                            *row_count as usize,
                            cx.processor(move |this, range, _window, cx| {
                                this.render_rows(&list_id, range, cx)
                            }),
                        )
                        .track_scroll(&state.scroll)
                        .flex_grow_1(),
                    )
                    .into_any_element()
            }
            NodeKind::Progress { fraction, label } => {
                let label = label.as_ref().map(|label| {
                    Label::new(label.to_string())
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                });
                match fraction {
                    Some(fraction) => v_flex()
                        .w_full()
                        .gap_1()
                        .children(label)
                        .child(ProgressBar::new(
                            ("lathe-view-progress", index as usize),
                            *fraction,
                            1.0,
                            cx,
                        ))
                        .into_any_element(),
                    None => h_flex()
                        .gap_1()
                        .child(SpinnerLabel::new().size(LabelSize::Small))
                        .children(label)
                        .into_any_element(),
                }
            }
            NodeKind::Markdown(_) => {
                let (Some(markdown), Some(style)) =
                    (self.markdown.get(&index), self.markdown_style.clone())
                else {
                    return div().into_any_element();
                };
                div()
                    .w_full()
                    .child(MarkdownElement::new(markdown.clone(), style).on_url_click(
                        |url, _, cx| {
                            if is_openable_url(&url) {
                                cx.open_url(&url);
                            }
                        },
                    ))
                    .into_any_element()
            }
            NodeKind::Icon(name) => match icon_name(name) {
                Some(icon) => Icon::new(icon).size(IconSize::Small).into_any_element(),
                None => div().into_any_element(),
            },
            NodeKind::Divider => {
                if in_row {
                    Divider::vertical().into_any_element()
                } else {
                    Divider::horizontal().into_any_element()
                }
            }
            NodeKind::Spacer => div().flex_1().into_any_element(),
        }
    }

    /// Rows `range` of a virtual list: loaded rows as list items, others as
    /// placeholders while their page is fetched.
    fn render_rows(
        &mut self,
        list_id: &Arc<str>,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut missing_pages = Vec::new();
        let mut rows = Vec::with_capacity(range.len());
        for index in range {
            let index = index as u32;
            let page = index / ROWS_PER_REQUEST;
            let row = self
                .virtual_lists
                .get(list_id)
                .and_then(|state| state.pages.get(&page))
                .and_then(|rows| rows.get((index % ROWS_PER_REQUEST) as usize))
                .cloned();
            rows.push(match row {
                Some(row) => self.render_list_item(&row, cx),
                None => {
                    if !missing_pages.contains(&page) {
                        missing_pages.push(page);
                    }
                    ListItem::new(("lathe-view-row-loading", index as usize))
                        .spacing(ListItemSpacing::Sparse)
                        .child(Label::new("…").color(Color::Muted))
                        .into_any_element()
                }
            });
        }
        for page in missing_pages {
            self.request_page(list_id.clone(), page, cx);
        }
        rows
    }

    fn request_page(&mut self, list_id: Arc<str>, page: u32, cx: &mut Context<Self>) {
        let Some(state) = self.virtual_lists.get_mut(&list_id) else {
            return;
        };
        if !state.requested.insert(page) {
            return;
        }
        let revision = state.revision;
        let task = (self.row_source)(
            self.extension_id.clone(),
            self.view_id.clone(),
            list_id.clone(),
            page * ROWS_PER_REQUEST,
            ROWS_PER_REQUEST,
            cx,
        );
        cx.spawn(async move |this, cx| {
            let rows = task.await;
            this.update(cx, |this, cx| {
                let Some(state) = this.virtual_lists.get_mut(&list_id) else {
                    return;
                };
                // Rows for an older revision are stale.
                if state.revision != revision {
                    return;
                }
                match rows {
                    Ok(rows) => {
                        state.requested.remove(&page);
                        state.pages.insert(page, rows);
                    }
                    // Stays in `requested`, so a failing extension isn't
                    // asked again until the list's revision changes.
                    Err(error) => log::error!("virtual list {list_id:?}: {error:#}"),
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn render_list_item(&self, kind: &NodeKind, cx: &mut Context<Self>) -> AnyElement {
        let NodeKind::ListItem {
            id,
            label,
            detail,
            icon,
            tooltip,
            selected,
            disabled,
            indent,
            disclosure,
            menu,
        } = kind
        else {
            return div().into_any_element();
        };
        // `ListItem` draws its arrow 1rem left of the row's content, so tree
        // rows get one extra indent step to make room for it; otherwise a
        // top-level row's arrow lands outside the row.
        let in_tree = *disclosure != Disclosure::None || *indent > 0;
        let mut item = ListItem::new(element_id(&self.view_id, id))
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(*selected)
            .disabled(*disabled)
            .indent_level(*indent as usize + usize::from(in_tree))
            .indent_step_size(TREE_INDENT_STEP)
            .start_slot::<Icon>(
                icon.as_deref()
                    .and_then(icon_name)
                    .map(|icon| Icon::new(icon).size(IconSize::Small).color(Color::Muted)),
            )
            .child(Label::new(label.to_string()))
            .end_slot::<Label>(detail.as_ref().map(|detail| {
                Label::new(detail.to_string())
                    .size(LabelSize::Small)
                    .color(Color::Muted)
            }))
            .on_click(cx.listener({
                let id = id.clone();
                move |this, _, _, cx| this.emit(id.clone(), ViewEvent::Clicked, cx)
            }));
        if *disclosure != Disclosure::None {
            let expanded = *disclosure == Disclosure::Expanded;
            // Expanded rows otherwise hide their arrow until hovered.
            item = item
                .always_show_disclosure_icon(true)
                .toggle(expanded)
                .on_toggle(cx.listener({
                    let id = id.clone();
                    move |this, _, _, cx| this.emit(id.clone(), ViewEvent::Toggled(!expanded), cx)
                }));
        }
        if let Some(tooltip) = tooltip {
            item = item.tooltip(Tooltip::text(tooltip.to_string()));
        }
        let item = if menu.is_empty() || *disabled {
            item.into_any_element()
        } else {
            self.with_menu(id, menu, item.into_any_element(), cx)
        };
        self.focusable_wrapper(id, *disabled, ViewEvent::Clicked, item, cx)
    }

    /// Opens `menu` when `child` is right-clicked; choosing an entry sends
    /// `menu-chosen` for the element `id`.
    fn with_menu(
        &self,
        id: &Arc<str>,
        menu: &[MenuEntry],
        child: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let id = id.clone();
        let entries = menu.to_vec();
        right_click_menu(gpui::ElementId::Name(
            format!("lathe-view-menu:{}:{id}", self.view_id).into(),
        ))
        .trigger(move |_, _, _| child)
        .menu(move |window, cx| entry_menu(this.clone(), id.clone(), &entries, window, cx))
        .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_select(
        &self,
        id: &Arc<str>,
        options: &[SelectOption],
        selected: Option<&str>,
        placeholder: Option<&str>,
        disabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = selected
            .and_then(|selected| {
                options
                    .iter()
                    .find(|option| option.value.as_ref() == selected)
            })
            .map(|option| option.label.to_string())
            .or_else(|| placeholder.map(str::to_string))
            .unwrap_or_else(|| "Choose…".to_string());
        let menu = select_menu(
            cx.entity().downgrade(),
            id.clone(),
            options,
            selected,
            window,
            cx,
        );
        div()
            .debug_selector({
                let id = id.clone();
                move || format!("lathe-view-element-{id}")
            })
            .child(DropdownMenu::new(element_id(&self.view_id, id), label, menu).disabled(disabled))
            .into_any_element()
    }

    /// Wraps an element so it can take keyboard focus (Tab) and fires
    /// `on_enter` when Enter is pressed while it's focused.
    fn focusable_wrapper(
        &self,
        id: &Arc<str>,
        disabled: bool,
        on_enter: ViewEvent,
        child: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut wrapper = div().debug_selector({
            let id = id.clone();
            move || format!("lathe-view-element-{id}")
        });
        if let Some(handle) = self.element_focus.get(id) {
            wrapper = wrapper
                .track_focus(handle)
                .rounded_sm()
                .focus(|style| style.border_1().border_color(gpui::blue()));
        }
        if !disabled {
            wrapper = wrapper.on_action(cx.listener({
                let id = id.clone();
                move |this, _: &menu::Confirm, _, cx| this.emit(id.clone(), on_enter.clone(), cx)
            }));
        }
        wrapper.child(child).into_any_element()
    }
}

impl Focusable for ExtensionViewHost {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ExtensionViewHost {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = self.current_view(cx);
        if let Some(view) = &view {
            self.prepare(view, window, cx);
        }
        if self.markdown_style.is_none() && !self.markdown.is_empty() {
            self.markdown_style = Some(MarkdownStyle::themed(MarkdownFont::Agent, window, cx));
        }
        v_flex()
            .key_context("ExtensionView")
            .track_focus(&self.focus_handle)
            .size_full()
            .on_action(cx.listener(|_, _: &menu::SelectNext, window, cx| window.focus_next(cx)))
            .on_action(cx.listener(|_, _: &menu::SelectPrevious, window, cx| window.focus_prev(cx)))
            .children(view.map(|view| self.render_node(&view, 0, false, window, cx)))
    }
}

/// A list item's right-click menu: each entry sends `menu-chosen`.
fn entry_menu(
    this: gpui::WeakEntity<ExtensionViewHost>,
    id: Arc<str>,
    entries: &[MenuEntry],
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    ContextMenu::build(window, cx, |mut menu, _, _| {
        for entry in entries {
            let this = this.clone();
            let id = id.clone();
            let entry_id = entry.id.to_string();
            menu = menu.entry(entry.label.to_string(), None, move |_, cx| {
                this.update(cx, |this, cx| {
                    this.emit(id.clone(), ViewEvent::MenuChosen(entry_id.clone()), cx)
                })
                .log_err();
            });
        }
        menu
    })
}

/// A select's dropdown: each option sends `selected`, and the chosen one is
/// checked.
fn select_menu(
    this: gpui::WeakEntity<ExtensionViewHost>,
    id: Arc<str>,
    options: &[SelectOption],
    selected: Option<&str>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    ContextMenu::build(window, cx, |mut menu, _, _| {
        for option in options {
            let this = this.clone();
            let id = id.clone();
            let value = option.value.to_string();
            menu = menu.toggleable_entry(
                option.label.to_string(),
                Some(option.value.as_ref()) == selected,
                ui::IconPosition::Start,
                None,
                move |_, cx| {
                    this.update(cx, |this, cx| {
                        this.emit(id.clone(), ViewEvent::Selected(value.clone()), cx)
                    })
                    .log_err();
                },
            );
        }
        menu
    })
}

/// Markdown links extensions may open: web pages and email, never local
/// files or app URLs.
fn is_openable_url(url: &str) -> bool {
    let url = url.trim_start().to_ascii_lowercase();
    ["https://", "http://", "mailto:"]
        .iter()
        .any(|scheme| url.starts_with(scheme))
}

fn element_id(view_id: &str, id: &str) -> gpui::ElementId {
    gpui::ElementId::Name(format!("lathe-view:{view_id}:{id}").into())
}

pub(crate) fn tone_color(tone: Tone) -> Color {
    match tone {
        Tone::Neutral => Color::Default,
        Tone::Accent => Color::Accent,
        Tone::Success => Color::Success,
        Tone::Warning => Color::Warning,
        Tone::Error => Color::Error,
    }
}

/// Lathe's icon for a snake_case name such as `"plus"` or `"file_code"`.
/// Unknown names render nothing rather than failing the view.
pub fn icon_name(name: &str) -> Option<IconName> {
    IconName::from_str(name).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use extension_host::wasm_host::lathe_views::{Node, validate};
    use gpui::{TestAppContext, VisualTestContext};
    use parking_lot::Mutex;
    use settings::SettingsStore;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
        });
    }

    fn leaf(kind: NodeKind) -> Node {
        Node {
            kind,
            children: Vec::new(),
        }
    }

    fn sample_view(input_value: &str, checked: bool) -> LatheView {
        validate(vec![
            Node {
                kind: NodeKind::Column,
                children: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
            },
            leaf(NodeKind::Text {
                text: "Heading".into(),
                style: TextStyle::Heading,
                tone: Tone::Neutral,
            }),
            leaf(NodeKind::Button {
                id: "go".into(),
                label: "Go".into(),
                icon: Some("plus".into()),
                tooltip: Some("Go somewhere".into()),
                disabled: false,
            }),
            leaf(NodeKind::TextInput {
                id: "filter".into(),
                value: input_value.into(),
                placeholder: Some("Filter".into()),
                disabled: false,
            }),
            leaf(NodeKind::Checkbox {
                id: "hidden".into(),
                label: "Show hidden".into(),
                checked,
                disabled: false,
            }),
            leaf(NodeKind::Badge {
                text: "3".into(),
                tone: Tone::Warning,
            }),
            leaf(NodeKind::Text {
                text: "error".into(),
                style: TextStyle::Code,
                tone: Tone::Error,
            }),
            leaf(NodeKind::Icon("not_an_icon".into())),
            leaf(NodeKind::Divider),
            leaf(NodeKind::Spacer),
            Node {
                kind: NodeKind::Scroll,
                children: vec![11, 12],
            },
            leaf(NodeKind::ListItem {
                id: "src".into(),
                label: "src".into(),
                detail: None,
                icon: Some("folder".into()),
                tooltip: None,
                selected: false,
                disabled: false,
                indent: 0,
                disclosure: Disclosure::Collapsed,
                menu: Vec::new(),
            }),
            leaf(NodeKind::ListItem {
                id: "main".into(),
                label: "main.rs".into(),
                detail: Some("2 KB".into()),
                icon: None,
                tooltip: Some("src/main.rs".into()),
                selected: true,
                disabled: false,
                indent: 1,
                disclosure: Disclosure::None,
                menu: Vec::new(),
            }),
        ])
        .expect("valid view")
    }

    type Recorded = Arc<Mutex<Vec<(String, ViewEvent)>>>;

    fn open(
        view: LatheView,
        cx: &mut TestAppContext,
    ) -> (
        Entity<ExtensionViewHost>,
        Entity<ExtensionViews>,
        Recorded,
        &mut VisualTestContext,
    ) {
        let views = cx.new(|_| ExtensionViews::default());
        views.update(cx, |views, cx| {
            views.set_view("ext".into(), "main".into(), view);
            cx.notify();
        });
        let recorded = Recorded::default();
        let sink: ViewEventSink = {
            let recorded = recorded.clone();
            Arc::new(move |extension_id, view_id, element_id, event, _cx| {
                assert_eq!((&*extension_id, &*view_id), ("ext", "main"));
                recorded.lock().push((element_id.to_string(), event));
            })
        };
        let (host, cx) = cx.add_window_view({
            let views = views.clone();
            move |window, cx| {
                ExtensionViewHost::with_sink("ext".into(), "main".into(), views, sink, window, cx)
            }
        });
        cx.run_until_parked();
        (host, views, recorded, cx)
    }

    fn click(selector: &str, cx: &mut VisualTestContext) {
        // `debug_bounds` wants a `&'static str`; leaking a few test strings is fine.
        let name: &'static str =
            Box::leak(format!("lathe-view-element-{selector}").into_boxed_str());
        let bounds = cx
            .debug_bounds(name)
            .unwrap_or_else(|| panic!("{selector} was not rendered"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();
    }

    fn take(recorded: &Recorded) -> Vec<(String, ViewEvent)> {
        std::mem::take(&mut *recorded.lock())
    }

    #[test]
    fn test_icon_names() {
        assert_eq!(icon_name("plus"), Some(IconName::Plus));
        assert_eq!(icon_name("not_an_icon"), None);
    }

    #[gpui::test]
    async fn test_long_rows_scroll_sideways(cx: &mut TestAppContext) {
        init_test(cx);
        let long = "TODO: ".to_string() + &"a very long comment that goes on ".repeat(40);
        let row = |id: &str, label: &str| {
            leaf(NodeKind::ListItem {
                id: id.into(),
                label: label.into(),
                detail: None,
                icon: None,
                tooltip: None,
                selected: false,
                disabled: false,
                indent: 0,
                disclosure: Disclosure::None,
                menu: Vec::new(),
            })
        };
        let view = validate(vec![
            Node {
                kind: NodeKind::Scroll,
                children: vec![1, 2],
            },
            row("long", &long),
            row("short", "short"),
        ])
        .expect("valid view");
        let (_host, _views, _recorded, cx) = open(view, cx);
        let area = cx.debug_bounds("lathe-view-scroll-0").expect("scroll area");
        let content = cx
            .debug_bounds("lathe-view-scroll-content-0")
            .expect("scroll content");
        let long_row = cx
            .debug_bounds("lathe-view-element-long")
            .expect("long row");
        let short_row = cx
            .debug_bounds("lathe-view-element-short")
            .expect("short row");
        assert!(
            long_row.size.width > area.size.width,
            "the long row keeps its width ({:?}) instead of being squeezed into the \
             scroll area ({:?})",
            long_row.size.width,
            area.size.width
        );
        assert_eq!(content.size.width, long_row.size.width);
        assert_eq!(
            short_row.size.width, long_row.size.width,
            "short rows stretch to the widest row, so highlights line up"
        );
    }

    #[gpui::test]
    async fn test_tree_arrow_is_inside_the_row(cx: &mut TestAppContext) {
        init_test(cx);
        // `src` is a top-level tree row, expanded or collapsed.
        for disclosure in [Disclosure::Expanded, Disclosure::Collapsed] {
            let view = validate(vec![
                Node {
                    kind: NodeKind::Column,
                    children: vec![1],
                },
                leaf(NodeKind::ListItem {
                    id: "src".into(),
                    label: "src".into(),
                    detail: None,
                    icon: None,
                    tooltip: None,
                    selected: false,
                    disabled: false,
                    indent: 0,
                    disclosure,
                    menu: Vec::new(),
                }),
            ])
            .expect("valid view");
            let (_host, _views, recorded, cx) = open(view, cx);
            let bounds = cx
                .debug_bounds("lathe-view-element-src")
                .expect("the row is rendered");
            // Without hovering, near the row's left edge: the arrow.
            let near_left = gpui::point(bounds.origin.x + px(14.), bounds.center().y);
            cx.simulate_click(near_left, gpui::Modifiers::default());
            cx.run_until_parked();
            assert_eq!(
                take(&recorded),
                vec![(
                    "src".to_string(),
                    ViewEvent::Toggled(disclosure == Disclosure::Collapsed)
                )],
                "the arrow is visible and clickable at the row's left edge ({disclosure:?})"
            );
            cx.simulate_click(bounds.center(), gpui::Modifiers::default());
            cx.run_until_parked();
            assert_eq!(
                take(&recorded),
                vec![("src".to_string(), ViewEvent::Clicked)]
            );
        }
    }

    #[gpui::test]
    async fn test_clicks_reach_the_extension(cx: &mut TestAppContext) {
        init_test(cx);
        let (_host, _views, recorded, cx) = open(sample_view("", false), cx);

        click("go", cx);
        click("hidden", cx);
        click("main", cx);
        assert_eq!(
            take(&recorded),
            vec![
                ("go".to_string(), ViewEvent::Clicked),
                ("hidden".to_string(), ViewEvent::Toggled(true)),
                ("main".to_string(), ViewEvent::Clicked),
            ]
        );
    }

    #[gpui::test]
    async fn test_typing_and_enter(cx: &mut TestAppContext) {
        init_test(cx);
        let (host, views, recorded, cx) = open(sample_view("", false), cx);
        let editor_handle = host.read_with(cx, |host, cx| {
            host.element_focus_handle("filter", cx).unwrap()
        });
        cx.update(|window, cx| window.focus(&editor_handle, cx));
        cx.simulate_input("rs");
        cx.run_until_parked();
        assert_eq!(take(&recorded), vec![], "nothing until typing pauses");

        cx.executor().advance_clock(INPUT_DEBOUNCE);
        cx.run_until_parked();
        assert_eq!(
            take(&recorded),
            vec![("filter".to_string(), ViewEvent::InputChanged("rs".into()))]
        );

        cx.simulate_input("t");
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        assert_eq!(
            take(&recorded),
            vec![
                ("filter".to_string(), ViewEvent::InputChanged("rst".into())),
                ("filter".to_string(), ViewEvent::Submitted("rst".into())),
            ],
            "Enter flushes the pending change before submitting"
        );
        cx.executor().advance_clock(INPUT_DEBOUNCE * 2);
        cx.run_until_parked();
        assert_eq!(
            take(&recorded),
            vec![],
            "the flushed change isn't sent twice"
        );

        // While the user is in the field, a value from the extension doesn't
        // overwrite what they typed.
        let editor_text = |host: &Entity<ExtensionViewHost>, cx: &mut VisualTestContext| {
            host.read_with(cx, |host, cx| {
                host.inputs["filter"].editor.read(cx).text(cx)
            })
        };
        views.update(cx, |views, cx| {
            views.set_view(
                "ext".into(),
                "main".into(),
                sample_view("from extension", false),
            );
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(editor_text(&host, cx), "rst");

        let host_focus = host.read_with(cx, |host, cx| host.focus_handle(cx));
        cx.update(|window, cx| window.focus(&host_focus, cx));
        views.update(cx, |views, cx| {
            views.set_view("ext".into(), "main".into(), sample_view("synced", false));
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            editor_text(&host, cx),
            "synced",
            "once unfocused, it takes the new value"
        );
    }

    #[gpui::test]
    async fn test_keyboard_focus_and_enter(cx: &mut TestAppContext) {
        init_test(cx);
        let (host, _views, recorded, cx) = open(sample_view("", false), cx);
        let host_focus = host.read_with(cx, |host, cx| host.focus_handle(cx));
        cx.update(|window, cx| window.focus(&host_focus, cx));

        let focused = |id: &str, host: &Entity<ExtensionViewHost>, cx: &mut VisualTestContext| {
            let handle = host.read_with(cx, |host, cx| host.element_focus_handle(id, cx).unwrap());
            cx.update(|window, _| handle.is_focused(window))
        };

        cx.dispatch_action(menu::SelectNext);
        cx.run_until_parked();
        assert!(focused("go", &host, cx), "Tab reaches the first button");
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        assert_eq!(
            take(&recorded),
            vec![("go".to_string(), ViewEvent::Clicked)]
        );

        cx.dispatch_action(menu::SelectNext);
        cx.run_until_parked();
        assert!(focused("filter", &host, cx), "then the text input");
    }

    #[gpui::test]
    async fn test_removed_view_renders_nothing(cx: &mut TestAppContext) {
        init_test(cx);
        let (_host, views, _recorded, cx) = open(sample_view("", false), cx);
        assert!(cx.debug_bounds("lathe-view-element-go").is_some());
        views.update(cx, |views, cx| {
            views.remove("ext".into(), "main".into());
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(cx.debug_bounds("lathe-view-element-go").is_none());
    }
}

#[cfg(test)]
mod node_tests {
    use super::*;
    use crate::lathe_ui::SurfaceEventSink;
    use extension_host::wasm_host::lathe::LatheSurfaceEvent;
    use extension_host::wasm_host::lathe_views::{Node, validate};
    use gpui::{TestAppContext, VisualTestContext};
    use parking_lot::Mutex;
    use settings::SettingsStore;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
        });
    }

    fn leaf(kind: NodeKind) -> Node {
        Node {
            kind,
            children: Vec::new(),
        }
    }

    fn view(markdown: &str, sort: Option<&str>) -> LatheView {
        validate(vec![
            Node {
                kind: NodeKind::Column,
                children: vec![1, 2, 3, 4, 5],
            },
            leaf(NodeKind::ListItem {
                id: "todo".into(),
                label: "A TODO".into(),
                detail: None,
                icon: None,
                tooltip: None,
                selected: false,
                disabled: false,
                indent: 0,
                disclosure: Disclosure::None,
                menu: vec![
                    MenuEntry {
                        id: "done".into(),
                        label: "Mark Done".into(),
                    },
                    MenuEntry {
                        id: "skip".into(),
                        label: "Skip".into(),
                    },
                ],
            }),
            leaf(NodeKind::Select {
                id: "sort".into(),
                options: vec![
                    SelectOption {
                        value: "name".into(),
                        label: "Name".into(),
                    },
                    SelectOption {
                        value: "size".into(),
                        label: "Size".into(),
                    },
                ],
                selected: sort.map(Into::into),
                placeholder: Some("Sort by".into()),
                disabled: false,
            }),
            leaf(NodeKind::Progress {
                fraction: Some(0.4),
                label: Some("Indexing".into()),
            }),
            leaf(NodeKind::Progress {
                fraction: None,
                label: None,
            }),
            leaf(NodeKind::Markdown(markdown.into())),
        ])
        .expect("valid view")
    }

    type Recorded = Arc<Mutex<Vec<(String, ViewEvent)>>>;
    type Surfaces = Arc<Mutex<Vec<LatheSurfaceEvent>>>;

    fn open(
        cx: &mut TestAppContext,
    ) -> (
        Entity<ExtensionViewHost>,
        Entity<ExtensionViews>,
        Recorded,
        Surfaces,
        &mut VisualTestContext,
    ) {
        let surfaces = Surfaces::default();
        let surface_sink: SurfaceEventSink = {
            let surfaces = surfaces.clone();
            Arc::new(move |_, event, _| surfaces.lock().push(event))
        };
        let views = cx.new(|_| ExtensionViews::with_surface_sink(surface_sink));
        views.update(cx, |views, cx| {
            views.set_view("ext".into(), "main".into(), view("**Hi**", None));
            cx.notify();
        });
        let recorded = Recorded::default();
        let sink: ViewEventSink = {
            let recorded = recorded.clone();
            Arc::new(move |_, _, element_id, event, _| {
                recorded.lock().push((element_id.to_string(), event))
            })
        };
        let (host, cx) = cx.add_window_view({
            let views = views.clone();
            move |window, cx| {
                ExtensionViewHost::with_sink("ext".into(), "main".into(), views, sink, window, cx)
            }
        });
        cx.run_until_parked();
        (host, views, recorded, surfaces, cx)
    }

    #[test]
    fn test_openable_urls() {
        assert!(is_openable_url("https://lathe-ide.com"));
        assert!(is_openable_url("MAILTO:someone@example.com"));
        assert!(!is_openable_url("file:///etc/passwd"));
        assert!(!is_openable_url("zed://settings"));
        assert!(!is_openable_url("javascript:alert(1)"));
    }

    /// Picks the last entry of a menu the way the keyboard does.
    fn choose_last(menu: &Entity<ContextMenu>, cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            menu.update(cx, |menu, cx| {
                menu.select_last(window, cx);
                menu.confirm(&menu::Confirm, window, cx);
            })
        });
        cx.run_until_parked();
    }

    // Opening these menus is the `ui` crate's job, and their focus moves on a
    // platform frame callback that test windows don't run, so these build the
    // menus the renderer hands to `right_click_menu` and `DropdownMenu`.
    #[gpui::test]
    async fn test_right_click_menu(cx: &mut TestAppContext) {
        init_test(cx);
        let (host, _views, recorded, _surfaces, cx) = open(cx);
        assert!(
            cx.debug_bounds("lathe-view-element-todo").is_some(),
            "the list item with a menu renders"
        );
        let entries = [
            MenuEntry {
                id: "done".into(),
                label: "Mark Done".into(),
            },
            MenuEntry {
                id: "skip".into(),
                label: "Skip".into(),
            },
        ];
        let menu = cx
            .update(|window, cx| entry_menu(host.downgrade(), "todo".into(), &entries, window, cx));
        choose_last(&menu, cx);
        assert_eq!(
            std::mem::take(&mut *recorded.lock()),
            vec![("todo".to_string(), ViewEvent::MenuChosen("skip".into()))]
        );
    }

    #[gpui::test]
    async fn test_select(cx: &mut TestAppContext) {
        init_test(cx);
        let (host, _views, recorded, _surfaces, cx) = open(cx);
        assert!(cx.debug_bounds("lathe-view-element-sort").is_some());
        let options = [
            SelectOption {
                value: "name".into(),
                label: "Name".into(),
            },
            SelectOption {
                value: "size".into(),
                label: "Size".into(),
            },
        ];
        let menu = cx.update(|window, cx| {
            select_menu(
                host.downgrade(),
                "sort".into(),
                &options,
                Some("name"),
                window,
                cx,
            )
        });
        choose_last(&menu, cx);
        assert_eq!(
            std::mem::take(&mut *recorded.lock()),
            vec![("sort".to_string(), ViewEvent::Selected("size".into()))]
        );
    }

    #[gpui::test]
    async fn test_markdown_follows_updates(cx: &mut TestAppContext) {
        init_test(cx);
        let (host, views, _recorded, _surfaces, cx) = open(cx);
        let sources = |host: &Entity<ExtensionViewHost>, cx: &mut VisualTestContext| {
            host.read_with(cx, |host, cx| {
                host.markdown
                    .values()
                    .map(|markdown| markdown.read(cx).source().to_string())
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(sources(&host, cx), vec!["**Hi**".to_string()]);
        views.update(cx, |views, cx| {
            views.set_view("ext".into(), "main".into(), view("_Bye_", Some("name")));
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(sources(&host, cx), vec!["_Bye_".to_string()]);
    }

    #[gpui::test]
    async fn test_shown_while_a_host_exists(cx: &mut TestAppContext) {
        init_test(cx);
        let (_host, views, _recorded, surfaces, cx) = open(cx);
        assert_eq!(
            std::mem::take(&mut *surfaces.lock()),
            vec![LatheSurfaceEvent::ViewShown("main".into())]
        );
        let sink: ViewEventSink = Arc::new(|_, _, _, _, _| {});
        let new_host = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                let views = views.clone();
                let sink = sink.clone();
                cx.new(|cx| {
                    ExtensionViewHost::with_sink(
                        "ext".into(),
                        "other".into(),
                        views,
                        sink,
                        window,
                        cx,
                    )
                })
            })
        };
        let first = new_host(cx);
        let second = new_host(cx);
        cx.run_until_parked();
        drop(first);
        cx.update(|_, _| {});
        cx.run_until_parked();
        assert_eq!(
            std::mem::take(&mut *surfaces.lock()),
            vec![LatheSurfaceEvent::ViewShown("other".into())],
            "shown once for two hosts, and still shown while one remains"
        );
        drop(second);
        // Released entities are only cleaned up when effects next flush.
        cx.update(|_, _| {});
        cx.run_until_parked();
        assert_eq!(
            std::mem::take(&mut *surfaces.lock()),
            vec![LatheSurfaceEvent::ViewHidden("other".into())]
        );
    }
}

#[cfg(test)]
mod virtual_list_tests {
    use super::*;
    use extension_host::wasm_host::lathe_views::{Node, validate};
    use gpui::{Modifiers, TestAppContext, VisualTestContext};
    use parking_lot::Mutex;
    use settings::SettingsStore;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
        });
    }

    fn list_view(revision: u32) -> LatheView {
        validate(vec![
            Node {
                kind: NodeKind::Column,
                children: vec![1],
            },
            Node {
                kind: NodeKind::VirtualList {
                    id: "files".into(),
                    row_count: 100_000,
                    revision,
                },
                children: Vec::new(),
            },
        ])
        .expect("valid view")
    }

    fn row(index: u32, revision: u32) -> NodeKind {
        NodeKind::ListItem {
            id: format!("row-{index}").into(),
            label: format!("r{revision} file {index}").into(),
            detail: None,
            icon: None,
            tooltip: None,
            selected: false,
            disabled: false,
            indent: 0,
            disclosure: Disclosure::None,
            menu: Vec::new(),
        }
    }

    #[gpui::test]
    async fn test_rows_load_by_page(cx: &mut TestAppContext) {
        init_test(cx);
        let views = cx.new(|_| ExtensionViews::default());
        views.update(cx, |views, cx| {
            views.set_view("ext".into(), "main".into(), list_view(1));
            cx.notify();
        });
        let requests = Arc::new(Mutex::new(Vec::new()));
        let row_source: RowSource = {
            let requests = requests.clone();
            let views = views.clone();
            Arc::new(move |_, _, list_id, start, count, cx| {
                assert_eq!(&*list_id, "files");
                requests.lock().push((start, count));
                let revision = match views
                    .read(cx)
                    .view("ext", "main")
                    .map(|view| view.node(1).kind.clone())
                {
                    Some(NodeKind::VirtualList { revision, .. }) => revision,
                    _ => 0,
                };
                Task::ready(Ok((start..start + count)
                    .map(|index| row(index, revision))
                    .collect()))
            })
        };
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let sink: ViewEventSink = {
            let recorded = recorded.clone();
            Arc::new(move |_, _, element_id, event, _| {
                recorded.lock().push((element_id.to_string(), event))
            })
        };
        let (host, cx) = cx.add_window_view({
            let views = views.clone();
            move |window, cx| {
                ExtensionViewHost::with_sink("ext".into(), "main".into(), views, sink, window, cx)
                    .with_row_source(row_source)
            }
        });
        cx.run_until_parked();
        let redraw = |cx: &mut VisualTestContext| {
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
        };
        redraw(cx);
        assert_eq!(
            std::mem::take(&mut *requests.lock()),
            vec![(0, ROWS_PER_REQUEST)],
            "only the first page is on screen, out of 100,000 rows"
        );
        let label = |index: usize, host: &Entity<ExtensionViewHost>, cx: &mut VisualTestContext| {
            host.read_with(cx, |host, _| {
                host.virtual_lists["files"]
                    .pages
                    .get(&0)
                    .and_then(|rows| match rows.get(index) {
                        Some(NodeKind::ListItem { label, .. }) => Some(label.to_string()),
                        _ => None,
                    })
            })
        };
        assert_eq!(label(3, &host, cx).as_deref(), Some("r1 file 3"));

        let bounds = cx
            .debug_bounds("lathe-view-element-row-2")
            .expect("row 2 is drawn once its page arrives");
        cx.simulate_click(bounds.center(), Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            std::mem::take(&mut *recorded.lock()),
            vec![("row-2".to_string(), ViewEvent::Clicked)],
            "row events use the row's id"
        );

        host.update(cx, |host, cx| {
            host.virtual_lists["files"]
                .scroll
                .scroll_to_item(50_000, gpui::ScrollStrategy::Top);
            cx.notify();
        });
        redraw(cx);
        let page = 50_000 / ROWS_PER_REQUEST;
        assert!(
            requests
                .lock()
                .contains(&(page * ROWS_PER_REQUEST, ROWS_PER_REQUEST)),
            "scrolling far down asks for that page: {:?}",
            requests.lock()
        );
        requests.lock().clear();

        views.update(cx, |views, cx| {
            views.set_view("ext".into(), "main".into(), list_view(2));
            cx.notify();
        });
        redraw(cx);
        assert!(
            !requests.lock().is_empty(),
            "a new revision drops cached rows and asks again"
        );
        let refreshed = host.read_with(cx, |host, _| {
            host.virtual_lists["files"].pages.values().any(|rows| {
                rows.iter().any(|row| {
                    matches!(row, NodeKind::ListItem { label, .. } if label.starts_with("r2 "))
                })
            })
        });
        assert!(refreshed);
    }

    #[gpui::test]
    async fn test_failed_page_is_not_retried(cx: &mut TestAppContext) {
        init_test(cx);
        let views = cx.new(|_| ExtensionViews::default());
        views.update(cx, |views, cx| {
            views.set_view("ext".into(), "main".into(), list_view(1));
            cx.notify();
        });
        let calls = Arc::new(Mutex::new(0));
        let row_source: RowSource = {
            let calls = calls.clone();
            Arc::new(move |_, _, _, _, _, _| {
                *calls.lock() += 1;
                Task::ready(Err(anyhow::anyhow!("extension trapped")))
            })
        };
        let sink: ViewEventSink = Arc::new(|_, _, _, _, _| {});
        let (_host, cx) = cx.add_window_view({
            let views = views.clone();
            move |window, cx| {
                ExtensionViewHost::with_sink("ext".into(), "main".into(), views, sink, window, cx)
                    .with_row_source(row_source)
            }
        });
        for _ in 0..3 {
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
        }
        assert_eq!(
            *calls.lock(),
            1,
            "a failing page isn't requested on every frame"
        );
    }
}
