use anyhow::{Context as _, Result};
use git::{
    GitHostingProvider, GitHostingProviderRegistry, NewPullRequest, ParsedGitRemote,
    parse_git_remote_url,
};
use gpui::{
    DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Task, WeakEntity,
};
use project::Project;
use std::sync::Arc;
use ui::{Checkbox, ToggleState, prelude::*};
use ui_input::InputField;
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::pull_request_view::PullRequestView;

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(open);
}

pub fn open(
    workspace: &mut Workspace,
    _: &crate::pull_request_panel::CreatePullRequest,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let workspace_weak = cx.weak_entity();
    workspace.toggle_modal(window, cx, |window, cx| {
        CreatePullRequestModal::new(project, workspace_weak, window, cx)
    });
}

/// What the modal needs from the repository and host before it can be filled in.
struct RepositoryContext {
    provider: Arc<dyn GitHostingProvider + Send + Sync>,
    remote: ParsedGitRemote,
}

pub struct CreatePullRequestModal {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    title_input: Entity<InputField>,
    body_input: Entity<InputField>,
    source_input: Entity<InputField>,
    target_input: Entity<InputField>,
    is_draft: bool,
    /// Resolved host and branch defaults. `None` until the initial lookup lands,
    /// or permanently when this workspace has no usable hosting remote.
    context: Option<RepositoryContext>,
    error: Option<SharedString>,
    busy: bool,
    _prepare_task: Option<Task<()>>,
    _create_task: Option<Task<()>>,
}

impl CreatePullRequestModal {
    pub fn new(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let title_input = cx.new(|cx| {
            InputField::new(window, cx, "Pull request title")
                .label("Title")
                .tab_index(1)
        });
        let body_input = cx.new(|cx| {
            InputField::new(window, cx, "Describe the change (optional)")
                .label("Description")
                .tab_index(2)
        });
        let source_input = cx.new(|cx| {
            InputField::new(window, cx, "Branch with your changes")
                .label("From")
                .tab_index(3)
        });
        let target_input = cx.new(|cx| {
            InputField::new(window, cx, "Branch to merge into")
                .label("Into")
                .tab_index(4)
        });
        window.focus(&title_input.focus_handle(cx), cx);

        let mut this = Self {
            project,
            workspace,
            focus_handle: cx.focus_handle(),
            title_input,
            body_input,
            source_input,
            target_input,
            is_draft: false,
            context: None,
            error: None,
            busy: false,
            _prepare_task: None,
            _create_task: None,
        };
        this.prepare(window, cx);
        this
    }

