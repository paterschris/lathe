use crate::pull_request_view::PullRequestView;
use anyhow::{Context as _, Result};
use git::{
    GitHostingProvider, GitHostingProviderRegistry, ParsedGitRemote, PullRequestListFilter,
    PullRequestState, PullRequestSummary, parse_git_remote_url,
};
use gpui::{
    App, AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString,
    Task, WeakEntity,
};
use picker::{Picker, PickerDelegate, PickerEditorPosition};
use project::Project;
use std::sync::Arc;
use ui::{ListItem, ListItemSpacing, prelude::*};
use workspace::{ModalView, Workspace};

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(open);
}

pub fn open(
    workspace: &mut Workspace,
    _: &git::ListPullRequests,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let workspace_weak = cx.weak_entity();
    workspace.toggle_modal(window, cx, |window, cx| {
        PullRequestPicker::new(project, workspace_weak, ui::rems(38.), window, cx)
    });
}

pub struct PullRequestPicker {
    width: ui::Rems,
    picker: Entity<Picker<PullRequestPickerDelegate>>,
}

impl PullRequestPicker {
    pub fn new(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        width: ui::Rems,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = PullRequestPickerDelegate {
            picker_entity: cx.entity().downgrade(),
            project,
            workspace,
            host_context: None,
            all_entries: Vec::new(),
            filtered_entries: Vec::new(),
            selected_index: 0,
            status: PickerStatus::Loading,
        };
        let picker = cx.new(|cx| {
            Picker::uniform_list(delegate, window, cx)
                .max_height(Some(ui::rems(24.).into()))
                .show_scrollbar(true)
        });
        let weak_picker = picker.downgrade();
        cx.spawn(async move |_this, cx| {
            let result = load_pull_requests(weak_picker.clone(), cx).await;
            weak_picker
                .update(cx, |picker, cx| {
                    match result {
                        Ok((provider, remote, prs)) => {
                            picker.delegate.host_context = Some((provider, remote));
                            picker.delegate.all_entries = prs.clone();
                            picker.delegate.filtered_entries = prs;
                            picker.delegate.status = PickerStatus::Loaded;
                        }
                        Err(error) => {
                            picker.delegate.status =
                                PickerStatus::Failed(format!("{error}").into());
                        }
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();

        Self { picker, width }
    }
}

impl EventEmitter<DismissEvent> for PullRequestPicker {}

impl Focusable for PullRequestPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for PullRequestPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("PullRequestPicker")
            .w(self.width)
            .child(self.picker.clone())
    }
}

impl ModalView for PullRequestPicker {}

#[derive(Clone)]
enum PickerStatus {
    Loading,
    Loaded,
    Failed(SharedString),
}

pub struct PullRequestPickerDelegate {
    picker_entity: WeakEntity<PullRequestPicker>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    /// Cached after a successful load so the confirm handler can open the
    /// detail view without re-resolving the hosting provider.
    host_context: Option<(Arc<dyn GitHostingProvider + Send + Sync>, ParsedGitRemote)>,
    all_entries: Vec<PullRequestSummary>,
    filtered_entries: Vec<PullRequestSummary>,
    selected_index: usize,
    status: PickerStatus,
}

/// Look at the active repository's `origin` (or fall back to first remote),
/// match it against the registered hosting providers, and call
/// `list_pull_requests`. Returns an empty list when the active repo has no
/// recognised remote — surfaced as a loaded-but-empty picker rather than an
/// error so the UX is just "no PRs found" instead of a scary toast.
async fn load_pull_requests(
    picker: WeakEntity<Picker<PullRequestPickerDelegate>>,
    cx: &mut gpui::AsyncApp,
) -> Result<(
    Arc<dyn GitHostingProvider + Send + Sync>,
    ParsedGitRemote,
    Vec<PullRequestSummary>,
)> {
    let (provider, remote, http_client) = picker.update(cx, |picker, cx| {
        let project = picker.delegate.project.clone();
        let git_store = project.read(cx).git_store().clone();
        let active = git_store
            .read(cx)
            .active_repository()
            .context("no active repository to list PRs from")?;
        let snapshot = active.read(cx).snapshot();
        let remote_url = snapshot
            .remote_origin_url
            .clone()
            .or(snapshot.remote_upstream_url)
            .context("active repository has no origin/upstream remote")?;
        let registry = GitHostingProviderRegistry::global(cx);
        let (provider, parsed) = parse_git_remote_url(registry, &remote_url)
            .context("remote URL did not match any registered hosting provider")?;
        let http_client = cx.http_client();
        anyhow::Ok((provider, parsed, http_client))
    })??;

    let filter = PullRequestListFilter {
        states: Some(vec![PullRequestState::Open]),
        author: None,
        limit: Some(50),
    };
    // Cloning the parsed remote requires reconstruction since the struct
    // doesn't derive Clone — its two Arc<str> fields are cheap to clone.
    let remote_for_call = ParsedGitRemote {
        owner: remote.owner.clone(),
        repo: remote.repo.clone(),
    };
    let host = provider.base_url().host_str().map(|host| host.to_string());
    let auth = match host.as_deref() {
        Some(host) => git::git_host_credentials::auth_for_host(cx, host)
            .await
            .ok()
            .flatten(),
        None => None,
    };
    let summaries = provider
        .list_pull_requests(&remote_for_call, filter, auth, http_client)
        .await?;
    Ok((provider, remote, summaries))
}

impl PickerDelegate for PullRequestPickerDelegate {
    type ListItem = ListItem;

    fn match_count(&self) -> usize {
        match self.status {
            PickerStatus::Loaded => self.filtered_entries.len(),
            // Show one row in the other states so the user sees the message.
            _ => 1,
        }
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix.min(self.filtered_entries.len().saturating_sub(1));
        cx.notify();
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Filter pull requests by title or author…".into()
    }

    fn editor_position(&self) -> PickerEditorPosition {
        PickerEditorPosition::Start
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let all = self.all_entries.clone();
        cx.spawn_in(window, async move |this, cx| {
            let filtered = cx
                .background_spawn(async move {
                    if query.is_empty() {
                        return all;
                    }
                    let q = query.to_lowercase();
                    all.into_iter()
                        .filter(|pr| {
                            pr.title.to_lowercase().contains(&q)
                                || pr.author_login.to_lowercase().contains(&q)
                                || pr.number.to_string().contains(&q)
                        })
                        .collect()
                })
                .await;
            this.update(cx, |this, cx| {
                this.delegate.filtered_entries = filtered;
                this.delegate.selected_index = 0;
                cx.notify();
            })
            .ok();
        })
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if !matches!(self.status, PickerStatus::Loaded) {
            return;
        }
        let Some(pr) = self.filtered_entries.get(self.selected_index).cloned() else {
            return;
        };

        // Cmd/Ctrl-Enter opens in the host browser; plain Enter opens the
        // in-editor detail view (when the host context is available).
        if secondary || self.host_context.is_none() {
            cx.open_url(pr.url.as_str());
            self.dismissed(window, cx);
            return;
        }

        let (provider, remote) = self.host_context.as_ref().unwrap();
        let provider = provider.clone();
        let remote = ParsedGitRemote {
            owner: remote.owner.clone(),
            repo: remote.repo.clone(),
        };
        let workspace = self.workspace.clone();
        let pr_number = pr.number;

        workspace
            .update(cx, |workspace, cx| {
                let view = cx.new(|cx| {
                    PullRequestView::new(provider, remote, pr_number, workspace.weak_handle(), cx)
                });
                workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
            })
            .ok();
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.picker_entity
            .update(cx, |_this, cx| cx.emit(DismissEvent))
            .ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        match &self.status {
            PickerStatus::Loading => Some(
                ListItem::new(ix).inset(true).child(
                    Label::new("Loading pull requests…")
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                ),
            ),
            PickerStatus::Failed(message) => Some(
                ListItem::new(ix).inset(true).child(
                    Label::new(message.clone())
                        .color(Color::Error)
                        .size(LabelSize::Small),
                ),
            ),
            PickerStatus::Loaded => {
                if self.filtered_entries.is_empty() {
                    return Some(
                        ListItem::new(ix).inset(true).child(
                            Label::new("No matching pull requests")
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        ),
                    );
                }
                let pr = self.filtered_entries.get(ix)?;
                let _ = cx;
                let state_color = match pr.state {
                    PullRequestState::Open if pr.is_draft => Color::Muted,
                    PullRequestState::Open => Color::Success,
                    PullRequestState::Merged => Color::Accent,
                    PullRequestState::Closed => Color::Error,
                };
                let state_label = match pr.state {
                    PullRequestState::Open if pr.is_draft => "draft",
                    PullRequestState::Open => "open",
                    PullRequestState::Merged => "merged",
                    PullRequestState::Closed => "closed",
                };
                let primary = h_flex()
                    .gap_2()
                    .child(
                        Label::new(format!("#{}", pr.number))
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .child(Label::new(pr.title.clone()))
                    .child(
                        Label::new(state_label)
                            .color(state_color)
                            .size(LabelSize::Small),
                    );
                let secondary = h_flex()
                    .gap_2()
                    .child(
                        Label::new(pr.author_login.clone())
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .child(
                        Label::new(format!("{} → {}", pr.source_branch, pr.target_branch))
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    );
                Some(
                    ListItem::new(ix)
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .toggle_state(selected)
                        .child(v_flex().gap_1().child(primary).child(secondary)),
                )
            }
        }
    }
}

