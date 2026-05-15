use git::repository::{RebaseAction, RebaseTodoEntry};
use gpui::{DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, WeakEntity};
use project::git_store::{GitStore, RepositoryId, undo_log::UndoAction};
use ui::{
    ActiveTheme, App, Button, ButtonCommon, ButtonStyle, Clickable, Color, Context, Divider,
    FluentBuilder, InteractiveElement, IntoElement, Label, LabelCommon, LabelSize, ParentElement,
    Render, SharedString, StyledExt, Window, h_flex, prelude::*, v_flex,
};
use workspace::{ModalView, Workspace};

/// One row in the interactive-rebase plan. Holds the commit SHA, a short label
/// for display, and the action the user has chosen to apply.
#[derive(Clone)]
pub struct RebasePlanEntry {
    pub sha: SharedString,
    pub short_sha: SharedString,
    pub subject: SharedString,
    pub action: RebaseAction,
}

/// Modal that mirrors `git rebase -i`. The user sees the list of commits to be
/// replayed (oldest at the top, matching git's own todo file), can cycle the
/// per-row action, and clicks "Start rebase" to run it.
pub struct InteractiveRebaseModal {
    focus_handle: FocusHandle,
    entries: Vec<RebasePlanEntry>,
    /// Argument passed to `git rebase -i <upstream>` — the commit *just before*
    /// the oldest entry. Everything after this gets rewritten.
    upstream: SharedString,
    repo_id: RepositoryId,
    git_store: Entity<GitStore>,
    workspace: WeakEntity<Workspace>,
    /// Branch + tip captured before the rebase so the Undo toast can restore.
    pre_state: Option<(String, String)>,
    in_progress: bool,
    last_error: Option<SharedString>,
}

impl EventEmitter<DismissEvent> for InteractiveRebaseModal {}
impl ModalView for InteractiveRebaseModal {}

impl Focusable for InteractiveRebaseModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl InteractiveRebaseModal {
    pub fn new(
        entries: Vec<RebasePlanEntry>,
        upstream: impl Into<SharedString>,
        repo_id: RepositoryId,
        git_store: Entity<GitStore>,
        workspace: WeakEntity<Workspace>,
        pre_state: Option<(String, String)>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            entries,
            upstream: upstream.into(),
            repo_id,
            git_store,
            workspace,
            pre_state,
            in_progress: false,
            last_error: None,
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn cycle_action(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get_mut(index) else {
            return;
        };
        entry.action = next_action(entry.action);
        cx.notify();
    }

    fn start_rebase(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.in_progress {
            return;
        }
        // If every row is "pick", running the rebase would be a no-op and just
        // potentially trigger conflicts. Treat as cancel.
        if self.entries.iter().all(|e| e.action == RebaseAction::Pick) {
            cx.emit(DismissEvent);
            return;
        }

        let todo: Vec<RebaseTodoEntry> = self
            .entries
            .iter()
            .map(|entry| RebaseTodoEntry {
                action: entry.action,
                commit: entry.sha.to_string(),
            })
            .collect();
        let upstream = self.upstream.to_string();
        let repo_id = self.repo_id;
        let git_store = self.git_store.clone();
        let workspace = self.workspace.clone();
        let pre_state = self.pre_state.clone();

        let Some(repo) = git_store.read(cx).repositories().get(&repo_id).cloned() else {
            return;
        };
        let receiver = repo.update(cx, |repo, _cx| {
            repo.rebase_interactive(upstream, todo)
        });

        self.in_progress = true;
        self.last_error = None;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            match receiver.await {
                Ok(Ok(())) => {
                    if let Some((branch, sha)) = pre_state.clone() {
                        cx.update(|_, cx| {
                            git_store.update(cx, |store, cx| {
                                store.record_undo(
                                    repo_id,
                                    "Interactive rebase".to_string(),
                                    UndoAction::RestoreBranchTip {
                                        branch,
                                        sha,
                                        is_current: true,
                                    },
                                    cx,
                                );
                            });
                            let _ = workspace;
                        })
                        .ok();
                    }
                    this.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
                }
                Ok(Err(error)) => {
                    let message: SharedString = format!("{error}").into();
                    this.update(cx, |this, cx| {
                        this.in_progress = false;
                        this.last_error = Some(message);
                        cx.notify();
                    })
                    .ok();
                }
                Err(_) => {
                    this.update(cx, |this, cx| {
                        this.in_progress = false;
                        this.last_error = Some("Rebase cancelled".into());
                        cx.notify();
                    })
                    .ok();
                }
            }
        })
        .detach();
    }
}