    /// Resolves the host from the repository's remote and prefills the branch
    /// fields: source from the checked-out branch, target from the host's
    /// default branch.
    fn prepare(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let project = self.project.clone();
        let http_client = cx.http_client();
        self._prepare_task = Some(cx.spawn_in(window, async move |this, cx| {
            let resolved = cx.update(|_window, cx| {
                let git_store = project.read(cx).git_store().clone();
                let active = git_store.read(cx).active_repository()?;
                let snapshot = active.read(cx).snapshot();
                let remote_url = snapshot.remote_origin_url.clone()?;
                let branch = snapshot
                    .branch
                    .as_ref()
                    .map(|branch| SharedString::from(branch.name().to_string()));
                let registry = GitHostingProviderRegistry::global(cx);
                let (provider, remote) = parse_git_remote_url(registry, &remote_url)?;
                Some((provider, remote, branch))
            });

            let Some((provider, remote, branch)) = resolved.ok().flatten() else {
                this.update(cx, |this, cx| {
                    this.error = Some(
                        "This workspace has no git repository with a supported hosting remote."
                            .into(),
                    );
                    cx.notify();
                })
                .ok();
                return;
            };

            let host = provider.base_url().host_str().map(|host| host.to_string());
            let auth = match host.as_deref() {
                Some(host) => git::git_host_credentials::auth_for_host(cx, host)
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            // Best-effort: an unreachable host still lets the user type a target
            // branch by hand rather than blocking the whole modal.
            let default_branch = provider
                .default_branch(
                    &ParsedGitRemote {
                        owner: remote.owner.clone(),
                        repo: remote.repo.clone(),
                    },
                    auth,
                    http_client,
                )
                .await
                .log_err()
                .flatten()
                .unwrap_or_else(|| SharedString::from("main"));

            this.update_in(cx, |this, window, cx| {
                let source_branch = branch.unwrap_or_default();
                this.context = Some(RepositoryContext { provider, remote });
                this.source_input.update(cx, |input, cx| {
                    input.set_text(source_branch.as_ref(), window, cx);
                });
                this.target_input.update(cx, |input, cx| {
                    input.set_text(default_branch.as_ref(), window, cx);
                });
                cx.notify();
            })
            .ok();
        }));
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(context) = self.context.as_ref() else {
            return;
        };
        let title = self.title_input.read(cx).text(cx).trim().to_string();
        let body = self.body_input.read(cx).text(cx).trim().to_string();
        let source_branch = self.source_input.read(cx).text(cx).trim().to_string();
        let target_branch = self.target_input.read(cx).text(cx).trim().to_string();

        if title.is_empty() {
            self.error = Some("Give the pull request a title.".into());
            cx.notify();
            return;
        }
        if source_branch.is_empty() || target_branch.is_empty() {
            self.error = Some("Both branches are required.".into());
            cx.notify();
            return;
        }
        if source_branch == target_branch {
            self.error = Some("A pull request needs two different branches.".into());
            cx.notify();
            return;
        }

        self.busy = true;
        self.error = None;
        cx.notify();

        let provider = context.provider.clone();
        let remote = ParsedGitRemote {
            owner: context.remote.owner.clone(),
            repo: context.remote.repo.clone(),
        };
        let request = NewPullRequest {
            title: title.into(),
            body: body.into(),
            source_branch: source_branch.into(),
            target_branch: target_branch.into(),
            is_draft: self.is_draft,
        };
        let http_client = cx.http_client();
        let workspace = self.workspace.clone();

        self._create_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = create(provider.clone(), remote, request, http_client, cx).await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok((summary, remote)) => {
                        // Open the new pull request straight away: creating one
                        // is almost always followed by looking at it.
                        workspace
                            .update(cx, |workspace, cx| {
                                let view = cx.new(|cx| {
                                    PullRequestView::new(
                                        provider,
                                        remote,
                                        summary.number,
                                        workspace.weak_handle(),
                                        cx,
                                    )
                                });
                                workspace.add_item_to_active_pane(
                                    Box::new(view),
                                    None,
                                    true,
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                        cx.emit(DismissEvent);
                    }
                    Err(error) => {
                        this.error = Some(format!("{error:#}").into());
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
    }

    fn on_confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.submit(window, cx);
    }

    fn on_tab(&mut self, _: &menu::SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        window.focus_next(cx);
    }

    fn on_tab_prev(
        &mut self,
        _: &menu::SelectPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus_prev(cx);
    }
}

async fn create(
    provider: Arc<dyn GitHostingProvider + Send + Sync>,
    remote: ParsedGitRemote,
    request: NewPullRequest,
    http_client: Arc<dyn gpui::http_client::HttpClient>,
    cx: &mut gpui::AsyncApp,
) -> Result<(git::PullRequestSummary, ParsedGitRemote)> {
    let host = provider
        .base_url()
        .host_str()
        .map(|host| host.to_string())
        .context("hosting provider has no host")?;
    let auth = git::git_host_credentials::auth_for_host(cx, &host)
        .await
        .ok()
        .flatten();
    let remote_for_call = ParsedGitRemote {
        owner: remote.owner.clone(),
        repo: remote.repo.clone(),
    };
    let summary = provider
        .create_pull_request(&remote_for_call, request, auth, http_client)
        .await?;
    Ok((summary, remote))
}

impl EventEmitter<DismissEvent> for CreatePullRequestModal {}

impl Focusable for CreatePullRequestModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for CreatePullRequestModal {}

impl Render for CreatePullRequestModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ready = self.context.is_some();
        let host_label = self
            .context
            .as_ref()
            .map(|context| format!("{}/{}", context.remote.owner, context.remote.repo));

        v_flex()
            .key_context("CreatePullRequestModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_confirm))
            .on_action(cx.listener(Self::on_tab))
            .on_action(cx.listener(Self::on_tab_prev))
            .elevation_3(cx)
            .w(rems(34.))
            .p_4()
            .gap_3()
            .child(
                v_flex()
                    .gap_0p5()
                    .child(Label::new("New Pull Request").size(LabelSize::Large))
                    .when_some(host_label, |this, repo| {
                        this.child(
                            Label::new(repo)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    }),
            )
            .child(self.title_input.clone())
            .child(self.body_input.clone())
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(self.source_input.clone()))
                    .child(div().flex_1().child(self.target_input.clone())),
            )
            .child(
                Checkbox::new(
                    "create-pr-draft",
                    if self.is_draft {
                        ToggleState::Selected
                    } else {
                        ToggleState::Unselected
                    },
                )
                .label("Create as draft")
                .on_click(cx.listener(|this, state: &ToggleState, _window, cx| {
                    this.is_draft = *state == ToggleState::Selected;
                    cx.notify();
                })),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).color(Color::Error).size(LabelSize::Small))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("cancel-create-pr", "Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        Button::new(
                            "submit-create-pr",
                            if self.busy { "Creating…" } else { "Create" },
                        )
                        .disabled(self.busy || !ready)
                        .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}
