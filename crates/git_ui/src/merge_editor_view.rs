use editor::{Editor, EditorEvent, RowHighlightOptions};
use gpui::{
    AppContext as _, Entity, EntityId, EventEmitter, FocusHandle, Focusable, Hsla, SharedString,
    Subscription, WeakEntity, point,
};
use language::{Anchor, Bias, Buffer, BufferSnapshot, Language, Point};
use project::{ConflictRegion, ConflictSet, ConflictSetUpdate, Project, ProjectItem as _};
use std::{ops::Range, sync::Arc};
use ui::{Divider, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

/// Workspace item that presents every conflict in a single buffer with
/// side-by-side "ours" / "theirs" (plus optional base) panes and per-region
/// resolve buttons. Functionally equivalent to the inline conflict view, but
/// trades the gutter buttons for a dedicated full-pane surface that's easier
/// to scan when a file has many conflicts.
pub struct MergeEditorView {
    focus_handle: FocusHandle,
    buffer: Entity<Buffer>,
    conflict_set: Entity<ConflictSet>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    file_label: SharedString,
    /// Index of the conflict currently highlighted by Prev/Next navigation.
    /// Clamped on every render so resolving a conflict (which shrinks the
    /// list) doesn't leave this dangling past the end.
    selected_index: usize,
    view_mode: MergeViewMode,
    split: Option<SplitView>,
    /// Guards against scroll-sync feedback: while we propagate one pane's
    /// scroll position to the others, their echoed ScrollPositionChanged
    /// events must not trigger another round of propagation.
    syncing_split_scroll: bool,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, Copy, PartialEq)]
enum MergeViewMode {
    Cards,
    Split,
}

/// Marker type for the row highlight showing the selected conflict in the
/// split panes.
struct SelectedConflictRows;

struct SplitPane {
    editor: Entity<Editor>,
    buffer: Entity<Buffer>,
    /// Buffer-row span of each conflict's kept content in this pane's derived
    /// text, in conflict order. Drives scroll alignment across panes and the
    /// selected-conflict highlight.
    conflict_rows: Vec<Range<u32>>,
}

/// Full-file split view: each pane holds a derived buffer where every
/// conflict region is replaced by just that side's content, so the panes read
/// as complete files and can be scrolled in lockstep.
struct SplitView {
    ours: SplitPane,
    theirs: SplitPane,
    base: Option<SplitPane>,
    _subscriptions: Vec<Subscription>,
}

impl SplitView {
    fn panes(&self) -> impl Iterator<Item = &SplitPane> {
        [&self.ours, &self.theirs]
            .into_iter()
            .chain(self.base.as_ref())
    }
}

fn selected_conflict_row_background(cx: &App) -> Hsla {
    cx.theme().colors().editor_highlighted_line_background
}

fn append_range(snapshot: &BufferSnapshot, range: Range<Anchor>, text: &mut String, row: &mut u32) {
    for chunk in snapshot.text_for_range(range) {
        *row += chunk.matches('\n').count() as u32;
        text.push_str(chunk);
    }
}

/// Splices each conflict's kept side into the surrounding non-conflict
/// content, producing one pane's full-file text plus the row span each
/// conflict's content occupies in it.
fn derive_pane_content(
    snapshot: &BufferSnapshot,
    conflicts: &[ConflictRegion],
    kept_range: impl Fn(&ConflictRegion) -> Option<Range<Anchor>>,
) -> (String, Vec<Range<u32>>) {
    let mut text = String::new();
    let mut row: u32 = 0;
    let mut conflict_rows = Vec::with_capacity(conflicts.len());
    let mut cursor = snapshot.anchor_before(0);
    for conflict in conflicts {
        append_range(snapshot, cursor..conflict.range.start, &mut text, &mut row);
        let content_start = row;
        if let Some(kept) = kept_range(conflict) {
            append_range(snapshot, kept, &mut text, &mut row);
        }
        conflict_rows.push(content_start..row);
        cursor = conflict.range.end;
    }
    let end = snapshot.anchor_after(snapshot.len());
    append_range(snapshot, cursor..end, &mut text, &mut row);
    (text, conflict_rows)
}

