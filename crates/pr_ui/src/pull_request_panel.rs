use crate::pull_request_panel_settings::PullRequestPanelSettings;
use crate::pull_request_view::PullRequestView;
use anyhow::{Context as _, Result};
use fs::Fs;
use git::{
    GitHostAuth, GitHostingProvider, GitHostingProviderRegistry, ParsedGitRemote,
    PullRequestListFilter, PullRequestReviewVerdict, PullRequestReviewer, PullRequestState,
    PullRequestSummary, parse_git_remote_url,
};
use gpui::http_client::HttpClient;
use gpui::{
    Action, AppContext as _, AsyncWindowContext, ClipboardItem, Entity, EventEmitter, FocusHandle,
    Focusable, ScrollStrategy, SharedString, Subscription, Task, UniformListScrollHandle,
    WeakEntity, actions, uniform_list,
};
use project::{
    Project,
    git_store::{GitStore, GitStoreEvent},
};
use settings::Settings;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use ui::{ContextMenu, PopoverMenu, Tooltip, prelude::*, right_click_menu};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    pull_request_panel,
    [
        /// Toggles focus on the pull request panel.
        ToggleFocus,
        /// Reloads the pull request list from the host.
        Refresh,
        /// Opens the selected pull request on the hosting provider's website.
        OpenSelectedInBrowser,
        /// Checks out the selected pull request's source branch locally.
        CheckoutSelectedBranch,
        /// Opens a dialog for creating a new pull request from the current branch.
        CreatePullRequest,
    ]
);

const PR_PANEL_KEY: &str = "PullRequestPanel";
const ROW_HEIGHT_REMS: f32 = 2.4;

/// How many pull requests one page of results holds. The panel loads a page at a
/// time and appends more on demand, so a busy repository is paged through rather
/// than silently truncated at a fixed cap.
const PAGE_SIZE: u32 = 50;

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
        workspace.toggle_panel_focus::<PullRequestPanel>(window, cx);
    });
}

#[derive(Clone)]
enum LoadState {
    Idle,
    Loading,
    Loaded(LoadedPullRequests),
    NoHost(SharedString),
    Failed(FailureMessage),
    /// A pull-request call returned HTTP 401; the stored credential for `host`
    /// is expired or invalid, so the user is offered a targeted reconnect.
    AuthExpired {
        host: SharedString,
    },
}

/// A load failure split into a one-line summary the user can act on and the
/// underlying error text, which is kept behind a disclosure rather than rendered
/// as the primary message. Raw `anyhow` chains are useful for a bug report and
/// actively unhelpful as headline copy.
#[derive(Clone)]
struct FailureMessage {
    summary: SharedString,
    detail: SharedString,
}

impl FailureMessage {
    fn from_error(error: &anyhow::Error) -> FailureMessage {
        let detail: SharedString = format!("{error:#}").into();
        let lowercase = detail.to_lowercase();
        // Map the failures users actually hit onto copy that says what to do.
        // Everything else keeps a neutral summary with the detail available
        // underneath.
        let summary: SharedString = if lowercase.contains("rate limit") {
            "The host's API rate limit was reached. Try again shortly.".into()
        } else if lowercase.contains("403") || lowercase.contains("forbidden") {
            "Your account cannot read pull requests in this repository.".into()
        } else if lowercase.contains("404") || lowercase.contains("not found") {
            "The repository was not found on the host. Check the remote URL.".into()
        } else if lowercase.contains("dns")
            || lowercase.contains("connect")
            || lowercase.contains("timed out")
            || lowercase.contains("timeout")
        {
            "Could not reach the host. Check your network connection.".into()
        } else {
            "Could not load pull requests from the host.".into()
        };
        FailureMessage { summary, detail }
    }
}

/// The two partitions the panel renders. `authored` holds PRs opened by the
/// connected account (rendered in a "Created by you" section at the bottom); `others`
/// is the rest of the list with the authored PRs removed so no PR appears
/// twice. `authored` is only populated when an account is connected and the
/// review-requested filter is off.
#[derive(Clone, Default)]
struct LoadedPullRequests {
    authored: Vec<PullRequestSummary>,
    others: Vec<PullRequestSummary>,
    /// True when the host returned a full page for `others`, so there may be
    /// more results behind it and a "Load more" row is worth offering.
    may_have_more: bool,
}

impl LoadedPullRequests {
    fn total(&self) -> usize {
        self.authored.len() + self.others.len()
    }

    fn is_empty(&self) -> bool {
        self.authored.is_empty() && self.others.is_empty()
    }

    /// Every PR number across both partitions, used to drive reviewer
    /// enrichment without fetching the same PR twice.
    fn numbers(&self) -> Vec<u32> {
        let mut seen = HashSet::new();
        self.authored
            .iter()
            .chain(self.others.iter())
            .map(|summary| summary.number)
            .filter(|number| seen.insert(*number))
            .collect()
    }
}

