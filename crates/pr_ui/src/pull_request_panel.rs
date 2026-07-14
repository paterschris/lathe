use crate::pull_request_panel_settings::PullRequestPanelSettings;
use crate::pull_request_view::PullRequestView;
use anyhow::{Context as _, Result};
use git::{
    GitHostingProvider, GitHostingProviderRegistry, ParsedGitRemote, PullRequestListFilter,
    PullRequestReviewVerdict, PullRequestState, PullRequestSummary, parse_git_remote_url,
};
use gpui::http_client::HttpClient;
use gpui::{
    Action, AppContext as _, AsyncWindowContext, Entity, EventEmitter, FocusHandle, Focusable,
    MouseButton, SharedString, Subscription, Task, UniformListScrollHandle, WeakEntity, actions,
    uniform_list,
};
use project::{
    Project,
    git_store::{GitStore, GitStoreEvent},
};
use settings::Settings;
use std::collections::HashMap;
use std::sync::Arc;
use ui::{ContextMenu, PopoverMenu, Tooltip, prelude::*};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    pull_request_panel,
    [
        /// Toggles focus on the pull request panel.
        ToggleFocus,
    ]
);

const PR_PANEL_KEY: &str = "PullRequestPanel";
const ROW_HEIGHT_REMS: f32 = 2.4;

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
        workspace.toggle_panel_focus::<PullRequestPanel>(window, cx);
    });
}