/// Maps a scroll row in one pane to the equivalent row in another. Outside
/// conflict regions the panes' contents are identical except for the
/// cumulative size difference of the conflict content above, so the last
/// passed conflict's end-row delta translates exactly. Inside a conflict the
/// row is aligned to the conflict's start and clamped to the target side's
/// extent.
fn translate_row(from: &[Range<u32>], to: &[Range<u32>], row: f64) -> f64 {
    let mut delta = 0.0;
    for (from_rows, to_rows) in from.iter().zip(to) {
        if row < f64::from(from_rows.start) {
            break;
        }
        if row < f64::from(from_rows.end) {
            let offset_in_conflict = row - f64::from(from_rows.start);
            let to_len = f64::from(to_rows.end - to_rows.start);
            return f64::from(to_rows.start) + offset_in_conflict.min(to_len);
        }
        delta = f64::from(to_rows.end) - f64::from(from_rows.end);
    }
    (row + delta).max(0.0)
}

fn build_split_pane(
    (text, conflict_rows): (String, Vec<Range<u32>>),
    language: Option<Arc<Language>>,
    window: &mut Window,
    cx: &mut Context<MergeEditorView>,
) -> SplitPane {
    let buffer = cx.new(|cx| {
        let mut buffer = Buffer::local(text, cx);
        buffer.set_language(language, cx);
        buffer
    });
    let editor = cx.new(|cx| {
        let mut editor = Editor::for_buffer(buffer.clone(), None, window, cx);
        editor.set_read_only(true);
        editor
    });
    SplitPane {
        editor,
        buffer,
        conflict_rows,
    }
}

fn update_split_pane(
    pane: &mut SplitPane,
    (text, conflict_rows): (String, Vec<Range<u32>>),
    cx: &mut App,
) {
    pane.conflict_rows = conflict_rows;
    pane.buffer.update(cx, |buffer, cx| {
        if buffer.text() != text {
            buffer.set_text(text, cx);
        }
    });
}

fn render_split_pane(
    editor: Entity<Editor>,
    label: SharedString,
    cx: &Context<MergeEditorView>,
) -> impl IntoElement {
    v_flex()
        .flex_1()
        .min_w_0()
        .child(
            div()
                .px_2()
                .py_1()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
        )
        .child(div().flex_1().min_h_0().child(editor))
}

impl MergeEditorView {
    pub fn new(
        buffer: Entity<Buffer>,
        conflict_set: Entity<ConflictSet>,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let file_label: SharedString = buffer
            .read(cx)
            .file()
            .map(|f| f.path().as_std_path().to_string_lossy().to_string().into())
            .unwrap_or_else(|| "untitled".into());

        let subscriptions = vec![
            cx.subscribe(&conflict_set, Self::on_conflict_set_update),
            cx.observe(&buffer, |this, _, cx| {
                this.refresh_split_content(cx);
                this.refresh_split_highlights(cx);
                cx.notify();
            }),
        ];

        Self {
            focus_handle,
            buffer,
            conflict_set,
            project,
            workspace,
            file_label,
            selected_index: 0,
            view_mode: MergeViewMode::Cards,
            split: None,
            syncing_split_scroll: false,
            _subscriptions: subscriptions,
        }
    }

