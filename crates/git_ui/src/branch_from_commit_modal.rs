use editor::Editor;
use gpui::{DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, WeakEntity};
use project::git_store::Repository;
use ui::{
    ActiveTheme, App, Button, ButtonCommon, ButtonStyle, Clickable, Color, Context, Divider,
    FluentBuilder, InteractiveElement, IntoElement, Label, LabelCommon, LabelSize, ParentElement,
    Render, SharedString, StyledExt, Window, h_flex, prelude::*, v_flex,
};
use workspace::{ModalView, Workspace};

/// Modal triggered from the git graph's "Branch from here..." entry. Takes a
/// branch name from the user and runs `git switch -c <name> <sha>` so the new
/// branch is created at the chosen commit and immediately checked out.
pub struct BranchFromCommitModal {
    name_editor: Entity<Editor>,
    repository: Entity<Repository>,
    sha: SharedString,
    short_sha: SharedString,
    workspace: WeakEntity<Workspace>,
    in_progress: bool,
    last_error: Option<SharedString>,
}

impl EventEmitter<DismissEvent> for BranchFromCommitModal {}
impl ModalView for BranchFromCommitModal {}

impl Focusable for BranchFromCommitModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name_editor.read(cx).focus_handle(cx)
    }
}

impl BranchFromCommitModal {
    pub fn new(
        sha: SharedString,
        short_sha: SharedString,
        repository: Entity<Repository>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("branch name", window, cx);
            editor
        });
        Self {
            name_editor,
            repository,
            sha,
            short_sha,
            workspace,
            in_progress: false,
            last_error: None,
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.create(window, cx);
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.in_progress {
            return;
        }
        let raw = self.name_editor.read(cx).text(cx);
        let name = normalize_branch_name(&raw);
        if name.is_empty() {
            self.last_error = Some("Branch name is required.".into());
            cx.notify();
            return;
        }

        let repository = self.repository.clone();
        let sha = self.sha.to_string();
        let workspace = self.workspace.clone();

        let receiver = repository.update(cx, |repo, _cx| {
            repo.create_branch(name.clone(), Some(sha.clone()))
        });

        self.in_progress = true;
        self.last_error = None;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let result = receiver.await;
            this.update(cx, |this, cx| match result {
                Ok(Ok(())) => {
                    let _ = workspace;
                    cx.emit(DismissEvent);
                }
                Ok(Err(error)) => {
                    this.in_progress = false;
                    this.last_error = Some(format!("{error}").into());
                    cx.notify();
                }
                Err(_) => {
                    this.in_progress = false;
                    this.last_error = Some("Branch creation cancelled".into());
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }
}

impl Render for BranchFromCommitModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let in_progress = self.in_progress;
        let theme = cx.theme();
        let target_label: SharedString = format!("Create at commit {}", self.short_sha).into();

        v_flex()
            .key_context("BranchFromCommitModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_3(cx)
            .w(ui::rems(28.))
            .child(
                v_flex()
                    .w_full()
                    .p_3()
                    .border_b_1()
                    .border_color(theme.colors().border_variant)
                    .child(Label::new("Branch from here").size(LabelSize::Large))
                    .child(
                        Label::new(target_label)
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    ),
            )
            .child(
                v_flex()
                    .w_full()
                    .p_3()
                    .gap_2()
                    .child(
                        Label::new("Branch name")
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .child(self.name_editor.clone().into_any_element()),
            )
            .child(Divider::horizontal())
            .child(
                h_flex()
                    .w_full()
                    .p_3()
                    .gap_2()
                    .justify_between()
                    .when_some(self.last_error.clone(), |this, error| {
                        this.child(Label::new(error).color(Color::Error).size(LabelSize::Small))
                    })
                    .when(self.last_error.is_none(), |this| {
                        this.child(
                            Label::new("Press Enter to create the branch.")
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("branch-from-cancel", "Cancel")
                                    .style(ButtonStyle::Subtle)
                                    .disabled(in_progress)
                                    .on_click(cx.listener(|_, _, _window, cx| {
                                        cx.emit(DismissEvent);
                                    })),
                            )
                            .child(
                                Button::new(
                                    "branch-from-create",
                                    if in_progress {
                                        "Creating…"
                                    } else {
                                        "Create branch"
                                    },
                                )
                                .style(ButtonStyle::Filled)
                                .disabled(in_progress)
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.create(window, cx);
                                    },
                                )),
                            ),
                    ),
            )
    }
}

fn normalize_branch_name(query: &str) -> String {
    query.trim().replace(' ', "-")
}