#[derive(Clone)]
enum LoadState {
    Idle,
    Loading,
    Loaded(Vec<PullRequestSummary>),
    NoHost(SharedString),
    Failed(SharedString),
    /// A pull-request call returned HTTP 401; the stored credential for `host`
    /// is expired or invalid, so the user is offered a targeted reconnect.
    AuthExpired {
        host: SharedString,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StateFilter {
    Open,
    Closed,
    Merged,
    All,
}

impl StateFilter {
    fn label(&self) -> &'static str {
        match self {
            StateFilter::Open => "Open",
            StateFilter::Closed => "Closed",
            StateFilter::Merged => "Merged",
            StateFilter::All => "All",
        }
    }

    fn states(&self) -> Option<Vec<PullRequestState>> {
        match self {
            StateFilter::Open => Some(vec![PullRequestState::Open]),
            StateFilter::Closed => Some(vec![PullRequestState::Closed]),
            StateFilter::Merged => Some(vec![PullRequestState::Merged]),
            StateFilter::All => None,
        }
    }
}

pub struct PullRequestPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    state: LoadState,
    host_context: Option<(Arc<dyn GitHostingProvider + Send + Sync>, ParsedGitRemote)>,
    scroll_handle: UniformListScrollHandle,
    filter: StateFilter,
    /// When set, restrict the list to PRs the connected account is a requested
    /// reviewer of. Combines with `filter` (the state selection).
    reviewing: bool,
    /// PR number most recently opened from this panel; highlighted in the list.
    selected_pr: Option<u32>,
    _load_task: Option<Task<()>>,
    /// Cached "my review verdict" per PR number, filled in lazily in the
    /// background after the list loads so already-reviewed rows can be tinted.
    viewer_reviews: HashMap<u32, Option<PullRequestReviewVerdict>>,
    _enrich_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl PullRequestPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            Self::new(workspace, window, cx)
        })
    }

    pub fn new(
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let project = workspace.project().clone();
        let git_store = project.read(cx).git_store().clone();
        let workspace_weak = workspace.weak_handle();
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();
            // Re-resolve host + reload when the active repository changes;
            // each repo can point at a different hosting provider, so the
            // cached `host_context` is per-active-repo.
            let subscriptions = vec![
                cx.subscribe(&git_store, Self::on_git_store_event),
                // Reload when a host is connected or disconnected, so a
                // reconnected account leaves the AuthExpired state without the
                // user reopening the panel.
                git::git_host_credentials::observe_connections(cx, |this, cx| {
                    this.host_context = None;
                    this.kick_off_refresh(cx);
                }),
            ];
            let mut this = Self {
                workspace: workspace_weak,
                project,
                focus_handle,
                state: LoadState::Idle,
                host_context: None,
                scroll_handle: UniformListScrollHandle::default(),
                filter: StateFilter::Open,
                reviewing: false,
                selected_pr: None,
                _load_task: None,
                viewer_reviews: HashMap::new(),
                _enrich_task: None,
                _subscriptions: subscriptions,
            };
            this.kick_off_refresh(cx);
            this
        })
    }

    fn on_git_store_event(
        &mut self,
        _: Entity<GitStore>,
        event: &GitStoreEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            // The active repository changed entirely: re-resolve the host (each
            // repo can point at a different provider) and reload.
            GitStoreEvent::ActiveRepositoryChanged(_) => {
                self.host_context = None;
                self.kick_off_refresh(cx);
            }
            // The active repository's data updated. At launch the repository is
            // present before its remote URLs are scanned, so the first resolve
            // sees no remote and parks in NoHost; retry once the repo updates so
            // the panel populates without a manual refresh. Gated on not having
            // resolved a host yet (state Idle/NoHost) so ordinary git activity on
            // an already-loaded panel does not re-hit the hosting API.
            GitStoreEvent::RepositoryUpdated(_, _, true) => {
                if self.host_context.is_none()
                    && matches!(self.state, LoadState::Idle | LoadState::NoHost(_))
                {
                    self.kick_off_refresh(cx);
                }
            }
            _ => {}
        }
    }

    fn kick_off_refresh(&mut self, cx: &mut Context<Self>) {
        self.state = LoadState::Loading;
        cx.notify();
        let project = self.project.clone();
        let http_client = cx.http_client();
        let filter = self.filter;
        let reviewing = self.reviewing;
        let task = cx.spawn(async move |this, cx| {
            let result = load_pull_requests(project, filter, reviewing, http_client, cx).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(LoadOutcome::Loaded {
                        provider,
                        remote,
                        prs,
                    }) => {
                        this.host_context = Some((provider, remote));
                        this.state = LoadState::Loaded(prs);
                        this.start_review_enrichment(cx);
                    }
                    Ok(LoadOutcome::NoHost(reason)) => {
                        this.host_context = None;
                        this.state = LoadState::NoHost(reason);
                    }
                    Err(error) => {
                        this.host_context = None;
                        this.state = match error.downcast_ref::<git::PullRequestAuthError>() {
                            Some(auth_error) => LoadState::AuthExpired {
                                host: auth_error.host.clone(),
                            },
                            None => LoadState::Failed(format!("{error}").into()),
                        };
                    }
                }
                cx.notify();
            })
            .ok();
        });
        self._load_task = Some(task);
    }

    /// After the list loads, fetch each PR's own review verdict in the background
    /// (bounded concurrency) so already-reviewed rows can be tinted. One request
    /// per PR, cached; the task is replaced (cancelled) on the next load.
    fn start_review_enrichment(&mut self, cx: &mut Context<Self>) {
        self.viewer_reviews.clear();
        let LoadState::Loaded(prs) = &self.state else {
            return;
        };
        let numbers: Vec<u32> = prs.iter().map(|summary| summary.number).collect();
        let Some((provider, remote)) = self.host_context.as_ref() else {
            return;
        };
        if numbers.is_empty() {
            return;
        }
        let provider = provider.clone();
        let remote = ParsedGitRemote {
            owner: remote.owner.clone(),
            repo: remote.repo.clone(),
        };
        let http_client = cx.http_client();
        let host = provider.base_url().host_str().map(|host| host.to_string());
        self._enrich_task = Some(cx.spawn(async move |this, cx| {
            let auth = match host.as_deref() {
                Some(host) => git::git_host_credentials::auth_for_host(cx, host)
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            // Process in small chunks so a large list doesn't fire dozens of
            // simultaneous requests at the host.
            for chunk in numbers.chunks(5) {
                let results = futures::future::join_all(chunk.iter().map(|&number| {
                    let provider = provider.clone();
                    let remote = ParsedGitRemote {
                        owner: remote.owner.clone(),
                        repo: remote.repo.clone(),
                    };
                    let auth = auth.clone();
                    let http_client = http_client.clone();
                    async move {
                        let verdict = provider
                            .pull_request_review_state(&remote, number, auth, http_client)
                            .await
                            .ok()
                            .flatten();
                        (number, verdict)
                    }
                }))
                .await;
                let alive = this
                    .update(cx, |this, cx| {
                        for (number, verdict) in results {
                            this.viewer_reviews.insert(number, verdict);
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        }));
    }

    /// Whether the connected account has already responded to this PR's review
    /// with a verdict. Comment-only reviews and not-yet-reviewed PRs are not a
    /// response, so they still count as "awaiting my review". Returns false while
    /// a PR's verdict is still being fetched (it stays visible until known).
    fn has_responded_to_review(&self, number: u32) -> bool {
        matches!(
            self.viewer_reviews.get(&number),
            Some(Some(
                PullRequestReviewVerdict::Approve | PullRequestReviewVerdict::RequestChanges
            ))
        )
    }

    fn open_pull_request(
        &mut self,
        summary: PullRequestSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((provider, remote)) = self.host_context.as_ref() else {
            // Falling back to the host URL keeps the click useful even if the
            // host has been cleared mid-render.
            cx.open_url(summary.url.as_str());
            return;
        };
        let provider = provider.clone();
        let remote = ParsedGitRemote {
            owner: remote.owner.clone(),
            repo: remote.repo.clone(),
        };
        let number = summary.number;
        self.selected_pr = Some(number);
        cx.notify();
        let workspace = self.workspace.clone();
        workspace
            .update(cx, |workspace, cx| {
                let view = cx.new(|cx| {
                    PullRequestView::new(provider, remote, number, workspace.weak_handle(), cx)
                });
                workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
            })
            .ok();
    }

    fn render_header(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = match &self.state {
            LoadState::Loaded(prs) => prs.len(),
            _ => 0,
        };
        let loading = matches!(self.state, LoadState::Loading);

        h_flex()
            .h(rems(2.))
            .px_2()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .justify_between()
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::PullRequest)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new("Pull Requests").size(LabelSize::Small))
                    .child(
                        Label::new(format!("({count})"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(self.render_filter_picker(cx))
                    .child(
                        IconButton::new(
                            "pr-panel-refresh",
                            if loading {
                                IconName::ArrowCircle
                            } else {
                                IconName::RotateCw
                            },
                        )
                        .icon_size(IconSize::Small)
                        .disabled(loading)
                        .tooltip(Tooltip::text("Refresh pull requests"))
                        .on_click(cx.listener(|this, _, _window, cx| {
                            this.kick_off_refresh(cx);
                        })),
                    ),
            )
    }

    fn render_filter_picker(&self, cx: &Context<Self>) -> impl IntoElement {
        let current = self.filter;
        let reviewing = self.reviewing;
        let weak_self = cx.entity().downgrade();
        let trigger_label: SharedString = if reviewing {
            format!("{}, my reviews", current.label()).into()
        } else {
            current.label().into()
        };
        PopoverMenu::new("pr-panel-filter")
            .trigger(
                Button::new("pr-panel-filter-trigger", trigger_label)
                    .label_size(LabelSize::Small)
                    .end_icon(
                        Icon::new(IconName::ChevronDown)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .menu(move |window, cx| {
                let weak_self = weak_self.clone();
                Some(ContextMenu::build(window, cx, move |menu, _window, _cx| {
                    let mut menu = menu;
                    for filter in [
                        StateFilter::Open,
                        StateFilter::Closed,
                        StateFilter::Merged,
                        StateFilter::All,
                    ] {
                        let is_current = filter == current;
                        let weak_self = weak_self.clone();
                        menu = menu.toggleable_entry(
                            filter.label(),
                            is_current,
                            ui::IconPosition::End,
                            None,
                            move |_window, cx| {
                                weak_self
                                    .update(cx, |this, cx| {
                                        if this.filter != filter {
                                            this.filter = filter;
                                            this.kick_off_refresh(cx);
                                        }
                                    })
                                    .ok();
                            },
                        );
                    }
                    // Reviewer scope is an independent toggle layered on top of the
                    // state selection above, so it sits below a separator rather
                    // than in the mutually-exclusive state group.
                    menu = menu.separator().toggleable_entry(
                        "Awaiting my review",
                        reviewing,
                        ui::IconPosition::End,
                        None,
                        move |_window, cx| {
                            weak_self
                                .update(cx, |this, cx| {
                                    this.reviewing = !this.reviewing;
                                    this.kick_off_refresh(cx);
                                })
                                .ok();
                        },
                    );
                    menu
                }))
            })
    }

    fn render_row(
        &self,
        ix: usize,
        summary: PullRequestSummary,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let title = summary.title.clone();
        let number = summary.number;
        let author = summary.author_login.clone();
        let branch = summary.source_branch.clone();
        let is_draft = summary.is_draft;
        let state_color = match summary.state {
            PullRequestState::Open if !is_draft => Color::Success,
            PullRequestState::Open => Color::Muted,
            PullRequestState::Merged => Color::Accent,
            PullRequestState::Closed => Color::Error,
        };

        let is_selected = self.selected_pr == Some(number);

        h_flex()
            .id(("pr-row", ix))
            .h(rems(ROW_HEIGHT_REMS))
            .px_2()
            .gap_2()
            .border_l_2()
            .border_color(if is_selected {
                cx.theme().colors().border_focused
            } else {
                gpui::transparent_black()
            })
            // Tint rows the connected account has already reviewed (filled in
            // lazily by `start_review_enrichment`). Selection/hover override it.
            .when_some(
                self.viewer_reviews.get(&number).copied().flatten(),
                |this, verdict| match verdict {
                    PullRequestReviewVerdict::Approve => {
                        this.bg(Color::Success.color(cx).opacity(0.12))
                    }
                    PullRequestReviewVerdict::RequestChanges => {
                        this.bg(Color::Error.color(cx).opacity(0.12))
                    }
                    PullRequestReviewVerdict::Comment => this,
                },
            )
            .when(is_selected, |this| {
                this.bg(cx.theme().colors().element_selected)
            })
            .hover(|this| this.bg(cx.theme().colors().element_hover))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    this.open_pull_request(summary.clone(), window, cx);
                }),
            )
            .child(
                Icon::new(IconName::PullRequest)
                    .size(IconSize::Small)
                    .color(state_color),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .flex_1()
                    .gap_0p5()
                    .child(
                        Label::new(format!("#{number} {title}"))
                            .size(LabelSize::Small)
                            .truncate(),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Label::new(author)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(branch)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                            .when(is_draft, |this| {
                                this.child(
                                    Label::new("draft")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                            }),
                    ),
            )
    }
}

enum LoadOutcome {
    Loaded {
        provider: Arc<dyn GitHostingProvider + Send + Sync>,
        remote: ParsedGitRemote,
        prs: Vec<PullRequestSummary>,
    },
    NoHost(SharedString),
}

enum CandidateResolution {
    Ready {
        candidates: Vec<String>,
        registry: Arc<git::GitHostingProviderRegistry>,
    },
    NoActiveRepo,
    NoRemote,
}

async fn load_pull_requests(
    project: Entity<Project>,
    filter: StateFilter,
    reviewing: bool,
    http_client: Arc<dyn HttpClient>,
    cx: &mut gpui::AsyncApp,
) -> Result<LoadOutcome> {
    // Collect candidate remotes (origin first, then upstream when set). Forks
    // typically have `origin = your fork` (often no PRs) and `upstream = the
    // canonical repo` (where the PRs live), so we list against each in turn
    // and return the first non-empty result.
    let resolution: CandidateResolution = cx.update(|cx| {
        let git_store = project.read(cx).git_store().clone();
        let Some(active) = git_store.read(cx).active_repository() else {
            return CandidateResolution::NoActiveRepo;
        };
        let snapshot = active.read(cx).snapshot();
        let mut candidates: Vec<String> = Vec::new();
        if let Some(origin) = snapshot.remote_origin_url.clone() {
            candidates.push(origin);
        }
        if let Some(upstream) = snapshot.remote_upstream_url
            && !candidates.contains(&upstream)
        {
            candidates.push(upstream);
        }
        if candidates.is_empty() {
            return CandidateResolution::NoRemote;
        }
        CandidateResolution::Ready {
            candidates,
            registry: GitHostingProviderRegistry::global(cx),
        }
    });

    let (candidates, registry) = match resolution {
        CandidateResolution::Ready {
            candidates,
            registry,
        } => (candidates, registry),
        CandidateResolution::NoActiveRepo => {
            return Ok(LoadOutcome::NoHost(
                "No active repository in this workspace.".into(),
            ));
        }
        CandidateResolution::NoRemote => {
            return Ok(LoadOutcome::NoHost(
                "Active repository has no origin/upstream remote.".into(),
            ));
        }
    };

    let mut chosen: Option<(Arc<dyn GitHostingProvider + Send + Sync>, ParsedGitRemote)> = None;
    let mut last_summaries: Vec<PullRequestSummary> = Vec::new();
    let mut connect_hint: Option<SharedString> = None;

    for remote_url in candidates {
        let Some((provider, parsed)) = parse_git_remote_url(registry.clone(), &remote_url) else {
            continue;
        };
        // Resolve the credential for THIS repo's host only. We never fall back to
        // another host's token, so a Bitbucket repository without a Bitbucket
        // credential surfaces a connect prompt instead of an empty GitHub result.
        let host = provider.base_url().host_str().map(|host| host.to_string());
        let auth = match host.as_deref() {
            Some(host) => git::git_host_credentials::auth_for_host(cx, host)
                .await
                .ok()
                .flatten(),
            None => None,
        };
        if auth.is_none() {
            if connect_hint.is_none() {
                connect_hint =
                    Some(format!("Connect {} to view pull requests.", provider.name()).into());
            }
            continue;
        }
        let filter = PullRequestListFilter {
            states: filter.states(),
            author: None,
            reviewer_is_me: reviewing,
            limit: Some(50),
        };
        let remote_for_call = ParsedGitRemote {
            owner: parsed.owner.clone(),
            repo: parsed.repo.clone(),
        };
        let summaries = provider
            .list_pull_requests(&remote_for_call, filter, auth, http_client.clone())
            .await
            .with_context(|| {
                format!("listing pull requests for {}/{}", parsed.owner, parsed.repo)
            })?;
        let got_any = !summaries.is_empty();
        chosen = Some((provider, parsed));
        last_summaries = summaries;
        if got_any {
            break;
        }
    }

    let Some((provider, remote)) = chosen else {
        if let Some(hint) = connect_hint {
            return Ok(LoadOutcome::NoHost(hint));
        }
        return Ok(LoadOutcome::NoHost(
            "Remote URL does not match any known hosting provider.".into(),
        ));
    };

    Ok(LoadOutcome::Loaded {
        provider,
        remote,
        prs: last_summaries,
    })
}

impl Focusable for PullRequestPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for PullRequestPanel {}

impl Render for PullRequestPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel_bg = cx.theme().colors().panel_background;
        let header = self.render_header(window, cx).into_any_element();

        let body = match self.state.clone() {
            LoadState::Idle | LoadState::Loading => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(
                    Label::new("Loading…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            LoadState::Loaded(rows) => {
                // In "awaiting my review" mode, hide PRs the connected account has
                // already responded to (approved or requested changes). Comment-only
                // reviews and not-yet-reviewed PRs stay visible. GitHub already
                // excludes responded PRs via its `requested_reviewers` query; this
                // makes Bitbucket (which keeps you as a reviewer regardless) match.
                let rows: Vec<PullRequestSummary> = if self.reviewing {
                    rows.into_iter()
                        .filter(|summary| !self.has_responded_to_review(summary.number))
                        .collect()
                } else {
                    rows
                };
                if rows.is_empty() {
                    let queried = self
                        .host_context
                        .as_ref()
                        .map(|(_, remote)| format!("{}/{}", remote.owner, remote.repo));
                    let empty_message = if self.reviewing {
                        "No pull requests awaiting your review"
                    } else {
                        "No open pull requests"
                    };
                    v_flex()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .gap_1()
                        .child(
                            Label::new(empty_message)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .when_some(queried, |this, q| {
                            this.child(
                                Label::new(format!("(queried {q})"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                        })
                        .into_any_element()
                } else {
                    let count = rows.len();
                    uniform_list(
                        "pull-request-panel-rows",
                        count,
                        cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                            range
                                .filter_map(|ix| {
                                    let summary = rows.get(ix)?.clone();
                                    Some(this.render_row(ix, summary, cx).into_any_element())
                                })
                                .collect()
                        }),
                    )
                    .size_full()
                    .track_scroll(&self.scroll_handle)
                    .into_any_element()
                }
            }
            LoadState::NoHost(reason) => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_1()
                .child(
                    Label::new("No connected host")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(reason)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element(),
            LoadState::Failed(reason) => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_1()
                .child(
                    Label::new("Failed to load pull requests")
                        .size(LabelSize::Small)
                        .color(Color::Error),
                )
                .child(
                    Label::new(reason)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element(),
            LoadState::AuthExpired { host } => {
                let host_for_action = host.to_string();
                let display = git::git_host_credentials::GitHostKind::from_host(&host)
                    .map(|kind| kind.display_name())
                    .unwrap_or("the git host");
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(
                        Label::new("Connection expired")
                            .size(LabelSize::Small)
                            .color(Color::Error),
                    )
                    .child(
                        Label::new(format!(
                            "Your {display} sign-in is no longer valid. Reconnect to continue."
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(
                        Button::new("pull-request-panel-reconnect", "Reconnect").on_click(
                            cx.listener(move |_, _, window, cx| {
                                window.dispatch_action(
                                    Box::new(zed_actions::ConnectGitHost {
                                        host: host_for_action.clone(),
                                    }),
                                    cx,
                                );
                            }),
                        ),
                    )
                    .into_any_element()
            }
        };

        v_flex()
            .key_context("PullRequestPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(panel_bg)
            .child(header)
            .child(body)
    }
}

impl Panel for PullRequestPanel {
    fn persistent_name() -> &'static str {
        PR_PANEL_KEY
    }

    fn panel_key() -> &'static str {
        PR_PANEL_KEY
    }

    fn position(&self, _: &Window, cx: &App) -> DockPosition {
        PullRequestPanelSettings::get_global(cx).dock
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, cx: &App) -> Pixels {
        PullRequestPanelSettings::get_global(cx).default_width
    }

    fn icon(&self, _: &Window, cx: &App) -> Option<ui::IconName> {
        PullRequestPanelSettings::get_global(cx)
            .button
            .then_some(ui::IconName::PullRequest)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Pull Requests")
    }

    fn icon_label(&self, _: &Window, _cx: &App) -> Option<String> {
        match &self.state {
            LoadState::Loaded(prs) if !prs.is_empty() => Some(prs.len().to_string()),
            _ => None,
        }
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn starts_open(&self, _: &Window, _: &App) -> bool {
        false
    }

    fn activation_priority(&self) -> u32 {
        // Must be unique across all registered panels (dock.rs panics in debug
        // builds otherwise). 5 collides with CollabPanel; 4 is a free slot.
        4
    }
}
