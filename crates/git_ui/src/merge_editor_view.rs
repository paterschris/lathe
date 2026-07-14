use gpui::{
    AppContext as _, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Subscription,
    WeakEntity,
};
use language::{Anchor, Buffer};
use project::{ConflictRegion, ConflictSet, ConflictSetUpdate, Project, ProjectItem as _};
use std::sync::Arc;
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
    _subscriptions: Vec<Subscription>,
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
            cx.observe(&buffer, |_, _, cx| cx.notify()),
        ];

        Self {
            focus_handle,
            buffer,
            conflict_set,
            project,
            workspace,
            file_label,
            selected_index: 0,
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

    fn select_prev_conflict(&mut self, cx: &mut Context<Self>) {
        let total = self.conflict_set.read(cx).snapshot.conflicts.len();
        if total == 0 {
            return;
        }
        self.selected_index = self.selected_index.saturating_sub(1);
        cx.notify();
    }

    fn select_next_conflict(&mut self, cx: &mut Context<Self>) {
        let total = self.conflict_set.read(cx).snapshot.conflicts.len();
        if total == 0 {
            return;
        }
        self.selected_index = (self.selected_index + 1).min(total - 1);
        cx.notify();
    }

    fn on_conflict_set_update(
        &mut self,
        _: Entity<ConflictSet>,
        _: &ConflictSetUpdate,
        cx: &mut Context<Self>,
    ) {
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
                        // Render as preformatted text — these are typically
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

        let nav_enabled = total > 1;

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
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.select_prev_conflict(cx);
                    })),
            )
            .child(
                IconButton::new("merge-next-conflict", IconName::ChevronRight)
                    .icon_size(IconSize::Small)
                    .disabled(!nav_enabled)
                    .tooltip(Tooltip::text("Next conflict"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.select_next_conflict(cx);
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

        let body = if total == 0 {
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