fn next_action(action: RebaseAction) -> RebaseAction {
    match action {
        RebaseAction::Pick => RebaseAction::Reword,
        RebaseAction::Reword => RebaseAction::Squash,
        RebaseAction::Squash => RebaseAction::Fixup,
        RebaseAction::Fixup => RebaseAction::Drop,
        RebaseAction::Drop => RebaseAction::Pick,
        RebaseAction::Edit => RebaseAction::Pick,
    }
}

fn action_label(action: RebaseAction) -> &'static str {
    match action {
        RebaseAction::Pick => "pick",
        RebaseAction::Reword => "reword",
        RebaseAction::Squash => "squash",
        RebaseAction::Fixup => "fixup",
        RebaseAction::Drop => "drop",
        RebaseAction::Edit => "edit",
    }
}

fn action_color(action: RebaseAction) -> Color {
    match action {
        RebaseAction::Pick => Color::Default,
        RebaseAction::Reword => Color::Accent,
        RebaseAction::Squash | RebaseAction::Fixup => Color::Warning,
        RebaseAction::Drop => Color::Error,
        RebaseAction::Edit => Color::Info,
    }
}

impl Render for InteractiveRebaseModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let entry_count = self.entries.len();
        let in_progress = self.in_progress;

        let rows = self.entries.iter().enumerate().map(|(idx, entry)| {
            let action = entry.action;
            let chip_label: SharedString = action_label(action).into();
            let chip_color = action_color(action);
            let short_sha = entry.short_sha.clone();
            let subject = entry.subject.clone();
            h_flex()
                .w_full()
                .gap_2()
                .py_1()
                .px_3()
                .child(
                    Button::new(("rebase-action", idx), chip_label)
                        .label_size(LabelSize::Small)
                        .color(chip_color)
                        .style(ButtonStyle::Outlined)
                        .disabled(in_progress)
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.cycle_action(idx, cx);
                        })),
                )
                .child(
                    Label::new(short_sha)
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .child(Label::new(subject).size(LabelSize::Small))
                .into_any_element()
        });

        let count_label: SharedString = format!("{entry_count} commit(s) onto {}", self.upstream)
            .into();

        v_flex()
            .key_context("InteractiveRebaseModal")
            .on_action(cx.listener(Self::cancel))
            .elevation_3(cx)
            .w(ui::rems(36.))
            .max_h(ui::rems(40.))
            .overflow_hidden()
            .child(
                v_flex()
                    .w_full()
                    .p_3()
                    .border_b_1()
                    .border_color(theme.colors().border_variant)
                    .child(Label::new("Interactive rebase").size(LabelSize::Large))
                    .child(Label::new(count_label).color(Color::Muted).size(LabelSize::Small)),
            )
            .child(
                v_flex()
                    .w_full()
                    .flex_1()
                    .py_2()
                    .children(rows),
            )
            .child(Divider::horizontal())
            .child(
                h_flex()
                    .w_full()
                    .p_3()
                    .gap_2()
                    .justify_between()
                    .when_some(self.last_error.clone(), |this, error| {
                        this.child(
                            Label::new(error)
                                .color(Color::Error)
                                .size(LabelSize::Small),
                        )
                    })
                    .when(self.last_error.is_none(), |this| {
                        this.child(
                            Label::new("Click an action to cycle (pick → reword → squash → fixup → drop)")
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("rebase-cancel", "Cancel")
                                    .style(ButtonStyle::Subtle)
                                    .disabled(in_progress)
                                    .on_click(cx.listener(|_, _, _window, cx| {
                                        cx.emit(DismissEvent);
                                    })),
                            )
                            .child(
                                Button::new("rebase-start", if in_progress {
                                    "Rebasing…"
                                } else {
                                    "Start rebase"
                                })
                                .style(ButtonStyle::Filled)
                                .disabled(in_progress)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.start_rebase(window, cx);
                                })),
                            ),
                    ),
            )
    }
}