/// One rendered entry in the flattened PR list. Section headers and PR rows
/// share the list so the whole panel scrolls as a single uniform list; headers
/// occupy a full row so every entry keeps the uniform height the list requires.
#[derive(Clone)]
enum PanelRow {
    Header(SharedString),
    PullRequest(PullRequestSummary),
    /// Trailing row that fetches the next page when activated.
    LoadMore,
}

impl PanelRow {
    /// Whether the keyboard cursor can land on this row. Headers are labels and
    /// are skipped when moving through the list.
    fn is_selectable(&self) -> bool {
        !matches!(self, PanelRow::Header(_))
    }

    fn pull_request(&self) -> Option<&PullRequestSummary> {
        match self {
            PanelRow::PullRequest(summary) => Some(summary),
            _ => None,
        }
    }
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

/// How the list is ordered. The host returns rows in its own order, which is
/// only ever "recently updated"; sorting client-side keeps every option cheap
/// because the whole loaded page is already in memory.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SortOrder {
    RecentlyUpdated,
    Newest,
    Oldest,
    Title,
}

impl SortOrder {
    const ALL: [SortOrder; 4] = [
        SortOrder::RecentlyUpdated,
        SortOrder::Newest,
        SortOrder::Oldest,
        SortOrder::Title,
    ];

    fn label(&self) -> &'static str {
        match self {
            SortOrder::RecentlyUpdated => "Recently updated",
            SortOrder::Newest => "Newest",
            SortOrder::Oldest => "Oldest",
            SortOrder::Title => "Title",
        }
    }

    fn apply(&self, summaries: &mut [PullRequestSummary]) {
        match self {
            // The host already returns updated-descending; keep that order so
            // the default costs nothing and matches the host's own list.
            SortOrder::RecentlyUpdated => {}
            SortOrder::Newest => summaries.sort_by_key(|summary| std::cmp::Reverse(summary.number)),
            SortOrder::Oldest => summaries.sort_by_key(|summary| summary.number),
            SortOrder::Title => {
                summaries.sort_by_key(|summary| summary.title.to_lowercase());
            }
        }
    }
}

