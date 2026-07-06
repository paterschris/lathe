use git::Oid;
use git::repository::{CommitSummary, RebaseAction, RebaseOptions};
use gpui::{
    DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Task, WeakEntity,
};
use project::git_store::{GitStore, Repository, RepositoryId, undo_log::UndoAction};
use ui::{
    ActiveTheme, App, Button, ButtonCommon, ButtonStyle, Clickable, Color, Context, Disableable,
    Divider, FluentBuilder, InteractiveElement, IntoElement, Label, LabelCommon, LabelSize,
    ParentElement, Render, SharedString, StyledExt, Window, h_flex, prelude::*, v_flex,
};
use workspace::{ModalView, Workspace};

use crate::interactive_rebase_modal::{InteractiveRebaseModal, RebasePlanEntry};

/// Tracks the async load of the commit range that a rebase would replay.
enum LoadState {
    Loading,
    Loaded(Vec<CommitSummary>),
    Error(SharedString),
}

/// Confirmation shown before a drag-and-drop rebase runs. It previews how many
/// commits would be replayed and lets the user either run a plain rebase or
/// escalate to an interactive rebase (`git rebase -i`). Without it, dropping a
/// branch rewrote history silently.
pub struct RebaseConfirmModal {
    focus_handle: FocusHandle,
    /// Branch whose commits get replayed; becomes HEAD before the rebase runs.
    source: SharedString,
    source_is_current: bool,
    /// Ref the source is rebased onto (the `git rebase <upstream>` argument).
    target_ref: SharedString,
    /// Human-friendly label for `target_ref` shown in the dialog.
    target_label: SharedString,
    repo_id: RepositoryId,
    git_store: Entity<GitStore>,
    repository: Entity<Repository>,
    workspace: WeakEntity<Workspace>,
    state: LoadState,
    in_progress: bool,
    _load_task: Task<()>,
}

impl EventEmitter<DismissEvent> for RebaseConfirmModal {}
impl ModalView for RebaseConfirmModal {}

impl Focusable for RebaseConfirmModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl RebaseConfirmModal {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source: impl Into<SharedString>,
        source_is_current: bool,
        target_ref: impl Into<SharedString>,
        target_label: impl Into<SharedString>,
        repo_id: RepositoryId,
        git_store: Entity<GitStore>,
        repository: Entity<Repository>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let source = source.into();
        let target_ref = target_ref.into();
        // Commits on `source` that are not on `target_ref` are exactly what a
        // `git rebase <target_ref>` (run while on `source`) would replay.
        let range = format!("{target_ref}..{source}");
        let receiver = repository.update(cx, |repo, _| repo.commits_in_range(range));
        let load_task = cx.spawn(async move |this, cx| {
            let result = receiver.await;
            this.update(cx, |this, cx| {
                this.state = match result {
                    Ok(Ok(commits)) => LoadState::Loaded(commits),
                    Ok(Err(error)) => LoadState::Error(format!("{error}").into()),
                    Err(_) => LoadState::Error("Could not determine commits to rebase".into()),
                };
                cx.notify();
            })
            .ok();
        });