    /// Opens the underlying buffer in a regular editor so the user can edit
    /// the conflict markers manually. The merge editor view stays open in the
    /// background; whatever the user types updates the same buffer so the
    /// merge editor's conflict cards update live.
    fn open_in_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project_path) = self.buffer.read(cx).project_path(cx) else {
            return;
        };
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_, cx| {
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace
                        .open_path(project_path, None, true, window, cx)
                        .detach();
                })
                .log_err();
        })
        .detach();
    }

    fn select_prev_conflict(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let total = self.conflict_set.read(cx).snapshot.conflicts.len();
        if total == 0 {
            return;
        }
        self.selected_index = self.selected_index.saturating_sub(1);
        self.on_selected_conflict_changed(window, cx);
    }

    fn select_next_conflict(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let total = self.conflict_set.read(cx).snapshot.conflicts.len();
        if total == 0 {
            return;
        }
        self.selected_index = (self.selected_index + 1).min(total - 1);
        self.on_selected_conflict_changed(window, cx);
    }

    fn on_selected_conflict_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.view_mode == MergeViewMode::Split {
            self.refresh_split_highlights(cx);
            self.scroll_split_to_selected(window, cx);
        }
        cx.notify();
    }

    fn on_conflict_set_update(
        &mut self,
        _: Entity<ConflictSet>,
        _: &ConflictSetUpdate,
        cx: &mut Context<Self>,
    ) {
        let total = self.conflict_set.read(cx).snapshot.conflicts.len();
        if total == 0 {
            self.selected_index = 0;
        } else if self.selected_index >= total {
            self.selected_index = total - 1;
        }
        self.refresh_split_content(cx);
        self.refresh_split_highlights(cx);
        cx.notify();
    }

    fn resolve_with(
        &self,
        conflict: &ConflictRegion,
        ranges: Vec<std::ops::Range<Anchor>>,
        cx: &mut Context<Self>,
    ) {
        conflict.resolve(self.buffer.clone(), &ranges, cx);
        cx.notify();
    }

    fn save_buffer(&self, cx: &mut Context<Self>) {
        let buffer = self.buffer.clone();
        let project = self.project.clone();
        project
            .update(cx, |project, cx| project.save_buffer(buffer, cx))
            .detach_and_log_err(cx);
    }

    fn toggle_view_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.view_mode {
            MergeViewMode::Cards => {
                if self.split.is_none() {
                    self.build_split_view(window, cx);
                } else {
                    self.refresh_split_content(cx);
                }
                self.view_mode = MergeViewMode::Split;
                self.refresh_split_highlights(cx);
                self.scroll_split_to_selected(window, cx);
            }
            MergeViewMode::Split => {
                self.view_mode = MergeViewMode::Cards;
            }
        }
        cx.notify();
    }

    fn build_split_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = self.buffer.read(cx).snapshot();
        let conflicts = self.conflict_set.read(cx).snapshot.conflicts.clone();
        let language = self.buffer.read(cx).language().cloned();

        let ours = build_split_pane(
            derive_pane_content(&snapshot, &conflicts, |conflict| {
                Some(conflict.ours.clone())
            }),
            language.clone(),
            window,
            cx,
        );
        let theirs = build_split_pane(
            derive_pane_content(&snapshot, &conflicts, |conflict| {
                Some(conflict.theirs.clone())
            }),
            language.clone(),
            window,
            cx,
        );
        let base = if conflicts.iter().any(|conflict| conflict.base.is_some()) {
            Some(build_split_pane(
                derive_pane_content(&snapshot, &conflicts, |conflict| conflict.base.clone()),
                language,
                window,
                cx,
            ))
        } else {
            None
        };

        let mut subscriptions = Vec::new();
        for editor in [&ours.editor, &theirs.editor]
            .into_iter()
            .chain(base.as_ref().map(|pane| &pane.editor))
        {
            subscriptions.push(cx.subscribe_in(
                editor,
                window,
                |this, editor, event, window, cx| {
                    if matches!(event, EditorEvent::ScrollPositionChanged { .. }) {
                        this.sync_split_scroll(editor.entity_id(), window, cx);
                    }
                },
            ));
        }

        self.split = Some(SplitView {
            ours,
            theirs,
            base,
            _subscriptions: subscriptions,
        });
    }

    fn refresh_split_content(&mut self, cx: &mut Context<Self>) {
        if self.split.is_none() {
            return;
        }
        let snapshot = self.buffer.read(cx).snapshot();
        let conflicts = self.conflict_set.read(cx).snapshot.conflicts.clone();
        let Some(split) = &mut self.split else {
            return;
        };
        update_split_pane(
            &mut split.ours,
            derive_pane_content(&snapshot, &conflicts, |conflict| {
                Some(conflict.ours.clone())
            }),
            cx,
        );
        update_split_pane(
            &mut split.theirs,
            derive_pane_content(&snapshot, &conflicts, |conflict| {
                Some(conflict.theirs.clone())
            }),
            cx,
        );
        if let Some(base) = &mut split.base {
            update_split_pane(
                base,
                derive_pane_content(&snapshot, &conflicts, |conflict| conflict.base.clone()),
                cx,
            );
        }
    }

    fn refresh_split_highlights(&self, cx: &mut Context<Self>) {
        let Some(split) = &self.split else {
            return;
        };
        for pane in split.panes() {
            let rows = pane.conflict_rows.get(self.selected_index).cloned();
            pane.editor.update(cx, |editor, cx| {
                editor.clear_row_highlights::<SelectedConflictRows>();
                let Some(rows) = rows else {
                    return;
                };
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                let start = snapshot.clip_point(Point::new(rows.start, 0), Bias::Left);
                // A side that contributed no content spans zero rows; extend
                // to one row so the conflict's position stays visible.
                let end =
                    snapshot.clip_point(Point::new(rows.end.max(rows.start + 1), 0), Bias::Left);
                editor.highlight_rows::<SelectedConflictRows>(
                    snapshot.anchor_before(start)..snapshot.anchor_before(end),
                    selected_conflict_row_background,
                    RowHighlightOptions {
                        autoscroll: false,
                        include_gutter: true,
                    },
                    cx,
                );
            });
        }
    }

    fn scroll_split_to_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(split) = &self.split else {
            return;
        };
        self.syncing_split_scroll = true;
        for pane in split.panes() {
            let Some(rows) = pane.conflict_rows.get(self.selected_index) else {
                continue;
            };
            // Leave a couple of context lines visible above the conflict.
            let target_y = f64::from(rows.start).max(2.0) - 2.0;
            pane.editor.update(cx, |editor, cx| {
                let current = editor.scroll_position(cx);
                editor.set_scroll_position(point(current.x, target_y), window, cx);
            });
        }
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            this.update(cx, |this, _| this.syncing_split_scroll = false)
                .log_err();
        });
    }

    fn sync_split_scroll(&mut self, source: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        if self.syncing_split_scroll {
            return;
        }
        let Some(split) = &self.split else {
            return;
        };
        let Some(source_pane) = split.panes().find(|pane| pane.editor.entity_id() == source) else {
            return;
        };
        let source_position = source_pane
            .editor
            .update(cx, |editor, cx| editor.scroll_position(cx));
        self.syncing_split_scroll = true;
        for pane in split.panes() {
            if pane.editor.entity_id() == source {
                continue;
            }
            let target_y = translate_row(
                &source_pane.conflict_rows,
                &pane.conflict_rows,
                source_position.y,
            );
            pane.editor.update(cx, |editor, cx| {
                let current = editor.scroll_position(cx);
                // Vertical-only sync: pane contents differ in line length, so
                // each pane keeps its own horizontal scroll.
                if (current.y - target_y).abs() > 0.01 {
                    editor.set_scroll_position(point(current.x, target_y), window, cx);
                }
            });
        }
        // The set_scroll_position calls above echo ScrollPositionChanged
        // events after this handler returns; keep the guard up until those
        // have been delivered.
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            this.update(cx, |this, _| this.syncing_split_scroll = false)
                .log_err();
        });
    }

    fn render_split_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(split) = &self.split else {
            return div().into_any_element();
        };
        let conflicts = self.conflict_set.read(cx).snapshot.conflicts.clone();
        let (ours_label, theirs_label): (SharedString, SharedString) = match conflicts.first() {
            Some(conflict) => (
                format!("Ours ({})", conflict.ours_branch_name).into(),
                format!("Theirs ({})", conflict.theirs_branch_name).into(),
            ),
            None => ("Ours".into(), "Theirs".into()),
        };

        let mut panes = div().flex().flex_1().min_h_0();
        panes = panes.child(render_split_pane(split.ours.editor.clone(), ours_label, cx));
        if let Some(base) = &split.base {
            panes = panes.child(Divider::vertical()).child(render_split_pane(
                base.editor.clone(),
                "Base".into(),
                cx,
            ));
        }
        panes = panes.child(Divider::vertical()).child(render_split_pane(
            split.theirs.editor.clone(),
            theirs_label,
            cx,
        ));
        panes.into_any_element()
    }

    fn render_conflict_pane(
        &self,
        label: SharedString,
        text: SharedString,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .min_w_0()
            .flex_1()
            .gap_1()
            .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(cx.theme().colors().editor_background)
                    .child(
                        // Render as preformatted text; these are typically
                        // small snippets, not full files; the conflict bodies
                        // come in with whitespace intact and we want the user
                        // to see them exactly.
                        Label::new(text).size(LabelSize::Small).buffer_font(cx),
                    ),
            )
    }

    fn render_conflict(
        &self,
        index: usize,
        total: usize,
        is_selected: bool,
        conflict: &ConflictRegion,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let snapshot = self.buffer.read(cx).snapshot();
        let ours_text: SharedString = snapshot
            .text_for_range(conflict.ours.clone())
            .collect::<String>()
            .into();
        let theirs_text: SharedString = snapshot
            .text_for_range(conflict.theirs.clone())
            .collect::<String>()
            .into();
        let base_text: Option<SharedString> = conflict.base.as_ref().map(|range| {
            snapshot
                .text_for_range(range.clone())
                .collect::<String>()
                .into()
        });

        let ours_branch = conflict.ours_branch_name.clone();
        let theirs_branch = conflict.theirs_branch_name.clone();

        let ours_range = conflict.ours.clone();
        let theirs_range = conflict.theirs.clone();
        let conflict_for_ours = conflict.clone();
        let conflict_for_theirs = conflict.clone();
        let conflict_for_both = conflict.clone();

        let border_color = if is_selected {
            cx.theme().colors().border_selected
        } else {
            cx.theme().colors().border_variant
        };

        v_flex()
            .gap_2()
            .px_3()
            .py_2()
            .border_1()
            .border_color(border_color)
            .rounded_md()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(format!("Conflict {} of {}", index + 1, total))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(format!("{ours_branch} ⇄ {theirs_branch}"))
                            .size(LabelSize::Small),
                    ),
            )
            .child({
                let mut row = h_flex().gap_2().items_start();
                if let Some(base) = base_text {
                    row = row.child(self.render_conflict_pane("Base".into(), base, cx));
                }
                row.child(self.render_conflict_pane(
                    format!("Ours ({ours_branch})").into(),
                    ours_text,
                    cx,
                ))
                .child(self.render_conflict_pane(
                    format!("Theirs ({theirs_branch})").into(),
                    theirs_text,
                    cx,
                ))
            })
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new(("take-ours", index), "Take ours")
                            .style(ButtonStyle::Filled)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.resolve_with(&conflict_for_ours, vec![ours_range.clone()], cx);
                            })),
                    )
                    .child(
                        Button::new(("take-theirs", index), "Take theirs")
                            .style(ButtonStyle::Filled)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.resolve_with(
                                    &conflict_for_theirs,
                                    vec![theirs_range.clone()],
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Button::new(("take-both", index), "Take both")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let ranges = vec![
                                    conflict_for_both.ours.clone(),
                                    conflict_for_both.theirs.clone(),
                                ];
                                this.resolve_with(&conflict_for_both, ranges, cx);
                            })),
                    ),
            )
            .into_any_element()
    }
}