pub struct PullRequestPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    state: LoadState,
    host_context: Option<(Arc<dyn GitHostingProvider + Send + Sync>, ParsedGitRemote)>,
    scroll_handle: UniformListScrollHandle,
    filter: StateFilter,
    sort: SortOrder,
    /// When set, restrict the list to PRs the connected account is a requested
    /// reviewer of. Combines with `filter` (the state selection).
    reviewing: bool,
    /// The flattened list the panel renders, rebuilt whenever the loaded set
    /// changes. Held in state rather than computed in `render` so the keyboard
    /// cursor has a stable set of indices to move through.
    rows: Vec<PanelRow>,
    /// Index into `rows` of the keyboard cursor.
    selected_index: Option<usize>,
    /// PR number most recently opened from this panel; marked in the list so the
    /// row matching the visible tab stays identifiable after the cursor moves.
    opened_pr: Option<u32>,
    /// Highest page fetched so far. "Load more" asks for the next one.
    loaded_pages: u32,
    loading_more: bool,
    _load_task: Option<Task<()>>,
    /// Cached reviewer lists, keyed by PR number and validated against the
    /// `updated_at` the list reported. Surviving a refresh is the point: without
    /// it every reload re-fetches one request per PR, which is the panel's
    /// heaviest source of API traffic.
    reviewers: HashMap<u32, (SharedString, Vec<PullRequestReviewer>)>,
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
        let fs = project.read(cx).fs().clone();
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
                fs,
                focus_handle,
                state: LoadState::Idle,
                host_context: None,
                scroll_handle: UniformListScrollHandle::default(),
                filter: StateFilter::Open,
                sort: SortOrder::RecentlyUpdated,
                reviewing: false,
                rows: Vec::new(),
                selected_index: None,
                opened_pr: None,
                loaded_pages: 0,
                loading_more: false,
                _load_task: None,
                reviewers: HashMap::new(),
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
        self.loaded_pages = 0;
        self.loading_more = false;
        self.rows.clear();
        self.selected_index = None;
        cx.notify();
        let project = self.project.clone();
        let http_client = cx.http_client();
        let filter = self.filter;
        let reviewing = self.reviewing;
        let task = cx.spawn(async move |this, cx| {
            let result = load_pull_requests(project, filter, reviewing, 1, http_client, cx).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(LoadOutcome::Loaded {
                        provider,
                        remote,
                        loaded,
                    }) => {
                        this.host_context = Some((provider, remote));
                        this.loaded_pages = 1;
                        this.state = LoadState::Loaded(loaded);
                        this.rebuild_rows();
                        this.start_review_enrichment(cx);
                    }
                    Ok(LoadOutcome::NoHost(reason)) => {
                        this.host_context = None;
                        this.state = LoadState::NoHost(reason);
                        this.rebuild_rows();
                    }
                    Err(error) => {
                        this.host_context = None;
                        this.state = match error.downcast_ref::<git::PullRequestAuthError>() {
                            Some(auth_error) => LoadState::AuthExpired {
                                host: auth_error.host.clone(),
                            },
                            None => LoadState::Failed(FailureMessage::from_error(&error)),
                        };
                        this.rebuild_rows();
                    }
                }
                cx.notify();
            })
            .ok();
        });
        self._load_task = Some(task);
    }

    /// Fetches the page after the last one loaded and appends it. Only the
    /// unauthored partition grows: the "Created by you" section is a separate,
    /// already-complete query.
    fn load_more(&mut self, cx: &mut Context<Self>) {
        if self.loading_more || !matches!(self.state, LoadState::Loaded(_)) {
            return;
        }
        self.loading_more = true;
        cx.notify();
        let project = self.project.clone();
        let http_client = cx.http_client();
        let filter = self.filter;
        let reviewing = self.reviewing;
        let next_page = self.loaded_pages + 1;
        let task = cx.spawn(async move |this, cx| {
            let result =
                load_pull_requests(project, filter, reviewing, next_page, http_client, cx).await;
            this.update(cx, |this, cx| {
                this.loading_more = false;
                match result {
                    Ok(LoadOutcome::Loaded { loaded, .. }) => {
                        this.loaded_pages = next_page;
                        if let LoadState::Loaded(existing) = &mut this.state {
                            // The authored partition is re-queried whole on every
                            // page, so keep the copy already displayed and only
                            // extend the paged partition. Filter by number to
                            // stay idempotent if the host repeats a row across
                            // page boundaries.
                            let seen: HashSet<u32> = existing
                                .authored
                                .iter()
                                .chain(existing.others.iter())
                                .map(|summary| summary.number)
                                .collect();
                            existing.others.extend(
                                loaded
                                    .others
                                    .into_iter()
                                    .filter(|summary| !seen.contains(&summary.number)),
                            );
                            existing.may_have_more = loaded.may_have_more;
                        }
                        this.rebuild_rows();
                        this.start_review_enrichment(cx);
                    }
                    Ok(LoadOutcome::NoHost(_)) => {}
                    Err(error) => {
                        // A failed "load more" keeps the rows already on screen;
                        // replacing them with an error would lose the user's place.
                        this.surface_error("Could not load more pull requests", &error, cx);
                    }
                }
                cx.notify();
            })
            .ok();
        });
        self._load_task = Some(task);
    }

    /// Rebuilds the flattened row list from the current load state, applying the
    /// active sort. Keeps the keyboard cursor on the same pull request where it
    /// can, so a refresh does not throw away the user's place.
    fn rebuild_rows(&mut self) {
        let previously_selected = self
            .selected_index
            .and_then(|ix| self.rows.get(ix))
            .and_then(|row| row.pull_request())
            .map(|summary| summary.number);

        let mut rows = Vec::new();
        if let LoadState::Loaded(loaded) = &self.state {
            let mut others = loaded.others.clone();
            let mut authored = loaded.authored.clone();
            self.sort.apply(&mut others);
            self.sort.apply(&mut authored);

            if !authored.is_empty() && !others.is_empty() {
                rows.push(PanelRow::Header("Other pull requests".into()));
            }
            rows.extend(others.into_iter().map(PanelRow::PullRequest));
            if loaded.may_have_more {
                rows.push(PanelRow::LoadMore);
            }
            if !authored.is_empty() {
                rows.push(PanelRow::Header("Created by you".into()));
                rows.extend(authored.into_iter().map(PanelRow::PullRequest));
            }
        }
        self.rows = rows;

        self.selected_index = previously_selected
            .and_then(|number| {
                self.rows.iter().position(|row| {
                    row.pull_request()
                        .is_some_and(|summary| summary.number == number)
                })
            })
            .or_else(|| {
                self.selected_index
                    .filter(|_| !self.rows.is_empty())
                    .map(|ix| ix.min(self.rows.len().saturating_sub(1)))
            });
    }

    fn surface_error(&self, action: &str, error: &anyhow::Error, cx: &mut Context<Self>) {
        let message: SharedString = format!("{action}: {error:#}").into();
        self.workspace
            .update(cx, |workspace, cx| {
                let toast = notifications::status_toast::StatusToast::new(
                    message.clone(),
                    cx,
                    |this, _cx| this.dismiss_button(true),
                );
                workspace.toggle_status_toast(toast, cx);
            })
            .ok();
    }

    /// After a load, fetch each PR's reviewers in the background so rows can be
    /// tinted and rolled up. Only PRs whose cached entry is missing or stale
    /// (the host reported a newer `updated_at`) are fetched, so an ordinary
    /// refresh of an unchanged list costs no requests at all.
    fn start_review_enrichment(&mut self, cx: &mut Context<Self>) {
        let LoadState::Loaded(loaded) = &self.state else {
            return;
        };
        let mut freshness: HashMap<u32, SharedString> = HashMap::new();
        for summary in loaded.authored.iter().chain(loaded.others.iter()) {
            freshness.insert(summary.number, summary.updated_at.clone());
        }
        // Drop cache entries for PRs no longer in the list so the map cannot
        // grow without bound as the user pages and refilters.
        self.reviewers
            .retain(|number, _| freshness.contains_key(number));

        let stale: Vec<u32> = loaded
            .numbers()
            .into_iter()
            .filter(|number| match self.reviewers.get(number) {
                Some((cached_at, _)) => freshness
                    .get(number)
                    .is_some_and(|current| current != cached_at),
                None => true,
            })
            .collect();

        let Some((provider, remote)) = self.host_context.as_ref() else {
            return;
        };
        if stale.is_empty() {
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
            for chunk in stale.chunks(5) {
                let results = futures::future::join_all(chunk.iter().map(|&number| {
                    let provider = provider.clone();
                    let remote = ParsedGitRemote {
                        owner: remote.owner.clone(),
                        repo: remote.repo.clone(),
                    };
                    let auth = auth.clone();
                    let http_client = http_client.clone();
                    async move {
                        let reviewers = provider
                            .pull_request_reviewers(&remote, number, auth, http_client)
                            .await
                            .unwrap_or_default();
                        (number, reviewers)
                    }
                }))
                .await;
                let alive = this
                    .update(cx, |this, cx| {
                        for (number, reviewers) in results {
                            let updated_at = freshness
                                .get(&number)
                                .cloned()
                                .unwrap_or_else(|| SharedString::from(""));
                            this.reviewers.insert(number, (updated_at, reviewers));
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

    fn reviewers_for(&self, number: u32) -> Option<&Vec<PullRequestReviewer>> {
        self.reviewers.get(&number).map(|(_, reviewers)| reviewers)
    }

    /// The connected account's own latest verdict on a PR, derived from the
    /// cached reviewer list (the `is_me` entry). `None` while reviewers are
    /// still loading, when the viewer has not reviewed, or when the host does
    /// not report reviewers.
    fn my_verdict(&self, number: u32) -> Option<PullRequestReviewVerdict> {
        self.reviewers_for(number)?
            .iter()
            .find(|reviewer| reviewer.is_me)
            .and_then(|reviewer| reviewer.verdict)
    }
}

/// Keyboard navigation and the per-row actions the context menu exposes.
impl PullRequestPanel {
    /// Moves the cursor by `delta` rows, skipping section headers. Stops at the
    /// ends rather than wrapping, matching the other list panels.
    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            return;
        }
        let start = match self.selected_index {
            Some(index) => index as isize + delta,
            // With no cursor yet, entering from the top selects the first row
            // and entering from the bottom selects the last.
            None if delta > 0 => 0,
            None => self.rows.len() as isize - 1,
        };
        let mut index = start;
        while index >= 0 && (index as usize) < self.rows.len() {
            if self.rows[index as usize].is_selectable() {
                self.select_index(index as usize, cx);
                return;
            }
            index += delta.signum();
        }
    }

    fn select_first(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = self.rows.iter().position(PanelRow::is_selectable) {
            self.select_index(index, cx);
        }
    }

    fn select_last(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = self.rows.iter().rposition(PanelRow::is_selectable) {
            self.select_index(index, cx);
        }
    }

    fn select_index(&mut self, index: usize, cx: &mut Context<Self>) {
        self.selected_index = Some(index);
        self.scroll_handle
            .scroll_to_item(index, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn selected_summary(&self) -> Option<&PullRequestSummary> {
        self.rows.get(self.selected_index?)?.pull_request()
    }

    fn confirm_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.rows.get(self.selected_index.unwrap_or(usize::MAX)) {
            Some(PanelRow::PullRequest(summary)) => {
                let summary = summary.clone();
                self.open_pull_request(summary, window, cx);
            }
            Some(PanelRow::LoadMore) => self.load_more(cx),
            _ => {}
        }
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
        self.opened_pr = Some(number);
        if let Some(index) = self.rows.iter().position(|row| {
            row.pull_request()
                .is_some_and(|candidate| candidate.number == number)
        }) {
            self.selected_index = Some(index);
        }
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

    /// Checks out the pull request's source branch in the active repository.
    ///
    /// Runs `git switch`, which creates a local tracking branch when exactly one
    /// remote has the branch. A branch that has never been fetched fails here,
    /// and the host's error is surfaced verbatim because it names the fix.
    fn checkout_branch(&mut self, branch: SharedString, cx: &mut Context<Self>) {
        let Some(repository) = self
            .project
            .read(cx)
            .git_store()
            .read(cx)
            .active_repository()
        else {
            return;
        };
        let receiver = repository.update(cx, |repository, _| {
            repository.change_branch(branch.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = receiver.await;
            this.update(cx, |this, cx| match result {
                Ok(Ok(())) => {
                    let message: SharedString = format!("Checked out {branch}").into();
                    this.workspace
                        .update(cx, |workspace, cx| {
                            let toast = notifications::status_toast::StatusToast::new(
                                message.clone(),
                                cx,
                                |this, _cx| this.dismiss_button(true),
                            );
                            workspace.toggle_status_toast(toast, cx);
                        })
                        .ok();
                }
                Ok(Err(error)) => {
                    this.surface_error(&format!("Could not check out {branch}"), &error, cx)
                }
                Err(error) => this.surface_error(
                    &format!("Could not check out {branch}"),
                    &anyhow::anyhow!(error),
                    cx,
                ),
            })
            .ok();
        })
        .detach();
    }

    fn on_select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, cx);
    }

    fn on_select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_selection(-1, cx);
    }

    fn on_select_first(&mut self, _: &menu::SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.select_first(cx);
    }

    fn on_select_last(&mut self, _: &menu::SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        self.select_last(cx);
    }

    fn on_confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_selection(window, cx);
    }

    /// Secondary confirm (cmd-enter) opens on the host's website instead of in a
    /// Lathe tab, mirroring cmd-click on a row.
    fn on_secondary_confirm(
        &mut self,
        _: &menu::SecondaryConfirm,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(summary) = self.selected_summary() {
            cx.open_url(summary.url.as_str());
        }
    }

    fn on_refresh(&mut self, _: &Refresh, _: &mut Window, cx: &mut Context<Self>) {
        self.kick_off_refresh(cx);
    }

    fn on_open_selected_in_browser(
        &mut self,
        _: &OpenSelectedInBrowser,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(summary) = self.selected_summary() {
            cx.open_url(summary.url.as_str());
        }
    }

    fn on_checkout_selected_branch(
        &mut self,
        _: &CheckoutSelectedBranch,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(branch) = self
            .selected_summary()
            .map(|summary| summary.source_branch.clone())
        {
            self.checkout_branch(branch, cx);
        }
    }

}

/// The per-row context menu. Every entry works on the row that was clicked
/// rather than the keyboard cursor, so right-clicking a row the cursor is not on
/// does what it looks like it does.
fn row_context_menu(
    summary: &PullRequestSummary,
    panel: WeakEntity<PullRequestPanel>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    let url = summary.url.to_string();
    let branch = summary.source_branch.clone();
    let number = summary.number;
    let title = summary.title.clone();
    ContextMenu::build(window, cx, move |menu, _window, _cx| {
        let open_url = url.clone();
        let copy_url = url.clone();
        let copy_branch = branch.clone();
        let checkout = branch.clone();
        let copy_title = format!("#{number} {title}");
        let panel_for_checkout = panel;
        menu.entry("Open on Host Website", None, move |_window, cx| {
            cx.open_url(&open_url);
        })
        .separator()
        .entry("Copy Link", None, move |_window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(copy_url.clone()));
        })
        .entry("Copy Title", None, move |_window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(copy_title.clone()));
        })
        .entry("Copy Branch Name", None, move |_window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(copy_branch.to_string()));
        })
        .separator()
        .entry("Check Out Branch", None, move |_window, cx| {
            panel_for_checkout
                .update(cx, |panel, cx| {
                    panel.checkout_branch(checkout.clone(), cx);
                })
                .ok();
        })
    })
}