        Self {
            focus_handle: cx.focus_handle(),
            source,
            source_is_current,
            target_ref,
            target_label: target_label.into(),
            repo_id,
            git_store,
            repository,
            workspace,
            state: LoadState::Loading,
            in_progress: false,
            _load_task: load_task,
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn loaded_commits(&self) -> Option<&Vec<CommitSummary>> {
        match &self.state {
            LoadState::Loaded(commits) => Some(commits),
            _ => None,
        }
    }

    /// Run a non-interactive `git rebase <target_ref>` after switching to the
    /// source branch, recording an undo entry that restores its prior tip.
    fn perform_rebase(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.in_progress {
            return;
        }
        let Some(commits) = self.loaded_commits() else {
            return;
        };
        if commits.is_empty() {
            cx.emit(DismissEvent);
            return;
        }
        // The newest commit in the range is the source branch tip before the
        // rebase; restoring it is what an undo of this rebase does.
        let undo_sha = commits.first().map(|commit| commit.sha.to_string());

        let source = self.source.clone();
        let target_ref = self.target_ref.to_string();
        let target_label = self.target_label.clone();
        let repository = self.repository.clone();
        let git_store = self.git_store.clone();
        let repo_id = self.repo_id;
        let workspace = self.workspace.clone();

        let checkout_receiver = if self.source_is_current {
            None
        } else {
            Some(repository.update(cx, |repo, _| repo.change_branch(source.to_string())))
        };

        self.in_progress = true;
        cx.notify();

        cx.spawn(async move |this, cx| {
            if let Some(receiver) = checkout_receiver {
                match receiver.await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        if let Some(workspace) = workspace.upgrade() {
                            let _ = cx.update(|cx| {
                                crate::git_panel::show_error_toast(
                                    workspace,
                                    format!("checkout {source}"),
                                    error,
                                    cx,
                                )
                            });
                        }
                        this.update(cx, |this, cx| {
                            this.in_progress = false;
                            cx.notify();
                        })
                        .ok();
                        return;
                    }
                    Err(_) => {
                        this.update(cx, |this, cx| {
                            this.in_progress = false;
                            cx.notify();
                        })
                        .ok();
                        return;
                    }
                }
            }

            let rebase_receiver = this
                .update(cx, |_, cx| {
                    repository.update(cx, |repo, _| {
                        repo.rebase(target_ref.clone(), RebaseOptions::default())
                    })
                })
                .ok();
            let Some(rebase_receiver) = rebase_receiver else {
                this.update(cx, |this, cx| {
                    this.in_progress = false;
                    cx.notify();
                })
                .ok();
                return;
            };

            match rebase_receiver.await {
                Ok(Ok(())) => {
                    if let Some(sha) = undo_sha {
                        cx.update(|cx| {
                            git_store.update(cx, |store, cx| {
                                store.record_undo(
                                    repo_id,
                                    format!("Rebase {source} onto {target_label}"),
                                    UndoAction::RestoreBranchTip {
                                        branch: source.to_string(),
                                        sha,
                                        is_current: true,
                                    },
                                    cx,
                                );
                            });
                        });
                    }
                    this.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
                }
                Ok(Err(error)) => {
                    if let Some(workspace) = workspace.upgrade() {
                        let _ = cx.update(|cx| {
                            crate::git_panel::show_error_toast(
                                workspace,
                                format!("rebase onto {target_label}"),
                                error,
                                cx,
                            )
                        });
                    }
                    this.update(cx, |this, cx| {
                        this.in_progress = false;
                        cx.notify();
                    })
                    .ok();
                }
                Err(_) => {
                    this.update(cx, |this, cx| {
                        this.in_progress = false;
                        cx.notify();
                    })
                    .ok();
                }
            }
        })
        .detach();
    }

    /// Switch to the source branch (so HEAD is correct) and hand off to the
    /// interactive rebase modal, pre-populated with the commits to replay.
    fn start_interactive(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.in_progress {
            return;
        }
        let Some(commits) = self.loaded_commits() else {
            return;
        };
        if commits.is_empty() {
            return;
        }
        // `commits_in_range` returns newest first; the rebase todo lists the
        // oldest commit at the top, matching git's own todo file.
        let plan_entries: Vec<RebasePlanEntry> = commits
            .iter()
            .rev()
            .map(|commit| {
                let short_sha = commit
                    .sha
                    .parse::<Oid>()
                    .map(|oid| oid.display_short())
                    .unwrap_or_else(|_| commit.sha.chars().take(7).collect());
                RebasePlanEntry {
                    sha: commit.sha.clone(),
                    short_sha: short_sha.into(),
                    action: RebaseAction::Pick,
                }
            })
            .collect();
        let pre_state = commits
            .first()
            .map(|commit| (self.source.to_string(), commit.sha.to_string()));

        let source = self.source.clone();
        let upstream = self.target_ref.clone();
        let repo_id = self.repo_id;
        let git_store = self.git_store.clone();
        let repository = self.repository.clone();
        let workspace = self.workspace.clone();
        let modal_workspace = self.workspace.clone();

        let checkout_receiver = if self.source_is_current {
            None
        } else {
            Some(repository.update(cx, |repo, _| repo.change_branch(source.to_string())))
        };

        cx.emit(DismissEvent);

        cx.spawn_in(window, async move |_, cx| {
            if let Some(receiver) = checkout_receiver {
                match receiver.await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        log::error!("checkout before interactive rebase failed: {error:?}");
                        return;
                    }
                    Err(_) => return,
                }
            }
            workspace
                .update_in(cx, move |workspace, window, cx| {
                    workspace.toggle_modal(window, cx, move |_window, cx| {
                        InteractiveRebaseModal::new(
                            plan_entries,
                            upstream,
                            repo_id,
                            git_store,
                            repository,
                            modal_workspace,
                            pre_state,
                            cx,
                        )
                    });
                })
                .ok();
        })
        .detach();
    }
}

