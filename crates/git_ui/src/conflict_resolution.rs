//! Reporting and resolution for operations that stop on merge conflicts.
//!
//! Git operations that can't complete because of conflicts fail with a
//! [`ConflictingOperationError`] carrying the unmerged paths. This module turns
//! that into a toast naming those files, and hosts the surface the toast links
//! to: the conflicted files listed beside the merge editor for the selected
//! one, with continue/abort controls for the operation as a whole.

use anyhow::Result;
use git::repository::{
    ConflictResolutionAction, ConflictStages, ConflictingOperation, ConflictingOperationError,
    RepoPath, WholeFileResolution,
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
    item::{Item, ItemEvent, SaveOptions},
};

use crate::merge_editor_view::{ConflictSide, MergeEditorView, MergeEditorViewEvent, decode_stage};

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
) -> Entity<ConflictResolutionView> {
    workspace.update(cx, |workspace, cx| {
        let repository_id = repository.read(cx).id;
        let existing = workspace
            .items_of_type::<ConflictResolutionView>(cx)
            .find(|view| view.read(cx).repository.read(cx).id == repository_id);
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            existing.update(cx, |view, cx| view.refresh(cx));
            return existing;
        }

        let project = workspace.project().clone();
        let workspace_handle = cx.weak_entity();
        let view = cx.new(|cx| {
            ConflictResolutionView::new(project, repository, workspace_handle, window, cx)
        });
        workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
        view
    })
}

/// A selected file whose conflict is over the whole file (one side deleted
/// it, or it's binary). Git writes no markers for these, so instead of the
/// merge editor the user picks a version to keep.
struct WholeFileConflict {
    path: RepoPath,
    stages: ConflictStages,
    resolving: bool,
    resolved: Option<WholeFileResolution>,
}

