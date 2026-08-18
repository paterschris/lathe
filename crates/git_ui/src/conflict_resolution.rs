//! Reporting and resolution for operations that stop on merge conflicts.
//!
//! Git operations that can't complete because of conflicts fail with a
//! [`ConflictingOperationError`] carrying the unmerged paths. This module turns
//! that into a toast naming those files, and hosts the surface the toast links
//! to: the conflicted files listed beside the merge editor for the selected
//! one, with continue/abort controls for the operation as a whole.

use anyhow::Result;
use git::repository::{
    ConflictResolutionAction, ConflictingOperation, ConflictingOperationError, RepoPath,
};
use gpui::{
    App, AppContext as _, AsyncWindowContext, Entity, EventEmitter, FocusHandle, Focusable,
    SharedString, Subscription, Task, WeakEntity,
};
use notifications::status_toast::StatusToast;
use project::{
    Project, ProjectPath,
    git_store::{GitStoreEvent, Repository, RepositoryEvent},
};
use ui::{Divider, ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

use crate::merge_editor_view::{ConflictSide, MergeEditorView};

/// Reports a failed git operation, preferring the conflict surface when the
/// failure left unmerged paths behind and falling back to the generic error
/// toast otherwise.
pub fn report_operation_error(
    workspace: &Entity<Workspace>,
    repository: Option<Entity<Repository>>,
    action: impl Into<SharedString>,
    error: anyhow::Error,
    cx: &mut App,
) {
    if let Some(repository) = repository
        && notify_conflicting_operation(workspace, repository, &error, cx)
    {
        return;
    }
    crate::git_panel::show_error_toast(workspace.clone(), action, error, cx);
}

/// Shows a toast naming the files an operation stopped on. Returns false when
/// `error` is any other failure, so callers can fall back to their own error
/// reporting.
pub fn notify_conflicting_operation(
    workspace: &Entity<Workspace>,
    repository: Entity<Repository>,
    error: &anyhow::Error,
    cx: &mut App,
) -> bool {
    let Some(conflict) = error.downcast_ref::<ConflictingOperationError>() else {
        return false;
    };

    // Name the file outright when there's only one; otherwise the count, with
    // the full list a click away in the resolution view.
    let message = match conflict.conflicted_paths.as_slice() {
        [path] => format!(
            "{} stopped: conflict in {}",
            conflict.operation,
            path.as_unix_str()
        ),
        paths => format!(
            "{} stopped: {} conflicted files",
            conflict.operation,
            paths.len()
        ),
    };

    workspace.update(cx, |workspace, cx| {
        let workspace_handle = cx.weak_entity();
        let toast = StatusToast::new(message, cx, move |this, _cx| {
            let repository = repository.clone();
            this.icon(
                Icon::new(IconName::GitMergeConflict)
                    .size(IconSize::Small)
                    .color(Color::Warning),
            )
            .auto_dismiss(false)
            .dismiss_button(true)
            .action("Resolve", move |window, cx| {
                let Some(workspace) = workspace_handle.upgrade() else {
                    return;
                };
                open_conflict_resolution(workspace, repository.clone(), window, cx);
            })
        });
        workspace.toggle_status_toast(toast, cx);
    });
    true
}

/// Opens (or re-focuses) the conflict resolution tab for `repository`.
pub fn open_conflict_resolution(
    workspace: Entity<Workspace>,
    repository: Entity<Repository>,
    window: &mut Window,
    cx: &mut App,
) {
    workspace.update(cx, |workspace, cx| {
        let repository_id = repository.read(cx).id;
        let existing = workspace
            .items_of_type::<ConflictResolutionView>(cx)
            .find(|view| view.read(cx).repository.read(cx).id == repository_id);
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            existing.update(cx, |view, cx| view.refresh(cx));
            return;
        }

        let project = workspace.project().clone();
        let workspace_handle = cx.weak_entity();
        let view = cx.new(|cx| {
            ConflictResolutionView::new(project, repository, workspace_handle, window, cx)
        });
        workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
    });
}