impl Render for RebaseConfirmModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let in_progress = self.in_progress;
        let theme = cx.theme();
        let header = format!("Rebase {}", self.source);
        let subtitle: SharedString = format!("onto {}", self.target_label).into();

        let body = v_flex().w_full().p_3().gap_2();
        let body = match &self.state {
            LoadState::Loading => body.child(
                Label::new("Calculating commits to replay…")
                    .color(Color::Muted)
                    .size(LabelSize::Small),
            ),
            LoadState::Error(message) => body.child(
                Label::new(message.clone())
                    .color(Color::Error)
                    .size(LabelSize::Small),
            ),
            LoadState::Loaded(commits) if commits.is_empty() => body.child(
                Label::new(format!(
                    "{} is already up to date with {}. Nothing to rebase.",
                    self.source, self.target_label
                ))
                .color(Color::Muted)
                .size(LabelSize::Small),
            ),
            LoadState::Loaded(commits) => {
                let count = commits.len();
                let summary = format!(
                    "{count} commit{} from {} will be replayed onto {}.",
                    if count == 1 { "" } else { "s" },
                    self.source,
                    self.target_label
                );
                body.child(Label::new(summary).size(LabelSize::Small)).when(
                    count > 1,
                    |this| {
                        this.child(
                            Label::new(
                                "An interactive rebase lets you reorder, squash, edit, \
                                 or drop these commits.",
                            )
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                        )
                    },
                )
            }
        };

        let can_rebase = matches!(&self.state, LoadState::Loaded(commits) if !commits.is_empty());

        let mut buttons = h_flex().w_full().p_3().gap_1().justify_end();
        buttons = buttons.child(
            Button::new("rebase-confirm-cancel", "Cancel")
                .style(ButtonStyle::Subtle)
                .disabled(in_progress)
                .on_click(cx.listener(|_, _, _window, cx| cx.emit(DismissEvent))),
        );
        if can_rebase {
            buttons = buttons
                .child(
                    Button::new("rebase-confirm-interactive", "Interactive rebase…")
                        .style(ButtonStyle::Outlined)
                        .disabled(in_progress)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.start_interactive(window, cx);
                        })),
                )
                .child(
                    Button::new("rebase-confirm-run", "Rebase")
                        .style(ButtonStyle::Filled)
                        .disabled(in_progress)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.perform_rebase(window, cx);
                        })),
                );
        }

        v_flex()
            .key_context("RebaseConfirmModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .elevation_3(cx)
            .w(ui::rems(30.))
            .child(
                v_flex()
                    .w_full()
                    .p_3()
                    .border_b_1()
                    .border_color(theme.colors().border_variant)
                    .child(Label::new(header).size(LabelSize::Large))
                    .child(
                        Label::new(subtitle)
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    ),
            )
            .child(body)
            .child(Divider::horizontal())
            .child(buttons)
    }
}