/// Rendering.
impl PullRequestPanel {
    fn render_header(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = match &self.state {
            LoadState::Loaded(loaded) => loaded.total(),
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
                        IconButton::new("pr-panel-create", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(move |_window, cx| {
                                Tooltip::for_action(
                                    "New Pull Request",
                                    &CreatePullRequest,
                                    cx,
                                )
                            })
                            .on_click(cx.listener(|_, _, _window, cx| {
                                cx.dispatch_action(&CreatePullRequest);
                            })),
                    )
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
                        .tooltip(move |_window, cx| {
                            Tooltip::for_action("Refresh Pull Requests", &Refresh, cx)
                        })
                        .on_click(cx.listener(|this, _, _window, cx| {
                            this.kick_off_refresh(cx);
                        })),
                    ),
            )
    }

    fn render_filter_picker(&self, cx: &Context<Self>) -> impl IntoElement {
        let current = self.filter;
        let sort = self.sort;
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
                    let weak_for_reviewing = weak_self.clone();
                    menu = menu.separator().toggleable_entry(
                        "My open reviews",
                        reviewing,
                        ui::IconPosition::End,
                        None,
                        move |_window, cx| {
                            weak_for_reviewing
                                .update(cx, |this, cx| {
                                    this.reviewing = !this.reviewing;
                                    this.kick_off_refresh(cx);
                                })
                                .ok();
                        },
                    );
                    // Sort is a third independent axis; reordering is client-side
                    // over the already-loaded rows, so it never refetches.
                    menu = menu.separator().header("Sort by");
                    for order in SortOrder::ALL {
                        let weak_self = weak_self.clone();
                        menu = menu.toggleable_entry(
                            order.label(),
                            order == sort,
                            ui::IconPosition::End,
                            None,
                            move |_window, cx| {
                                weak_self
                                    .update(cx, |this, cx| {
                                        if this.sort != order {
                                            this.sort = order;
                                            this.rebuild_rows();
                                            cx.notify();
                                        }
                                    })
                                    .ok();
                            },
                        );
                    }
                    menu
                }))
            })
    }

    fn render_row(
        &self,
        ix: usize,
        summary: PullRequestSummary,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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

        let is_open_in_tab = self.opened_pr == Some(number);
        let is_selected = self.selected_index == Some(ix);
        let summary_for_menu = summary.clone();
        let summary_for_click = summary;

        let row = h_flex()
            .id(("pr-row", ix))
            .h(rems(ROW_HEIGHT_REMS))
            .w_full()
            .px_2()
            .gap_2()
            .border_l_2()
            .border_color(if is_open_in_tab {
                cx.theme().colors().border_focused
            } else {
                gpui::transparent_black()
            })
            // Tint rows the connected account has already reviewed (filled in
            // lazily by `start_review_enrichment`). Selection/hover override it.
            .when_some(self.my_verdict(number), |this, verdict| match verdict {
                PullRequestReviewVerdict::Approve => {
                    this.bg(Color::Success.color(cx).opacity(0.12))
                }
                PullRequestReviewVerdict::RequestChanges => {
                    this.bg(Color::Error.color(cx).opacity(0.12))
                }
                PullRequestReviewVerdict::Comment => this,
            })
            .when(is_selected, |this| {
                this.bg(cx.theme().colors().element_selected)
            })
            .hover(|this| this.bg(cx.theme().colors().element_hover))
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                // Cmd/ctrl-click and middle-click open on the host's website,
                // matching how links behave everywhere else.
                if event.modifiers().secondary() || event.is_middle_click() {
                    cx.open_url(summary_for_click.url.as_str());
                } else {
                    this.open_pull_request(summary_for_click.clone(), window, cx);
                }
            }))
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
                            })
                            .when(self.reviewing, |this| {
                                let (verdict_label, verdict_color) = match self.my_verdict(number) {
                                    Some(PullRequestReviewVerdict::Approve) => {
                                        ("approved", Color::Success)
                                    }
                                    Some(PullRequestReviewVerdict::RequestChanges) => {
                                        ("changes requested", Color::Error)
                                    }
                                    Some(PullRequestReviewVerdict::Comment) => {
                                        ("commented", Color::Info)
                                    }
                                    None => ("awaiting", Color::Muted),
                                };
                                this.child(
                                    Label::new(verdict_label)
                                        .size(LabelSize::XSmall)
                                        .color(verdict_color),
                                )
                            }),
                    ),
            )
            .when_some(self.render_reviewer_rollup(number, cx), |this, rollup| {
                this.child(rollup)
            });

        let panel = cx.entity().downgrade();
        right_click_menu(("pr-row-menu", ix))
            .trigger(move |_, _, _| row)
            .menu(move |window, cx| {
                row_context_menu(&summary_for_menu, panel.clone(), window, cx)
            })
            .into_any_element()
    }

    /// The trailing row that fetches the next page.
    fn render_load_more(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let is_selected = self.selected_index == Some(ix);
        let loading = self.loading_more;
        h_flex()
            .id(("pr-load-more", ix))
            .h(rems(ROW_HEIGHT_REMS))
            .w_full()
            .px_2()
            .gap_2()
            .justify_center()
            .when(is_selected, |this| {
                this.bg(cx.theme().colors().element_selected)
            })
            .hover(|this| this.bg(cx.theme().colors().element_hover))
            .on_click(cx.listener(|this, _, _window, cx| this.load_more(cx)))
            .child(
                Label::new(if loading { "Loading…" } else { "Load more" })
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    /// A compact reviewer summary for a PR list row: up to `MAX_ROLLUP_DOTS`
    /// verdict-colored dots (approved green, changes-requested red, pending
    /// hollow) plus a "+N" overflow, with a tooltip listing every reviewer.
    /// `None` when reviewers are still loading or the host reports none.
    fn render_reviewer_rollup(&self, number: u32, cx: &Context<Self>) -> Option<AnyElement> {
        const MAX_ROLLUP_DOTS: usize = 3;
        let reviewers = self.reviewers_for(number)?;
        if reviewers.is_empty() {
            return None;
        }
        let overflow = reviewers.len().saturating_sub(MAX_ROLLUP_DOTS);
        let dots = reviewers.iter().take(MAX_ROLLUP_DOTS).map(|reviewer| {
            let (color, filled) = match reviewer.verdict {
                Some(PullRequestReviewVerdict::Approve) => (Color::Success, true),
                Some(PullRequestReviewVerdict::RequestChanges) => (Color::Error, true),
                Some(PullRequestReviewVerdict::Comment) => (Color::Info, true),
                None => (Color::Muted, false),
            };
            let color = color.color(cx);
            let mut dot = div().size(px(8.)).rounded_full();
            dot = if filled {
                dot.bg(color)
            } else {
                dot.border_1().border_color(color)
            };
            dot
        });
        let tooltip: SharedString = reviewers
            .iter()
            .map(|reviewer| {
                let state = match reviewer.verdict {
                    Some(PullRequestReviewVerdict::Approve) => "approved",
                    Some(PullRequestReviewVerdict::RequestChanges) => "changes requested",
                    Some(PullRequestReviewVerdict::Comment) => "commented",
                    None => "pending",
                };
                let you = if reviewer.is_me { " (you)" } else { "" };
                format!("{}{you}: {state}", reviewer.login)
            })
            .collect::<Vec<_>>()
            .join("\n")
            .into();
        Some(
            h_flex()
                .id(("pr-reviewer-rollup", number as usize))
                .flex_none()
                .gap_1()
                .items_center()
                .children(dots)
                .when(overflow > 0, |this| {
                    this.child(
                        Label::new(format!("+{overflow}"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                })
                .tooltip(Tooltip::text(tooltip))
                .into_any_element(),
        )
    }

    /// A centered message with an optional secondary line, used for every state
    /// that has no rows to show.
    fn render_message(
        title: impl Into<SharedString>,
        title_color: Color,
        detail: Option<SharedString>,
    ) -> Div {
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_1()
            .p_4()
            .child(
                Label::new(title.into())
                    .size(LabelSize::Small)
                    .color(title_color),
            )
            .when_some(detail, |this, detail| {
                this.child(
                    Label::new(detail)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .truncate(),
                )
            })
    }
}

/// A section header row inside the flattened PR list. Occupies a full uniform
/// row so it can sit between PR rows without breaking the list's fixed height.
fn render_section_header(label: SharedString) -> impl IntoElement {
    h_flex()
        .h(rems(ROW_HEIGHT_REMS))
        .px_2()
        .items_center()
        .child(
            Label::new(label)
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
}

enum LoadOutcome {
    Loaded {
        provider: Arc<dyn GitHostingProvider + Send + Sync>,
        remote: ParsedGitRemote,
        loaded: LoadedPullRequests,
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
    page: u32,
    http_client: Arc<dyn HttpClient>,
    cx: &mut gpui::AsyncApp,
) -> Result<LoadOutcome> {
    // Collect candidate remotes (origin first, then upstream when set). We query
    // the first remote that resolves to a known host and has a credential, so a
    // fork shows its own `origin` pull requests (often none) rather than falling
    // through to the `upstream` project and surfacing the canonical repo's PRs.
    // `upstream` is only used when `origin` is not a usable hosting remote.
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
                "Open a folder that is a git repository to see its pull requests.".into(),
            ));
        }
        CandidateResolution::NoRemote => {
            return Ok(LoadOutcome::NoHost(
                "This repository has no origin or upstream remote, so there is no host to query."
                    .into(),
            ));
        }
    };

    let mut chosen: Option<(Arc<dyn GitHostingProvider + Send + Sync>, ParsedGitRemote)> = None;
    let mut last_summaries: Vec<PullRequestSummary> = Vec::new();
    // Auth for the chosen candidate, reused for the second "authored by me" call
    // so we do not re-read the keychain.
    let mut chosen_auth: Option<GitHostAuth> = None;
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
                connect_hint = Some(
                    format!(
                        "Connect {} from the account menu to see this repository's pull requests.",
                        provider.name()
                    )
                    .into(),
                );
            }
            continue;
        }
        let list_filter = PullRequestListFilter {
            states: filter.states(),
            author: None,
            reviewer_is_me: reviewing,
            author_is_me: false,
            limit: Some(PAGE_SIZE),
            page: Some(page),
        };
        let remote_for_call = ParsedGitRemote {
            owner: parsed.owner.clone(),
            repo: parsed.repo.clone(),
        };
        let summaries = provider
            .list_pull_requests(
                &remote_for_call,
                list_filter,
                auth.clone(),
                http_client.clone(),
            )
            .await
            .with_context(|| {
                format!("listing pull requests for {}/{}", parsed.owner, parsed.repo)
            })?;
        chosen = Some((provider, parsed));
        last_summaries = summaries;
        chosen_auth = auth;
        // The first queryable remote wins. For a fork this is `origin` (your own
        // repo), so its pull requests are shown even when empty, instead of
        // falling through to `upstream` and listing the canonical repo's PRs.
        break;
    }

    let Some((provider, remote)) = chosen else {
        if let Some(hint) = connect_hint {
            return Ok(LoadOutcome::NoHost(hint));
        }
        return Ok(LoadOutcome::NoHost(
            "This repository's remote does not match any known hosting provider.".into(),
        ));
    };

    // A full page back means the host may have more behind it.
    let may_have_more = last_summaries.len() as u32 >= PAGE_SIZE;

    // With an account connected, also fetch the viewer's own PRs so the panel
    // can surface them in a "Created by you" section. Skipped in review mode,
    // where the list is already scoped to review requests (which exclude your
    // own PRs), and on later pages, where the section is already populated.
    // A failure here (for example a host that cannot resolve the authenticated
    // user) leaves the section empty rather than failing the load.
    let authored = if reviewing || page > 1 {
        Vec::new()
    } else if let Some(auth) = chosen_auth {
        let authored_filter = PullRequestListFilter {
            states: filter.states(),
            author: None,
            reviewer_is_me: false,
            author_is_me: true,
            limit: Some(PAGE_SIZE),
            page: None,
        };
        let remote_for_authored = ParsedGitRemote {
            owner: remote.owner.clone(),
            repo: remote.repo.clone(),
        };
        provider
            .list_pull_requests(
                &remote_for_authored,
                authored_filter,
                Some(auth),
                http_client,
            )
            .await
            .with_context(|| {
                format!(
                    "listing authored pull requests for {}/{}",
                    remote.owner, remote.repo
                )
            })
            .log_err()
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let authored_numbers: HashSet<u32> = authored.iter().map(|summary| summary.number).collect();
    let others: Vec<PullRequestSummary> = last_summaries
        .into_iter()
        .filter(|summary| !authored_numbers.contains(&summary.number))
        .collect();

    Ok(LoadOutcome::Loaded {
        provider,
        remote,
        loaded: LoadedPullRequests {
            authored,
            others,
            may_have_more,
        },
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
            LoadState::Idle | LoadState::Loading => {
                Self::render_message("Loading pull requests…", Color::Muted, None).into_any_element()
            }
            LoadState::Loaded(loaded) => {
                if loaded.is_empty() {
                    let queried = self
                        .host_context
                        .as_ref()
                        .map(|(_, remote)| format!("{}/{}", remote.owner, remote.repo));
                    let empty_message = if self.reviewing {
                        "No pull requests are waiting on your review"
                    } else {
                        match self.filter {
                            StateFilter::Open => "No open pull requests",
                            StateFilter::Closed => "No closed pull requests",
                            StateFilter::Merged => "No merged pull requests",
                            StateFilter::All => "No pull requests",
                        }
                    };
                    Self::render_message(
                        empty_message,
                        Color::Muted,
                        queried.map(|repo| format!("in {repo}").into()),
                    )
                    .into_any_element()
                } else {
                    let rows = self.rows.clone();
                    uniform_list(
                        "pull-request-panel-rows",
                        rows.len(),
                        cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                            range
                                .filter_map(|ix| match rows.get(ix)? {
                                    PanelRow::Header(label) => Some(
                                        render_section_header(label.clone()).into_any_element(),
                                    ),
                                    PanelRow::PullRequest(summary) => {
                                        Some(this.render_row(ix, summary.clone(), cx))
                                    }
                                    PanelRow::LoadMore => Some(this.render_load_more(ix, cx)),
                                })
                                .collect()
                        }),
                    )
                    .size_full()
                    .track_scroll(&self.scroll_handle)
                    .into_any_element()
                }
            }
            LoadState::NoHost(reason) => {
                Self::render_message("No connected host", Color::Muted, Some(reason))
                    .into_any_element()
            }
            LoadState::Failed(failure) => Self::render_message(
                failure.summary.clone(),
                Color::Error,
                Some(failure.detail),
            )
            .child(
                Button::new("pull-request-panel-retry", "Try Again")
                    .on_click(cx.listener(|this, _, _window, cx| this.kick_off_refresh(cx))),
            )
            .into_any_element(),
            LoadState::AuthExpired { host } => {
                let host_for_action = host.to_string();
                let display = crate::pull_request_view::host_display_name(cx, &host);
                Self::render_message(
                    "Connection expired",
                    Color::Error,
                    Some(
                        format!("Your {display} sign-in is no longer valid. Reconnect to continue.")
                            .into(),
                    ),
                )
                .child(
                    Button::new("pull-request-panel-reconnect", "Reconnect").on_click(
                        cx.listener(move |_, _, _window, cx| {
                            cx.dispatch_action(&zed_actions::ConnectGitHost {
                                host: host_for_action.clone(),
                            });
                        }),
                    ),
                )
                .into_any_element()
            }
        };

        v_flex()
            .key_context("PullRequestPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_select_next))
            .on_action(cx.listener(Self::on_select_previous))
            .on_action(cx.listener(Self::on_select_first))
            .on_action(cx.listener(Self::on_select_last))
            .on_action(cx.listener(Self::on_confirm))
            .on_action(cx.listener(Self::on_secondary_confirm))
            .on_action(cx.listener(Self::on_refresh))
            .on_action(cx.listener(Self::on_open_selected_in_browser))
            .on_action(cx.listener(Self::on_checkout_selected_branch))
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

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            settings.pull_request_panel.get_or_insert_default().dock = Some(position.into());
        });
    }

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
            LoadState::Loaded(loaded) if !loaded.is_empty() => Some(loaded.total().to_string()),
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