struct ConflictEntry {
    path: RepoPath,
    /// Still unmerged in the index. False once the file's markers are gone and
    /// it has been staged.
    unresolved: bool,
}

/// Workspace item listing every file the in-progress operation conflicted on,
/// with the merge editor for the selected file alongside.
pub struct ConflictResolutionView {
    focus_handle: FocusHandle,
    project: Entity<Project>,
    repository: Entity<Repository>,
    workspace: WeakEntity<Workspace>,
    entries: Vec<ConflictEntry>,
    selected_path: Option<RepoPath>,
    /// Merge editor for `selected_path`. Rebuilt whenever the selection moves
    /// to a different file.
    merge_editor: Option<(RepoPath, Entity<MergeEditorView>)>,
    _pending_open: Option<Task<()>>,
    operation: Option<ConflictingOperation>,
    _pending_operation_query: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ConflictResolutionView {
    fn new(
        project: Entity<Project>,
        repository: Entity<Repository>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let git_store = project.read(cx).git_store().clone();
        let subscriptions = vec![
            cx.subscribe(&git_store, |this, _, event, cx| {
                if matches!(
                    event,
                    GitStoreEvent::ConflictsUpdated
                        | GitStoreEvent::RepositoryUpdated(_, RepositoryEvent::StatusesChanged, _)
                ) {
                    this.refresh(cx);
                }
            }),
            cx.observe(&repository, |this, _, cx| this.refresh_entries(cx)),
        ];

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            project,
            repository,
            workspace,
            entries: Vec::new(),
            selected_path: None,
            merge_editor: None,
            _pending_open: None,
            operation: None,
            _pending_operation_query: None,
            _subscriptions: subscriptions,
        };
        this.refresh_entries(cx);
        this.refresh_operation(cx);
        if let Some(path) = this
            .entries
            .iter()
            .find(|entry| entry.unresolved)
            .or_else(|| this.entries.first())
            .map(|entry| entry.path.clone())
        {
            this.select_path(path, window, cx);
        }
        this
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_entries(cx);
        self.refresh_operation(cx);
    }

    fn refresh_entries(&mut self, cx: &mut Context<Self>) {
        let repository = self.repository.read(cx);
        let mut entries = Vec::new();
        for status_entry in repository.cached_status() {
            let is_conflict = repository
                .had_conflict_on_last_merge_head_change(&status_entry.repo_path)
                || status_entry.status.is_conflicted();
            if !is_conflict {
                continue;
            }
            entries.push(ConflictEntry {
                path: status_entry.repo_path.clone(),
                unresolved: status_entry.status.is_conflicted(),
            });
        }
        self.entries = entries;
        cx.notify();
    }

    fn refresh_operation(&mut self, cx: &mut Context<Self>) {
        let receiver = self
            .repository
            .update(cx, |repository, _| repository.operation_in_progress());
        self._pending_operation_query = Some(cx.spawn(async move |this, cx| {
            let operation = match receiver.await {
                Ok(Ok(operation)) => operation,
                Ok(Err(error)) => {
                    log::error!("failed to query in-progress git operation: {error:?}");
                    return;
                }
                Err(_) => return,
            };
            this.update(cx, |this, cx| {
                if this.operation != operation {
                    this.operation = operation;
                    cx.notify();
                }
            })
            .log_err();
        }));
    }

    fn unresolved_count(&self) -> usize {
        self.entries.iter().filter(|entry| entry.unresolved).count()
    }

    fn select_path(&mut self, path: RepoPath, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_path.as_ref() == Some(&path) {
            return;
        }
        self.selected_path = Some(path.clone());
        self.merge_editor = None;
        cx.notify();

        let Some(project_path) = self
            .repository
            .read(cx)
            .repo_path_to_project_path(&path, cx)
        else {
            return;
        };
        self._pending_open = Some(cx.spawn_in(window, async move |this, cx| {
            if let Err(error) = Self::load_merge_editor(&this, path, project_path, cx).await {
                log::error!("failed to open the merge editor: {error:?}");
            }
        }));
    }