impl Focusable for MergeEditorView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

pub enum MergeEditorViewEvent {}

impl EventEmitter<MergeEditorViewEvent> for MergeEditorView {}

impl Render for MergeEditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.conflict_set.read(cx).snapshot();
        let conflicts: Arc<[ConflictRegion]> = snapshot.conflicts;
        let total = conflicts.len();
        // Clamp selection so a resolved conflict doesn't leave the index past
        // the end (the conflict list shrinks).
        if total == 0 {
            self.selected_index = 0;
        } else if self.selected_index >= total {
            self.selected_index = total - 1;
        }
        let selected_index = self.selected_index;
        let file_label = self.file_label.clone();
        let is_dirty = self.buffer.read(cx).is_dirty();
        let view_mode = self.view_mode;

        let nav_enabled = total > 1;
        // In split mode the per-conflict cards (and their buttons) aren't
        // visible, so the header carries resolve buttons for the selected
        // conflict instead.
        let selected_conflict = (view_mode == MergeViewMode::Split)
            .then(|| conflicts.get(selected_index).cloned())
            .flatten();

        let header = h_flex()
            .px_4()
            .py_2()
            .gap_3()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new("Merge editor").size(LabelSize::Large))
            .child(
                Label::new(file_label)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Label::new(if total == 0 {
                    "0 conflicts".to_string()
                } else {
                    format!("{} of {total}", selected_index + 1)
                })
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(
                IconButton::new("merge-prev-conflict", IconName::ChevronLeft)
                    .icon_size(IconSize::Small)
                    .disabled(!nav_enabled)
                    .tooltip(Tooltip::text("Previous conflict"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.select_prev_conflict(window, cx);
                    })),
            )
            .child(
                IconButton::new("merge-next-conflict", IconName::ChevronRight)
                    .icon_size(IconSize::Small)
                    .disabled(!nav_enabled)
                    .tooltip(Tooltip::text("Next conflict"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.select_next_conflict(window, cx);
                    })),
            )
            .when_some(selected_conflict, |header, conflict| {
                let ours_range = conflict.ours.clone();
                let theirs_range = conflict.theirs.clone();
                let conflict_for_ours = conflict.clone();
                let conflict_for_theirs = conflict.clone();
                let conflict_for_both = conflict;
                header
                    .child(
                        Button::new("split-take-ours", "Take ours")
                            .style(ButtonStyle::Filled)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.resolve_with(&conflict_for_ours, vec![ours_range.clone()], cx);
                            })),
                    )
                    .child(
                        Button::new("split-take-theirs", "Take theirs")
                            .style(ButtonStyle::Filled)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.resolve_with(
                                    &conflict_for_theirs,
                                    vec![theirs_range.clone()],
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Button::new("split-take-both", "Take both")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let ranges = vec![
                                    conflict_for_both.ours.clone(),
                                    conflict_for_both.theirs.clone(),
                                ];
                                this.resolve_with(&conflict_for_both, ranges, cx);
                            })),
                    )
            })
            .child(
                Button::new(
                    "toggle-view-mode",
                    match view_mode {
                        MergeViewMode::Cards => "Split view",
                        MergeViewMode::Split => "Card view",
                    },
                )
                .style(ButtonStyle::Subtle)
                .label_size(LabelSize::Small)
                .tooltip(Tooltip::text(
                    "Switch between per-conflict cards and full-file panes with synced scrolling",
                ))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.toggle_view_mode(window, cx);
                })),
            )
            .child(
                Button::new("edit-manually", "Edit manually")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::text(
                        "Open the buffer with conflict markers in a regular editor",
                    ))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_in_editor(window, cx);
                    })),
            )
            .child(
                Button::new("save-buffer", if is_dirty { "Save" } else { "Saved" })
                    .style(ButtonStyle::Filled)
                    .label_size(LabelSize::Small)
                    .disabled(!is_dirty)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.save_buffer(cx);
                    })),
            );

        let body = if view_mode == MergeViewMode::Split && self.split.is_some() {
            self.render_split_body(cx)
        } else if total == 0 {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(
                    Label::new("No conflicts remaining")
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .into_any_element()
        } else {
            let rows = (0..total).map(|ix| {
                let conflict = conflicts[ix].clone();
                self.render_conflict(ix, total, ix == selected_index, &conflict, cx)
            });
            v_flex().p_3().gap_3().children(rows).into_any_element()
        };

        v_flex()
            .key_context("MergeEditorView")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(header)
            .child(Divider::horizontal())
            .child(body)
    }
}

impl Item for MergeEditorView {
    type Event = MergeEditorViewEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitBranch).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("Merge: {}", self.file_label).into()
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Merge Editor Opened")
    }

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(ItemEvent)) {}

    fn show_toolbar(&self) -> bool {
        false
    }
}

/// Open the merge editor as a workspace item for the given buffer. The buffer
/// must already have its conflicts populated (which happens automatically the
/// first time an `Editor` displays it).
pub fn open_merge_editor(
    workspace: WeakEntity<Workspace>,
    buffer: Entity<Buffer>,
    conflict_set: Entity<ConflictSet>,
    window: &mut Window,
    cx: &mut App,
) {
    workspace
        .update(cx, |workspace, cx| {
            let project = workspace.project().clone();
            let workspace_weak = workspace.weak_handle();
            let view = cx
                .new(|cx| MergeEditorView::new(buffer, conflict_set, project, workspace_weak, cx));
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        })
        .ok();
}