impl WholeFileConflict {
    fn description(&self) -> &'static str {
        let stages = &self.stages;
        match (
            stages.base.is_some(),
            stages.ours.is_some(),
            stages.theirs.is_some(),
        ) {
            (_, true, true) => "Both sides changed this binary file. Keep one version whole.",
            (true, false, true) => "Ours deleted this file and theirs changed it.",
            (true, true, false) => "Ours changed this file and theirs deleted it.",
            // Without a base, the one side that has the file added it, usually
            // by renaming something the other side renamed differently.
            (false, true, false) => "Only ours has this file. It was added or renamed on our side.",
            (false, false, true) => {
                "Only theirs has this file. It was added or renamed on their side."
            }
            (_, false, false) => "Both sides deleted or renamed this file.",
        }
    }

    /// Taking a side whose version doesn't exist means accepting its deletion.
    fn resolution_for(&self, side: ConflictSide) -> WholeFileResolution {
        match side {
            ConflictSide::Ours if self.stages.ours.is_some() => WholeFileResolution::KeepOurs,
            ConflictSide::Theirs if self.stages.theirs.is_some() => WholeFileResolution::KeepTheirs,
            ConflictSide::Ours | ConflictSide::Theirs => WholeFileResolution::Delete,
        }
    }
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
    _merge_editor_subscription: Option<Subscription>,
    /// Set instead of `merge_editor` when the selected file's conflict is over
    /// the whole file.
    whole_file: Option<WholeFileConflict>,
    _pending_whole_file: Option<Task<()>>,
    /// Explains a merge Lathe did itself because git skipped it, shown
    /// above the merge editor.
    merge_note: Option<SharedString>,
    /// Shown in place of the editor when there's none to show: the selected
    /// file was resolved by deleting it, or couldn't be opened.
    body_message: Option<SharedString>,
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
            _merge_editor_subscription: None,
            whole_file: None,
            _pending_whole_file: None,
            body_message: None,
            merge_note: None,
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

    /// Whether the repository has no conflicted files left and no operation
    /// stopped waiting to be continued, which is when a caller waiting on this
    /// tab (`lathe --resolve --wait`) is done.
    pub fn is_settled(&self) -> bool {
        self.unresolved_count() == 0 && self.operation.is_none()
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
        self._merge_editor_subscription = None;
        self.whole_file = None;
        self.body_message = None;
        self.merge_note = None;
        cx.notify();

        let Some(project_path) = self
            .repository
            .read(cx)
            .repo_path_to_project_path(&path, cx)
        else {
            self.body_message =
                Some(format!("{} is outside the project", path.as_unix_str()).into());
            return;
        };
        self._pending_open = Some(cx.spawn_in(window, async move |this, cx| {
            if let Err(error) = Self::load_selection(&this, path.clone(), project_path, cx).await {
                log::error!("failed to open the merge editor: {error:?}");
                this.update(cx, |this, cx| {
                    if this.selected_path.as_ref() == Some(&path) {
                        this.body_message =
                            Some(format!("Couldn't open {}: {error:#}", path.as_unix_str()).into());
                        cx.notify();
                    }
                })
                .log_err();
            }
        }));
    }

    /// Loads the selected file's index versions, then shows either the
    /// whole-file choice or the merge editor, depending on the conflict.
    async fn load_selection(
        this: &WeakEntity<Self>,
        path: RepoPath,
        project_path: ProjectPath,
        cx: &mut AsyncWindowContext,
    ) -> Result<()> {
        let (project, repository) = this.read_with(cx, |this, _| {
            (this.project.clone(), this.repository.clone())
        })?;
        let receiver = repository.update(cx, |repository, _| {
            repository.load_conflict_stages(path.clone())
        });
        // Without the index versions the merge editor still works from the
        // markers, so a failure here isn't fatal. Pass along that the load was
        // tried so the merge editor doesn't repeat it.
        let stages = match receiver.await {
            Ok(Ok(stages)) => stages,
            Ok(Err(error)) => {
                log::warn!("failed to load conflict stages: {error:#}");
                ConflictStages::default()
            }
            Err(_) => ConflictStages::default(),
        };
        if stages.is_whole_file() {
            return this.update(cx, |this, cx| {
                if this.selected_path.as_ref() != Some(&path) {
                    return;
                }
                this.whole_file = Some(WholeFileConflict {
                    path,
                    stages,
                    resolving: false,
                    resolved: None,
                });
                cx.notify();
            });
        }

        // A resolved file that was deleted is gone from the worktree. Opening
        // it would create an empty buffer that saving would write back. Ask
        // the filesystem rather than the worktree, which doesn't track files
        // in unscanned (e.g. gitignored) directories.
        let local_file = project.read_with(cx, |project, cx| {
            project
                .is_local()
                .then(|| project.absolute_path(&project_path, cx))
                .flatten()
                .map(|abs_path| (project.fs().clone(), abs_path))
        });
        let exists = match local_file {
            Some((fs, abs_path)) => fs.is_file(&abs_path).await,
            None => true,
        };
        if !exists {
            return this.update(cx, |this, cx| {
                if this.selected_path.as_ref() == Some(&path) {
                    this.body_message =
                        Some(format!("{} has been deleted", path.as_unix_str()).into());
                    cx.notify();
                }
            });
        }

        let buffer = project
            .update(cx, |project, cx| project.open_buffer(project_path, cx))
            .await?;
        let git_store = project.read_with(cx, |project, _| project.git_store().clone());
        let conflict_set = git_store
            .update(cx, |git_store, cx| {
                git_store.open_conflict_set(buffer.clone(), cx)
            })
            .await;

        // Git can also skip the line merge for text, e.g. for a -merge or
        // merge=binary rule in .gitattributes. It then leaves ours in place
        // with no markers. Merge the versions line by line here instead, the
        // way git would have without the rule, so the file can be resolved
        // like any other. Only when it's still exactly ours: without markers
        // but with other content, it was resolved by hand already. The merge
        // only changes the buffer; the file on disk stays as git left it
        // until the user saves.
        let no_markers = conflict_set.read_with(cx, |conflict_set, _| {
            conflict_set.snapshot.conflicts.is_empty()
        });
        let merged = buffer.read_with(cx, |buffer, _| {
            let (Some(ours), Some(theirs)) = (&stages.ours, &stages.theirs) else {
                return None;
            };
            if !no_markers || stages.binary {
                return None;
            }
            let encoding = buffer.encoding();
            let ours = decode_stage(ours, encoding);
            if ours != buffer.text() {
                return None;
            }
            let theirs = decode_stage(theirs, encoding);
            let base = stages
                .base
                .as_ref()
                .map(|base| decode_stage(base, encoding))
                .unwrap_or_default();
            let merged = match diffy::MergeOptions::new()
                .set_conflict_style(diffy::ConflictStyle::Merge)
                .merge(&base, &ours, &theirs)
            {
                Ok(clean) => clean,
                Err(with_conflicts) => with_conflicts,
            };
            Some(merged)
        });
        let merged_here = merged.is_some();
        let conflict_set = match merged {
            Some(merged) => {
                buffer.update(cx, |buffer, cx| {
                    let len = buffer.len();
                    buffer.edit([(0..len, merged)], None, cx);
                });
                git_store
                    .update(cx, |git_store, cx| {
                        git_store.open_conflict_set(buffer.clone(), cx)
                    })
                    .await
            }
            None => conflict_set,
        };

        this.update_in(cx, |this, window, cx| {
            // A later selection may have superseded this load.
            if this.selected_path.as_ref() != Some(&path) {
                return;
            }
            let workspace = this.workspace.clone();
            let project = this.project.clone();
            let merge_editor = cx.new(|cx| {
                MergeEditorView::new(
                    buffer,
                    conflict_set,
                    project,
                    workspace,
                    Some(stages),
                    window,
                    cx,
                )
            });
            this._merge_editor_subscription =
                Some(cx.subscribe(&merge_editor, |_, _, event, cx| match event {
                    MergeEditorViewEvent::UpdateTab => {
                        cx.emit(ConflictResolutionViewEvent::UpdateTab);
                        cx.notify();
                    }
                    // Only merge-tool mode closes itself, and it is never
                    // embedded here.
                    MergeEditorViewEvent::Close => {}
                }));
            this.merge_editor = Some((path, merge_editor));
            this.merge_note = merged_here.then(|| {
                "Git didn't merge this file line by line, usually because of a -merge or \
                 merge=binary rule in .gitattributes. Lathe merged it here; the file on disk \
                 doesn't change until you save."
                    .into()
            });
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
        if let Some(whole_file) = &self.whole_file {
            let resolution = whole_file.resolution_for(side);
            self.resolve_whole_file(resolution, cx);
            return;
        }
        let Some((_, merge_editor)) = self.merge_editor.clone() else {
            return;
        };
        merge_editor.update(cx, |merge_editor, cx| merge_editor.resolve_all(side, cx));
    }

    fn resolve_whole_file(&mut self, resolution: WholeFileResolution, cx: &mut Context<Self>) {
        let Some(whole_file) = &mut self.whole_file else {
            return;
        };
        if whole_file.resolving || whole_file.resolved.is_some() {
            return;
        }
        whole_file.resolving = true;
        let path = whole_file.path.clone();
        cx.notify();

        let receiver = self.repository.update(cx, |repository, _| {
            repository.resolve_whole_file(path.clone(), resolution)
        });
        let workspace = self.workspace.clone();
        self._pending_whole_file = Some(cx.spawn(async move |this, cx| {
            let result = receiver.await;
            this.update(cx, |this, cx| {
                let whole_file = this
                    .whole_file
                    .as_mut()
                    .filter(|whole_file| whole_file.path == path);
                match result {
                    Ok(Ok(())) => {
                        if let Some(whole_file) = whole_file {
                            whole_file.resolving = false;
                            whole_file.resolved = Some(resolution);
                        }
                        this.refresh(cx);
                    }
                    Ok(Err(error)) => {
                        if let Some(whole_file) = whole_file {
                            whole_file.resolving = false;
                        }
                        if let Some(workspace) = workspace.upgrade() {
                            crate::git_panel::show_error_toast(
                                workspace,
                                "Resolve conflict",
                                error,
                                cx,
                            );
                        }
                    }
                    Err(_) => {}
                }
                cx.notify();
            })
            .log_err();
        }));
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
        let has_selection = self.merge_editor.is_some()
            || self
                .whole_file
                .as_ref()
                .is_some_and(|whole_file| !whole_file.resolving && whole_file.resolved.is_none());

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
                    .when(operation.is_sequenced(), |header| {
                        header.child(
                            Button::new("skip-commit", "Skip commit")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .tooltip(Tooltip::text(format!(
                                    "Drop the commit being applied, discarding its changes, \
                                     and carry on with the {operation}"
                                )))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.run_operation_action(ConflictResolutionAction::Skip, cx);
                                })),
                        )
                    })
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

    fn render_whole_file(&self, whole_file: &WholeFileConflict, cx: &Context<Self>) -> AnyElement {
        let status: Option<(SharedString, Color)> = match whole_file.resolved {
            Some(WholeFileResolution::KeepOurs) => {
                Some(("Resolved: kept ours".into(), Color::Success))
            }
            Some(WholeFileResolution::KeepTheirs) => {
                Some(("Resolved: kept theirs".into(), Color::Success))
            }
            Some(WholeFileResolution::Delete) => {
                Some(("Resolved: file deleted".into(), Color::Success))
            }
            None if whole_file.resolving => Some(("Resolving…".into(), Color::Muted)),
            None => None,
        };
        let has_ours = whole_file.stages.ours.is_some();
        let has_theirs = whole_file.stages.theirs.is_some();
        let can_delete = !has_ours || !has_theirs;
        let buttons_disabled = whole_file.resolving || whole_file.resolved.is_some();

        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_3()
            .px_4()
            .child(Label::new(whole_file.path.as_unix_str().to_string()).size(LabelSize::Large))
            .child(
                // A fixed width, capped by the pane, so the description wraps
                // instead of running past the edge of the panel.
                div()
                    .w(rems(32.))
                    .max_w_full()
                    .text_center()
                    .child(Label::new(whole_file.description()).color(Color::Muted)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .when(has_ours, |row| {
                        row.child(
                            Button::new("keep-ours", "Keep ours")
                                .style(ButtonStyle::Filled)
                                .disabled(buttons_disabled)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.resolve_whole_file(WholeFileResolution::KeepOurs, cx);
                                })),
                        )
                    })
                    .when(has_theirs, |row| {
                        row.child(
                            Button::new("keep-theirs", "Keep theirs")
                                .style(ButtonStyle::Filled)
                                .disabled(buttons_disabled)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.resolve_whole_file(WholeFileResolution::KeepTheirs, cx);
                                })),
                        )
                    })
                    .when(can_delete, |row| {
                        row.child(
                            Button::new("delete-file", "Delete file")
                                .style(ButtonStyle::Subtle)
                                .color(Color::Error)
                                .disabled(buttons_disabled)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.resolve_whole_file(WholeFileResolution::Delete, cx);
                                })),
                        )
                    }),
            )
            .when_some(status, |panel, (status, color)| {
                panel.child(Label::new(status).size(LabelSize::Small).color(color))
            })
            .into_any_element()
    }

    fn render_body(&self, cx: &Context<Self>) -> AnyElement {
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

        if let Some(whole_file) = &self.whole_file {
            return self.render_whole_file(whole_file, cx);
        }

        match &self.merge_editor {
            Some((_, merge_editor)) => v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .when_some(self.merge_note.clone(), |body, note| {
                    body.child(
                        h_flex()
                            .px_4()
                            .py_1()
                            .gap_2()
                            .border_b_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child(
                                Icon::new(IconName::Info)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(Label::new(note).size(LabelSize::Small).color(Color::Muted)),
                    )
                })
                .child(div().flex_1().min_h_0().child(merge_editor.clone()))
                .into_any_element(),
            None => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(
                    Label::new(match (&self.body_message, &self.selected_path) {
                        (Some(error), _) => error.clone(),
                        (None, Some(_)) => "Loading conflicts…".into(),
                        (None, None) => "Select a file to resolve".into(),
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

pub enum ConflictResolutionViewEvent {
    UpdateTab,
}

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

    fn is_dirty(&self, cx: &App) -> bool {
        self.merge_editor
            .as_ref()
            .is_some_and(|(_, merge_editor)| merge_editor.read(cx).is_dirty(cx))
    }

    fn can_save(&self, cx: &App) -> bool {
        self.merge_editor
            .as_ref()
            .is_some_and(|(_, merge_editor)| merge_editor.read(cx).can_save(cx))
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some((_, merge_editor)) = self.merge_editor.clone() else {
            return Task::ready(Ok(()));
        };
        merge_editor.update(cx, |merge_editor, cx| {
            merge_editor.save(options, project, window, cx)
        })
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            ConflictResolutionViewEvent::UpdateTab => f(ItemEvent::UpdateTab),
        }
    }

    fn show_toolbar(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::Fs as _;
    use git::{
        repository::repo_path,
        status::{FileStatus, UnmergedStatus, UnmergedStatusCode},
    };
    use gpui::{TestAppContext, VisualTestContext};
    use project::FakeFs;
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::Path;
    use theme::LoadThemes;
    use util::path;
    use workspace::MultiWorkspace;

    fn init_test(cx: &mut TestAppContext) {
        zlog::init_test();
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(LoadThemes::JustBase, cx);
            language_model::init(cx);
            editor::init(cx);
            crate::init(cx);
        });
    }

    #[gpui::test]
    async fn test_whole_file_conflict_resolved_by_taking_the_deletion(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        // Ours changed the file and theirs deleted it; git leaves our version
        // in the working tree without markers.
        fs.insert_tree(
            path!("/project"),
            json!({ ".git": {}, "notes.txt": "ours\n" }),
        )
        .await;
        let dot_git = Path::new(path!("/project/.git"));
        fs.set_status_for_repo(
            dot_git,
            &[(
                "notes.txt",
                FileStatus::Unmerged(UnmergedStatus {
                    first_head: UnmergedStatusCode::Updated,
                    second_head: UnmergedStatusCode::Deleted,
                }),
            )],
        );
        fs.with_git_state(dot_git, true, |state| {
            state.conflict_stages.insert(
                repo_path("notes.txt"),
                ConflictStages {
                    base: Some("base\n".into()),
                    ours: Some("ours\n".into()),
                    theirs: None,
                    binary: false,
                },
            );
        })
        .unwrap();

        let project = Project::test(fs.clone(), [Path::new(path!("/project"))], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);
        cx.run_until_parked();

        let repository = project
            .read_with(cx, |project, cx| project.active_repository(cx))
            .expect("project should have a repository");
        cx.update(|window, cx| {
            open_conflict_resolution(workspace.clone(), repository, window, cx);
        });
        cx.run_until_parked();

        let view = workspace
            .read_with(cx, |workspace, cx| {
                workspace.items_of_type::<ConflictResolutionView>(cx).next()
            })
            .expect("the conflicts tab should be open");
        view.read_with(cx, |view, _| {
            assert!(view.merge_editor.is_none());
            let whole_file = view
                .whole_file
                .as_ref()
                .expect("a deletion conflict should get the whole-file choice");
            assert_eq!(whole_file.path, repo_path("notes.txt"));
            assert_eq!(
                whole_file.description(),
                "Ours changed this file and theirs deleted it."
            );
            assert_eq!(
                whole_file.resolution_for(ConflictSide::Ours),
                WholeFileResolution::KeepOurs
            );
            assert_eq!(
                whole_file.resolution_for(ConflictSide::Theirs),
                WholeFileResolution::Delete
            );
        });

        // "File: all theirs" takes their side, which is the deletion.
        view.update(cx, |view, cx| {
            view.resolve_selected_file_with(ConflictSide::Theirs, cx)
        });
        cx.run_until_parked();

        view.read_with(cx, |view, _| {
            let whole_file = view.whole_file.as_ref().unwrap();
            assert_eq!(whole_file.resolved, Some(WholeFileResolution::Delete));
            assert!(!whole_file.resolving);
            assert_eq!(view.unresolved_count(), 0);
        });
        assert!(!fs.is_file(Path::new(path!("/project/notes.txt"))).await);
        let unmerged = fs
            .with_git_state(dot_git, false, |state| state.unmerged_paths.len())
            .unwrap();
        assert_eq!(unmerged, 0);

        // Selecting the file again must not open a buffer for it, which would
        // recreate the deleted file on save.
        view.update_in(cx, |view, window, cx| {
            view.selected_path = None;
            view.select_path(repo_path("notes.txt"), window, cx);
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.merge_editor.is_none());
            assert!(view.whole_file.is_none());
            assert_eq!(
                view.body_message.as_deref(),
                Some("notes.txt has been deleted")
            );
        });
    }

    #[gpui::test]
    async fn test_text_conflict_without_markers(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        // With a -merge attribute git leaves ours in the working copy with no
        // markers. "edited.txt" is in the same state except the user has
        // already rewritten it by hand.
        fs.insert_tree(
            path!("/project"),
            json!({ ".git": {}, "deps.lock": "ours\n", "edited.txt": "by hand\n" }),
        )
        .await;
        let dot_git = Path::new(path!("/project/.git"));
        let both_modified = FileStatus::Unmerged(UnmergedStatus {
            first_head: UnmergedStatusCode::Updated,
            second_head: UnmergedStatusCode::Updated,
        });
        fs.set_status_for_repo(
            dot_git,
            &[("deps.lock", both_modified), ("edited.txt", both_modified)],
        );
        fs.with_git_state(dot_git, true, |state| {
            for name in ["deps.lock", "edited.txt"] {
                state.conflict_stages.insert(
                    repo_path(name),
                    ConflictStages {
                        base: Some("base\n".into()),
                        ours: Some("ours\n".into()),
                        theirs: Some("theirs\n".into()),
                        binary: false,
                    },
                );
            }
        })
        .unwrap();

        let project = Project::test(fs.clone(), [Path::new(path!("/project"))], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);
        cx.run_until_parked();

        let repository = project
            .read_with(cx, |project, cx| project.active_repository(cx))
            .expect("project should have a repository");
        cx.update(|window, cx| {
            open_conflict_resolution(workspace.clone(), repository, window, cx);
        });
        cx.run_until_parked();
        let view = workspace
            .read_with(cx, |workspace, cx| {
                workspace.items_of_type::<ConflictResolutionView>(cx).next()
            })
            .expect("the conflicts tab should be open");

        view.update_in(cx, |view, window, cx| {
            view.select_path(repo_path("deps.lock"), window, cx)
        });
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert!(view.whole_file.is_none());
            let (_, merge_editor) = view
                .merge_editor
                .as_ref()
                .expect("an untouched file without markers is merged and opened");
            assert_eq!(
                merge_editor.read(cx).buffer_text(cx),
                "<<<<<<< ours\nours\n=======\ntheirs\n>>>>>>> theirs\n"
            );
            assert!(view.merge_note.is_some());
        });
        assert_eq!(
            fs.load(Path::new(path!("/project/deps.lock")))
                .await
                .unwrap(),
            "ours\n",
            "the merge only touches the buffer until saved"
        );

        view.update_in(cx, |view, window, cx| {
            view.select_path(repo_path("edited.txt"), window, cx)
        });
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert!(view.whole_file.is_none());
            assert!(view.merge_note.is_none());
            let (_, merge_editor) = view
                .merge_editor
                .as_ref()
                .expect("a file edited by hand opens normally so it can be marked resolved");
            assert_eq!(merge_editor.read(cx).buffer_text(cx), "by hand\n");
        });
    }

    #[test]
    fn test_whole_file_descriptions() {
        let conflict = |base: bool, ours: bool, theirs: bool| WholeFileConflict {
            path: repo_path("file.txt"),
            stages: ConflictStages {
                base: base.then(Vec::new),
                ours: ours.then(Vec::new),
                theirs: theirs.then(Vec::new),
                binary: false,
            },
            resolving: false,
            resolved: None,
        };
        assert_eq!(
            conflict(true, false, true).description(),
            "Ours deleted this file and theirs changed it."
        );
        // A rename on both sides leaves each new name with one version and
        // no base; that isn't a deletion by the other side.
        assert_eq!(
            conflict(false, true, false).description(),
            "Only ours has this file. It was added or renamed on our side."
        );
        assert_eq!(
            conflict(true, false, false).description(),
            "Both sides deleted or renamed this file."
        );
    }
}