    async fn load_merge_editor(
        this: &WeakEntity<Self>,
        path: RepoPath,
        project_path: ProjectPath,
        cx: &mut AsyncWindowContext,
    ) -> Result<()> {
        let project = this.read_with(cx, |this, _| this.project.clone())?;
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(project_path, cx))
            .await?;
        let git_store = project.read_with(cx, |project, _| project.git_store().clone());
        let conflict_set = git_store
            .update(cx, |git_store, cx| {
                git_store.open_conflict_set(buffer.clone(), cx)
            })
            .await;

        this.update(cx, |this, cx| {
            // A later selection may have superseded this load.
            if this.selected_path.as_ref() != Some(&path) {
                return;
            }
            let workspace = this.workspace.clone();
            let project = this.project.clone();
            let merge_editor =
                cx.new(|cx| MergeEditorView::new(buffer, conflict_set, project, workspace, cx));
            this.merge_editor = Some((path, merge_editor));
            cx.notify();
        })?;
        Ok(())
    }

    /// Saves and stages the file, which is what marks a conflict resolved as
    /// far as git is concerned.
    fn mark_resolved(&mut self, path: RepoPath, cx: &mut Context<Self>) {
        self.repository
            .update(cx, |repository, cx| {
                repository.stage_entries(vec![path], cx)
            })
            .detach_and_log_err(cx);
    }

    fn resolve_selected_file_with(&mut self, side: ConflictSide, cx: &mut Context<Self>) {
        let Some((_, merge_editor)) = self.merge_editor.clone() else {
            return;
        };
        merge_editor.update(cx, |merge_editor, cx| merge_editor.resolve_all(side, cx));
    }

    /// Runs `--continue` or `--abort` for the in-progress operation. Continuing
    /// stages the resolved files first, since git refuses to continue while
    /// they're still unmerged in the index.
    fn run_operation_action(&mut self, action: ConflictResolutionAction, cx: &mut Context<Self>) {
        let Some(operation) = self.operation else {
            return;
        };
        let repository = self.repository.clone();
        let workspace = self.workspace.clone();
        let paths: Vec<RepoPath> = self
            .entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect();

        cx.spawn(async move |this, cx| {
            if action == ConflictResolutionAction::Continue && !paths.is_empty() {
                repository
                    .update(cx, |repository, cx| repository.stage_entries(paths, cx))
                    .await?;
            }

            let receiver = repository.update(cx, |repository, _| {
                repository.resolve_operation(operation, action)
            });
            match receiver.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    cx.update(|cx| {
                        if let Some(workspace) = workspace.upgrade() {
                            report_operation_error(
                                &workspace,
                                Some(repository.clone()),
                                format!("{operation}"),
                                error,
                                cx,
                            );
                        }
                    });
                }
                Err(_) => return Ok(()),
            }
            this.update(cx, |this, cx| this.refresh(cx))?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let total = self.entries.len();
        let unresolved = self.unresolved_count();
        let title: SharedString = match self.operation {
            Some(operation) => format!("Resolving {operation}").into(),
            None => "Conflicts".into(),
        };
        let can_continue = self.operation.is_some() && unresolved == 0 && total > 0;
        let has_selection = self.merge_editor.is_some();

        h_flex()
            .px_4()
            .py_2()
            .gap_3()
            .flex_none()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new(title).size(LabelSize::Large))
            .child(
                Label::new(format!("{} of {total} resolved", total - unresolved))
                    .size(LabelSize::Small)
                    .color(if unresolved == 0 {
                        Color::Success
                    } else {
                        Color::Muted
                    }),
            )
            .child(div().flex_1())
            .child(
                Button::new("resolve-file-ours", "File: all ours")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .disabled(!has_selection)
                    .tooltip(Tooltip::text(
                        "Resolve every conflict in the selected file as ours",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.resolve_selected_file_with(ConflictSide::Ours, cx);
                    })),
            )
            .child(
                Button::new("resolve-file-theirs", "File: all theirs")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .disabled(!has_selection)
                    .tooltip(Tooltip::text(
                        "Resolve every conflict in the selected file as theirs",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.resolve_selected_file_with(ConflictSide::Theirs, cx);
                    })),
            )
            .when_some(self.operation, |header, operation| {
                header
                    .child(
                        Button::new("abort-operation", format!("Abort {operation}"))
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .color(Color::Error)
                            .tooltip(Tooltip::text(
                                "Undo the operation and restore the previous state",
                            ))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.run_operation_action(ConflictResolutionAction::Abort, cx);
                            })),
                    )
                    .child(
                        Button::new("continue-operation", format!("Continue {operation}"))
                            .style(ButtonStyle::Filled)
                            .label_size(LabelSize::Small)
                            .disabled(!can_continue)
                            .tooltip(Tooltip::text(
                                "Stage the resolved files and carry on with the operation",
                            ))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.run_operation_action(ConflictResolutionAction::Continue, cx);
                            })),
                    )
            })
    }

    fn render_file_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected_path = self.selected_path.clone();
        let rows = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let path = entry.path.clone();
                let label: SharedString = path.as_unix_str().to_string().into();
                let is_selected = selected_path.as_ref() == Some(&path);
                let unresolved = entry.unresolved;
                let path_for_select = path.clone();
                let path_for_resolve = path;

                ListItem::new(("conflicted-file", index))
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .toggle_state(is_selected)
                    .start_slot(
                        Icon::new(if unresolved {
                            IconName::GitMergeConflict
                        } else {
                            IconName::Check
                        })
                        .size(IconSize::Small)
                        .color(if unresolved {
                            Color::Warning
                        } else {
                            Color::Success
                        }),
                    )
                    .child(
                        Label::new(label.clone())
                            .size(LabelSize::Small)
                            .truncate()
                            .color(if unresolved {
                                Color::Default
                            } else {
                                Color::Muted
                            }),
                    )
                    .tooltip(Tooltip::text(label))
                    .end_slot(
                        IconButton::new(("mark-resolved", index), IconName::Check)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Stage this file as resolved"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.mark_resolved(path_for_resolve.clone(), cx);
                            })),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_path(path_for_select.clone(), window, cx);
                    }))
            })
            .collect::<Vec<_>>();

        v_flex()
            .id("conflicted-files")
            .w(rems(18.))
            .flex_none()
            .h_full()
            .border_r_1()
            .border_color(cx.theme().colors().border)
            .overflow_y_scroll()
            .child(
                div().px_3().py_2().child(
                    Label::new("Conflicted files")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
            .children(rows)
    }

    fn render_body(&self, _cx: &Context<Self>) -> AnyElement {
        if self.entries.is_empty() {
            return v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(
                    Label::new("No conflicted files")
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .into_any_element();
        }

        match &self.merge_editor {
            Some((_, merge_editor)) => div()
                .flex_1()
                .min_w_0()
                .h_full()
                .child(merge_editor.clone())
                .into_any_element(),
            None => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(
                    Label::new(if self.selected_path.is_some() {
                        "Loading conflicts…"
                    } else {
                        "Select a file to resolve"
                    })
                    .color(Color::Muted)
                    .size(LabelSize::Small),
                )
                .into_any_element(),
        }
    }
}

impl Focusable for ConflictResolutionView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

pub enum ConflictResolutionViewEvent {}

impl EventEmitter<ConflictResolutionViewEvent> for ConflictResolutionView {}

impl Render for ConflictResolutionView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("ConflictResolutionView")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_header(cx))
            .child(Divider::horizontal())
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_file_list(cx))
                    .child(self.render_body(cx)),
            )
    }
}

impl Item for ConflictResolutionView {
    type Event = ConflictResolutionViewEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitMergeConflict).color(Color::Warning))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        let unresolved = self.unresolved_count();
        if unresolved == 0 {
            "Conflicts".into()
        } else {
            format!("Conflicts ({unresolved})").into()
        }
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Conflict Resolution Opened")
    }

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(ItemEvent)) {}

    fn show_toolbar(&self) -> bool {
        false
    }
}
