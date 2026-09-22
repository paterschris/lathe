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
    Focusable, ScrollHandle, SharedString, Subscription, Task, WeakEntity, actions,
};
use project::{
    Project, repo_identity_path_if_local,
    git_store::{GitStore, GitStoreEvent, Repository, RepositoryId},
};
use settings::{Settings, SettingsStore};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
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

/// Why a reload is happening, which decides whether it is allowed to disturb
/// what is currently on screen.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RefreshMode {
    /// The user asked for it (opening the panel, the Refresh action, changing a
    /// filter). Clearing the list and showing a spinner or an error is correct
    /// feedback here.
    Interactive,
    /// The auto-refresh timer asked for it. The user did not, so the list must
    /// not flicker, shrink, or be replaced by an error page.
    Background,
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

/// A failure from a background refresh.
///
/// Deliberately separate from `LoadState::Failed` / `LoadState::AuthExpired`:
/// those replace the list, which is the right answer when the user asked for a
/// reload and got nothing. A timer-driven failure instead leaves the previous
/// results on screen and raises this as a banner above them, because the list
/// is still real, it is just no longer known to be current.
#[derive(Clone)]
enum BackgroundFailure {
    /// The stored credential for `host` expired. Recoverable by reconnecting,
    /// and worth saying so loudly: every later refresh will fail the same way
    /// until it is fixed.
    AuthExpired {
        host: SharedString,
    },
    Other(FailureMessage),
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

/// One repository's slice of the panel. Every repository open in the workspace
/// gets a section of its own, each with an independent host, load state, paging
/// cursor and scroll position, so all of them are visible at once instead of the
/// panel following whichever repository happens to be active.
struct RepoSection {
    id: RepositoryId,
    display_name: SharedString,
    repository: Entity<Repository>,
    state: LoadState,
    host_context: Option<(Arc<dyn GitHostingProvider + Send + Sync>, ParsedGitRemote)>,
    /// The flattened list this section renders, rebuilt whenever its loaded set
    /// changes. Held in state rather than computed in `render` so the keyboard
    /// cursor has a stable set of indices to move through.
    rows: Vec<PanelRow>,
    scroll_handle: ScrollHandle,
    /// Highest page fetched so far. "Load more" asks for the next one.
    loaded_pages: u32,
    loading_more: bool,
    /// Cached reviewer lists, keyed by PR number and validated against the
    /// `updated_at` the list reported. Surviving a refresh is the point: without
    /// it every reload re-fetches one request per PR, which is the panel's
    /// heaviest source of API traffic.
    reviewers: HashMap<u32, (SharedString, Vec<PullRequestReviewer>)>,
    /// True until the repository has reported at least one remote. A repository
    /// exists in the git store before its remote URLs are scanned, so the first
    /// resolve can legitimately find nothing; this is what makes the panel retry
    /// on repository updates, and what makes it stop once there is something to
    /// query.
    awaiting_remotes: bool,
    /// The `updated_at` this panel last observed for each PR number. Seeded on
    /// the first successful load and updated on every load after it, so a
    /// background refresh can tell a genuinely changed PR from one that merely
    /// came back in the response again.
    /// Set when a background refresh fails, cleared by the next success. Shown
    /// as a banner over the retained list rather than replacing it.
    background_failure: Option<BackgroundFailure>,
    last_seen_updated_at: HashMap<u32, SharedString>,
    /// PRs whose `updated_at` moved since this panel last saw them, and which
    /// the user has not opened yet. Purely a display hint; it never affects
    /// which PRs are fetched or shown.
    updated_since_seen: HashSet<u32>,
    _load_task: Option<Task<()>>,
    _enrich_task: Option<Task<()>>,
}

impl RepoSection {
    fn new(id: RepositoryId, display_name: SharedString, repository: Entity<Repository>) -> Self {
        Self {
            id,
            display_name,
            repository,
            state: LoadState::Idle,
            host_context: None,
            rows: Vec::new(),
            scroll_handle: ScrollHandle::default(),
            loaded_pages: 0,
            loading_more: false,
            reviewers: HashMap::new(),
            awaiting_remotes: true,
            background_failure: None,
            last_seen_updated_at: HashMap::new(),
            updated_since_seen: HashSet::default(),
            _load_task: None,
            _enrich_task: None,
        }
    }

    fn total(&self) -> usize {
        match &self.state {
            LoadState::Loaded(loaded) => loaded.total(),
            _ => 0,
        }
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

    /// Rebuilds the flattened row list from this section's load state, applying
    /// the panel-wide sort. Returns the pull request the cursor was on before
    /// the rebuild so the caller can restore it.
    /// Folds a freshly loaded set into this section's seen-state, flagging any
    /// PR whose `updated_at` moved since the last observation.
    fn note_loaded(&mut self, loaded: &LoadedPullRequests) {
        note_updated_pull_requests(
            &mut self.last_seen_updated_at,
            &mut self.updated_since_seen,
            loaded,
        );
    }

    fn is_updated_since_seen(&self, number: u32) -> bool {
        self.updated_since_seen.contains(&number)
    }

    /// Drops the updated indicator from every flagged PR the viewer has already
    /// ruled on, using the reviewer lists cached so far.
    fn clear_updated_for_own_verdict(&mut self) {
        clear_updated_for_own_verdict(&mut self.updated_since_seen, &self.reviewers);
    }

    fn rebuild_rows(&mut self, sort: SortOrder) {
        let mut rows = Vec::new();
        if let LoadState::Loaded(loaded) = &self.state {
            let mut others = loaded.others.clone();
            let mut authored = loaded.authored.clone();
            sort.apply(&mut others);
            sort.apply(&mut authored);

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
    }

    /// The remotes worth querying for this repository, origin first and then
    /// upstream when it is set to something different.
    fn remote_candidates(&self, cx: &App) -> Vec<String> {
        let snapshot = self.repository.read(cx).snapshot();
        let mut candidates = Vec::new();
        if let Some(origin) = snapshot.remote_origin_url.clone() {
            candidates.push(origin);
        }
        if let Some(upstream) = snapshot.remote_upstream_url
            && !candidates.contains(&upstream)
        {
            candidates.push(upstream);
        }
        candidates
    }
}

pub struct PullRequestPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    /// One section per workspace repository, ordered by display name.
    sections: Vec<RepoSection>,
    /// Repository sections whose pull request lists are hidden.
    collapsed_sections: HashSet<RepositoryId>,
    filter: StateFilter,
    sort: SortOrder,
    /// When set, restrict the lists to PRs the connected account is a requested
    /// reviewer of. Combines with `filter` (the state selection).
    reviewing: bool,
    /// Keyboard cursor, as an index into `sections` and an index into that
    /// section's `rows`.
    selected: Option<(usize, usize)>,
    /// The pull request most recently opened from this panel; marked in the list
    /// so the row matching the visible tab stays identifiable after the cursor
    /// moves.
    opened_pr: Option<(RepositoryId, u32)>,
    /// Whether the dock is currently showing this panel. Drives the
    /// auto-refresh timer: polling a panel nobody is looking at spends the
    /// host's rate limit for no benefit.
    active: bool,
    _subscriptions: Vec<Subscription>,
    _auto_refresh_task: Option<Task<()>>,
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
            let subscriptions = vec![
                cx.subscribe(&git_store, Self::on_git_store_event),
                // Reload when a host is connected or disconnected, so a
                // reconnected account leaves the AuthExpired state without the
                // user reopening the panel.
                git::git_host_credentials::observe_connections(cx, |this, cx| {
                    for section in &mut this.sections {
                        section.host_context = None;
                    }
                    this.refresh_all(cx);
                }),
                cx.observe_global::<SettingsStore>(|this, cx| {
                    this.restart_auto_refresh(cx);
                }),
            ];
            let mut this = Self {
                workspace: workspace_weak,
                project,
                fs,
                focus_handle,
                sections: Vec::new(),
                collapsed_sections: HashSet::default(),
                filter: StateFilter::Open,
                sort: SortOrder::RecentlyUpdated,
                reviewing: false,
                selected: None,
                opened_pr: None,
                active: false,
                _subscriptions: subscriptions,
                _auto_refresh_task: None,
            };
            this.sync_sections(cx);
            this
        })
    }

    fn section_index(&self, id: RepositoryId) -> Option<usize> {
        self.sections.iter().position(|section| section.id == id)
    }

    fn toggle_section_collapsed(&mut self, id: RepositoryId, cx: &mut Context<Self>) {
        if !self.collapsed_sections.remove(&id) {
            self.collapsed_sections.insert(id);
            if self.selected.is_some_and(|(section_index, _)| {
                self.sections
                    .get(section_index)
                    .is_some_and(|section| section.id == id)
            }) {
                self.selected = None;
            }
        }
        cx.notify();
    }

    /// Reconciles the sections with the workspace's repositories: existing
    /// sections keep their loaded pull requests, repositories that have gone
    /// away drop out, and newly opened ones start loading immediately.
    fn sync_sections(&mut self, cx: &mut Context<Self>) {
        let all_repositories: Vec<Entity<Repository>> = self
            .project
            .read(cx)
            .git_store()
            .read(cx)
            .repositories()
            .values()
            .cloned()
            .collect();

        // One section per repository *identity*, not per `Repository`. A linked
        // worktree is its own repository in the git store but shares a remote
        // with its main checkout, so rendering one section each would list the
        // same pull requests twice, the second time under the worktree's
        // directory name (`.pr12-review` rather than `offline-mode`).
        //
        // The main checkout is preferred as the representative so the label and
        // the remote come from it; when only a worktree is open, the first
        // repository seen for that identity stands in.
        let mut grouped: Vec<(Option<PathBuf>, Entity<Repository>, SharedString)> = Vec::new();
        for repository in all_repositories {
            let (identity, is_main, label) = {
                let repo = repository.read(cx);
                let snapshot = repo.snapshot();
                let identity = repo_identity_path_if_local(
                    &snapshot.common_dir_abs_path,
                    snapshot.path_style,
                )
                .map(Path::to_path_buf);
                let label = identity
                    .as_deref()
                    .and_then(|identity| {
                        if identity.extension() == Some(std::ffi::OsStr::new("git")) {
                            identity.file_stem()
                        } else {
                            identity.file_name()
                        }
                    })
                    .and_then(|name| name.to_str())
                    .map(SharedString::from)
                    .unwrap_or_else(|| repo.display_name());
                (identity, snapshot.is_main_worktree(), label)
            };

            // A repository whose identity cannot be resolved locally never
            // groups; it keeps a section of its own, as before.
            let existing = identity.as_ref().and_then(|identity| {
                grouped
                    .iter_mut()
                    .find(|(other, _, _)| other.as_ref() == Some(identity))
            });
            match existing {
                Some(entry) => {
                    if is_main {
                        entry.1 = repository;
                        entry.2 = label;
                    }
                }
                None => grouped.push((identity, repository, label)),
            }
        }
        grouped.sort_by_key(|(_, _, label)| label.to_lowercase());
        let repositories: Vec<(Entity<Repository>, SharedString)> = grouped
            .into_iter()
            .map(|(_, repository, label)| (repository, label))
            .collect();

        let mut existing: HashMap<RepositoryId, RepoSection> = std::mem::take(&mut self.sections)
            .into_iter()
            .map(|section| (section.id, section))
            .collect();

        let mut added = Vec::new();
        let mut sections = Vec::with_capacity(repositories.len());
        for (repository, display_name) in repositories {
            let id = repository.read(cx).id;
            match existing.remove(&id) {
                Some(mut section) => {
                    section.display_name = display_name;
                    section.repository = repository;
                    sections.push(section);
                }
                None => {
                    added.push(sections.len());
                    sections.push(RepoSection::new(id, display_name, repository));
                }
            }
        }
        self.sections = sections;
        self.collapsed_sections
            .retain(|id| self.sections.iter().any(|section| section.id == *id));

        for index in added {
            self.refresh_section(index, RefreshMode::Interactive, cx);
        }
        self.clamp_selection();
        cx.notify();
    }

    fn on_git_store_event(
        &mut self,
        _: Entity<GitStore>,
        event: &GitStoreEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            GitStoreEvent::RepositoryAdded | GitStoreEvent::RepositoryRemoved(_) => {
                self.sync_sections(cx);
            }
            // At launch a repository is present before its remote URLs are
            // scanned, so the first resolve sees no remote and parks in NoHost;
            // retry once the repository updates so the section populates without
            // a manual refresh. Gated on the repository still having no remotes
            // at all, because this event fires on ordinary git activity and
            // re-resolving would read the keychain every time.
            GitStoreEvent::RepositoryUpdated(id, _, _) => {
                if let Some(index) = self.section_index(*id)
                    && self.sections[index].awaiting_remotes
                {
                    self.refresh_section(index, RefreshMode::Interactive, cx);
                }
            }
            _ => {}
        }
    }

    /// Rebuilds the auto-refresh timer from the current settings and
    /// visibility. Dropping the previous task cancels it, which is how the
    /// timer pauses while the panel is hidden.
    fn restart_auto_refresh(&mut self, cx: &mut Context<Self>) {
        self._auto_refresh_task = None;
        if !self.active {
            return;
        }
        let settings = PullRequestPanelSettings::get_global(cx);
        if !settings.auto_refresh {
            return;
        }
        let interval = settings.auto_refresh_interval;
        self._auto_refresh_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(interval).await;
                if this
                    .update(cx, |this, cx| this.background_refresh_all(cx))
                    .is_err()
                {
                    return;
                }
            }
        }));
    }

    fn refresh_all(&mut self, cx: &mut Context<Self>) {
        for index in 0..self.sections.len() {
            self.refresh_section(index, RefreshMode::Interactive, cx);
        }
    }

    /// Timer-driven reload of every section. Unlike `refresh_all` this leaves
    /// whatever is on screen in place until new results actually arrive.
    fn background_refresh_all(&mut self, cx: &mut Context<Self>) {
        for index in 0..self.sections.len() {
            self.refresh_section(index, RefreshMode::Background, cx);
        }
    }

    fn refresh_section(&mut self, index: usize, mode: RefreshMode, cx: &mut Context<Self>) {
        let filter = self.filter;
        let reviewing = self.reviewing;
        let sort = self.sort;
        let registry = GitHostingProviderRegistry::global(cx);
        let http_client = cx.http_client();
        let Some(section) = self.sections.get_mut(index) else {
            return;
        };
        let id = section.id;
        let candidates = section.remote_candidates(cx);

        // A background pass only has something to protect when results are
        // already displayed. Any other state (never loaded, previously failed,
        // still loading) has nothing to lose, so it takes the interactive path
        // and gets a normal load, which doubles as automatic recovery from a
        // transient failure.
        let preserve = mode == RefreshMode::Background
            && matches!(section.state, LoadState::Loaded(_))
            && !section.loading_more;
        if mode == RefreshMode::Background && section.loading_more {
            // A "Load more" request is in flight and owns `_load_task`.
            // Starting a refresh here would cancel it and drop the page the
            // user explicitly asked for.
            return;
        }

        section.awaiting_remotes = candidates.is_empty();

        if candidates.is_empty() {
            if preserve {
                return;
            }
            section.loaded_pages = 0;
            section.loading_more = false;
            section.rows.clear();
            section.host_context = None;
            section._load_task = None;
            section.state = LoadState::NoHost(
                "This repository has no origin or upstream remote, so there is no host to query."
                    .into(),
            );
            self.clamp_selection();
            cx.notify();
            return;
        }

        // Re-fetch exactly as many pages as are on screen, so a background
        // refresh of a list the user has paged through does not silently
        // shrink it back to the first page.
        let pages = if preserve {
            section.loaded_pages.max(1)
        } else {
            section.loaded_pages = 0;
            section.loading_more = false;
            section.rows.clear();
            section.background_failure = None;
            section.state = LoadState::Loading;
            1
        };

        section._load_task = Some(cx.spawn(async move |this, cx| {
            let result = load_pull_request_pages(
                candidates,
                registry,
                filter,
                reviewing,
                pages,
                http_client,
                cx,
            )
            .await;
            this.update(cx, |this, cx| {
                let Some(index) = this.section_index(id) else {
                    return;
                };
                if let Some(section) = this.sections.get_mut(index) {
                    match result {
                        Ok(LoadOutcome::Loaded {
                            provider,
                            remote,
                            loaded,
                        }) => {
                            section.host_context = Some((provider, remote));
                            section.background_failure = None;
                            section.loaded_pages = pages;
                            section.note_loaded(&loaded);
                            section.state = LoadState::Loaded(loaded);
                        }
                        Ok(LoadOutcome::NoHost(reason)) => {
                            if !preserve {
                                section.host_context = None;
                                section.state = LoadState::NoHost(reason);
                            }
                        }
                        Err(error) => {
                            // A background failure is deliberately silent: the
                            // already-displayed list stays exactly as it is
                            // rather than being replaced by an error panel. The
                            // next manual refresh surfaces the real message.
                            if preserve {
                                log::warn!(
                                    "background pull request refresh failed, keeping previous results: {error:#}"
                                );
                                section.background_failure = Some(
                                    match error.downcast_ref::<git::PullRequestAuthError>() {
                                        Some(auth_error) => BackgroundFailure::AuthExpired {
                                            host: auth_error.host.clone(),
                                        },
                                        None => BackgroundFailure::Other(
                                            FailureMessage::from_error(&error),
                                        ),
                                    },
                                );
                                cx.notify();
                                return;
                            }
                            section.host_context = None;
                            section.state = match error.downcast_ref::<git::PullRequestAuthError>()
                            {
                                Some(auth_error) => LoadState::AuthExpired {
                                    host: auth_error.host.clone(),
                                },
                                None => LoadState::Failed(FailureMessage::from_error(&error)),
                            };
                        }
                    }
                    section.rebuild_rows(sort);
                }
                this.clamp_selection();
                this.start_review_enrichment(id, cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Fetches the page after the last one loaded in `index`'s section and
    /// appends it. Only the unauthored partition grows: the "Created by you"
    /// section is a separate, already-complete query.
    fn load_more(&mut self, index: usize, cx: &mut Context<Self>) {
        let filter = self.filter;
        let reviewing = self.reviewing;
        let sort = self.sort;
        let registry = GitHostingProviderRegistry::global(cx);
        let http_client = cx.http_client();
        let Some(section) = self.sections.get_mut(index) else {
            return;
        };
        if section.loading_more || !matches!(section.state, LoadState::Loaded(_)) {
            return;
        }
        let id = section.id;
        let next_page = section.loaded_pages + 1;
        let candidates = section.remote_candidates(cx);
        if candidates.is_empty() {
            return;
        }
        section.loading_more = true;
        section._load_task = Some(cx.spawn(async move |this, cx| {
            let result = load_pull_requests(
                candidates,
                registry,
                filter,
                reviewing,
                next_page,
                http_client,
                cx,
            )
            .await;
            this.update(cx, |this, cx| {
                let Some(index) = this.section_index(id) else {
                    return;
                };
                let mut failure = None;
                if let Some(section) = this.sections.get_mut(index) {
                    section.loading_more = false;
                    match result {
                        Ok(LoadOutcome::Loaded { loaded, .. }) => {
                            section.loaded_pages = next_page;
                            if let LoadState::Loaded(existing) = &mut section.state {
                                // The authored partition is re-queried whole on
                                // every page, so keep the copy already displayed
                                // and only extend the paged partition. Filter by
                                // number to stay idempotent if the host repeats a
                                // row across page boundaries.
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
                            section.rebuild_rows(sort);
                        }
                        Ok(LoadOutcome::NoHost(_)) => {}
                        Err(error) => failure = Some(error),
                    }
                }
                match failure {
                    // A failed "load more" keeps the rows already on screen;
                    // replacing them with an error would lose the user's place.
                    Some(error) => {
                        this.surface_error("Could not load more pull requests", &error, cx)
                    }
                    None => {
                        this.clamp_selection();
                        this.start_review_enrichment(id, cx);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Re-sorts every section in place, keeping the cursor on the same pull
    /// request. Used when the sort order changes, which never refetches.
    fn rebuild_all_rows(&mut self) {
        let selected = self.selected_key();
        let sort = self.sort;
        for section in &mut self.sections {
            section.rebuild_rows(sort);
        }
        self.restore_selection(selected);
    }

    /// The repository and pull request the cursor is on, used to put it back
    /// where it was after the rows underneath it are rebuilt.
    fn selected_key(&self) -> Option<(RepositoryId, u32)> {
        let (section_index, row_index) = self.selected?;
        let section = self.sections.get(section_index)?;
        let number = section.rows.get(row_index)?.pull_request()?.number;
        Some((section.id, number))
    }

    fn restore_selection(&mut self, key: Option<(RepositoryId, u32)>) {
        if let Some((id, number)) = key
            && let Some(section_index) = self.section_index(id)
            && let Some(row_index) = self.sections[section_index].rows.iter().position(|row| {
                row.pull_request()
                    .is_some_and(|summary| summary.number == number)
            })
        {
            self.selected = Some((section_index, row_index));
            return;
        }
        self.clamp_selection();
    }

    /// Keeps the cursor on a real, selectable row after sections or rows change.
    fn clamp_selection(&mut self) {
        let Some((section_index, row_index)) = self.selected else {
            return;
        };
        let Some(section) = self.sections.get(section_index) else {
            self.selected = None;
            return;
        };
        if self.collapsed_sections.contains(&section.id) {
            self.selected = None;
            return;
        }
        if section
            .rows
            .get(row_index)
            .is_some_and(PanelRow::is_selectable)
        {
            return;
        }
        // The row moved or went away; land on the nearest selectable row in the
        // same section rather than throwing the cursor back to the top.
        self.selected = section
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.is_selectable())
            .min_by_key(|(index, _)| index.abs_diff(row_index))
            .map(|(index, _)| (section_index, index));
    }

    fn total(&self) -> usize {
        self.sections.iter().map(RepoSection::total).sum()
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
    fn start_review_enrichment(&mut self, id: RepositoryId, cx: &mut Context<Self>) {
        let http_client = cx.http_client();
        let Some(index) = self.section_index(id) else {
            return;
        };
        let Some(section) = self.sections.get_mut(index) else {
            return;
        };
        let (freshness, numbers) = {
            let LoadState::Loaded(loaded) = &section.state else {
                return;
            };
            let mut freshness: HashMap<u32, SharedString> = HashMap::new();
            for summary in loaded.authored.iter().chain(loaded.others.iter()) {
                freshness.insert(summary.number, summary.updated_at.clone());
            }
            (freshness, loaded.numbers())
        };
        // Drop cache entries for PRs no longer in the list so the map cannot
        // grow without bound as the user pages and refilters.
        section
            .reviewers
            .retain(|number, _| freshness.contains_key(number));

        let stale: Vec<u32> = numbers
            .into_iter()
            .filter(|number| match section.reviewers.get(number) {
                Some((cached_at, _)) => freshness
                    .get(number)
                    .is_some_and(|current| current != cached_at),
                None => true,
            })
            .collect();
        if stale.is_empty() {
            return;
        }

        let Some((provider, remote)) = section.host_context.as_ref() else {
            return;
        };
        let provider = provider.clone();
        let remote = ParsedGitRemote {
            owner: remote.owner.clone(),
            repo: remote.repo.clone(),
        };
        let host = provider.base_url().host_str().map(|host| host.to_string());
        section._enrich_task = Some(cx.spawn(async move |this, cx| {
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
                        let Some(index) = this.section_index(id) else {
                            return;
                        };
                        if let Some(section) = this.sections.get_mut(index) {
                            for (number, reviewers) in results {
                                let updated_at = freshness
                                    .get(&number)
                                    .cloned()
                                    .unwrap_or_else(|| SharedString::from(""));
                                section.reviewers.insert(number, (updated_at, reviewers));
                            }
                            // Reviewer lists are what tell us the viewer has
                            // already ruled on a PR, so the indicator can only
                            // be resolved once they land.
                            section.clear_updated_for_own_verdict();
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
}

/// Keyboard navigation and the per-row actions the context menu exposes.
impl PullRequestPanel {
    /// Every selectable position, in the order the panel draws them: section by
    /// section, top to bottom. Section headers are labels and are left out, so
    /// the cursor steps over them and across section boundaries.
    fn selectable_positions(&self) -> Vec<(usize, usize)> {
        self.sections
            .iter()
            .enumerate()
            .filter(|(_, section)| !self.collapsed_sections.contains(&section.id))
            .flat_map(|(section_index, section)| {
                section
                    .rows
                    .iter()
                    .enumerate()
                    .filter(|(_, row)| row.is_selectable())
                    .map(move |(row_index, _)| (section_index, row_index))
            })
            .collect()
    }

    /// Moves the cursor by `delta` positions. Stops at the ends rather than
    /// wrapping, matching the other list panels.
    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let positions = self.selectable_positions();
        if positions.is_empty() {
            return;
        }
        let current = self
            .selected
            .and_then(|selected| positions.iter().position(|position| *position == selected));
        let next = match current {
            Some(index) => (index as isize + delta).clamp(0, positions.len() as isize - 1) as usize,
            // With no cursor yet, entering from the top selects the first
            // position and entering from the bottom selects the last.
            None if delta > 0 => 0,
            None => positions.len() - 1,
        };
        self.select_position(positions[next], cx);
    }

    fn select_first(&mut self, cx: &mut Context<Self>) {
        if let Some(position) = self.selectable_positions().first().copied() {
            self.select_position(position, cx);
        }
    }

    fn select_last(&mut self, cx: &mut Context<Self>) {
        if let Some(position) = self.selectable_positions().last().copied() {
            self.select_position(position, cx);
        }
    }

    fn select_position(&mut self, position: (usize, usize), cx: &mut Context<Self>) {
        let (section_index, row_index) = position;
        self.selected = Some(position);
        if let Some(section) = self.sections.get(section_index) {
            section.scroll_handle.scroll_to_item(row_index);
        }
        cx.notify();
    }

    fn selected_summary(&self) -> Option<(usize, &PullRequestSummary)> {
        let (section_index, row_index) = self.selected?;
        let section = self.sections.get(section_index)?;
        Some((section_index, section.rows.get(row_index)?.pull_request()?))
    }

    fn confirm_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((section_index, row_index)) = self.selected else {
            return;
        };
        let row = self
            .sections
            .get(section_index)
            .and_then(|section| section.rows.get(row_index))
            .cloned();
        match row {
            Some(PanelRow::PullRequest(summary)) => {
                self.open_pull_request(section_index, summary, window, cx)
            }
            Some(PanelRow::LoadMore) => self.load_more(section_index, cx),
            _ => {}
        }
    }

    fn open_pull_request(
        &mut self,
        section_index: usize,
        summary: PullRequestSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(section) = self.sections.get(section_index) else {
            return;
        };
        let Some((provider, remote)) = section.host_context.as_ref() else {
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
        let repository_id = section.id;
        let row_index = section.rows.iter().position(|row| {
            row.pull_request()
                .is_some_and(|candidate| candidate.number == number)
        });
        self.opened_pr = Some((repository_id, number));
        // Opening a PR is what marks it seen, so the updated indicator clears
        // here rather than on the next refresh. A PR that changes again after
        // this point is flagged again.
        if let Some(section) = self.sections.get_mut(section_index) {
            section.updated_since_seen.remove(&number);
            section.rebuild_rows(self.sort);
        }
        if let Some(row_index) = row_index {
            self.selected = Some((section_index, row_index));
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

    /// Clears a PR's updated indicator without opening it. A PR that changes
    /// again after this point is flagged again, exactly as if it had been
    /// opened.
    fn mark_pull_request_read(
        &mut self,
        section_index: usize,
        number: u32,
        cx: &mut Context<Self>,
    ) {
        let Some(section) = self.sections.get_mut(section_index) else {
            return;
        };
        if section.updated_since_seen.remove(&number) {
            cx.notify();
        }
    }

    /// Checks out the pull request's source branch in the repository the row
    /// belongs to.
    ///
    /// Runs `git switch`, which creates a local tracking branch when exactly one
    /// remote has the branch. A branch that has never been fetched fails here,
    /// and the host's error is surfaced verbatim because it names the fix.
    fn checkout_branch(
        &mut self,
        section_index: usize,
        branch: SharedString,
        cx: &mut Context<Self>,
    ) {
        let Some(section) = self.sections.get(section_index) else {
            return;
        };
        let repository = section.repository.clone();
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
        if let Some(url) = self
            .selected_summary()
            .map(|(_, summary)| summary.url.to_string())
        {
            cx.open_url(&url);
        }
    }

    fn on_refresh(&mut self, _: &Refresh, _: &mut Window, cx: &mut Context<Self>) {
        self.refresh_all(cx);
    }

    fn on_open_selected_in_browser(
        &mut self,
        _: &OpenSelectedInBrowser,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(url) = self
            .selected_summary()
            .map(|(_, summary)| summary.url.to_string())
        {
            cx.open_url(&url);
        }
    }

    fn on_checkout_selected_branch(
        &mut self,
        _: &CheckoutSelectedBranch,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((section_index, branch)) = self
            .selected_summary()
            .map(|(section_index, summary)| (section_index, summary.source_branch.clone()))
        {
            self.checkout_branch(section_index, branch, cx);
        }
    }
}

/// The per-row context menu. Every entry works on the row that was clicked
/// rather than the keyboard cursor, so right-clicking a row the cursor is not on
/// does what it looks like it does.
fn row_context_menu(
    section_index: usize,
    summary: &PullRequestSummary,
    is_updated: bool,
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
        let panel_for_checkout = panel.clone();
        let panel_for_mark_read = panel;
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
                    panel.checkout_branch(section_index, checkout.clone(), cx);
                })
                .ok();
        })
        .when(is_updated, move |menu| {
            menu.separator()
                .entry("Mark as Read", None, move |_window, cx| {
                    panel_for_mark_read
                        .update(cx, |panel, cx| {
                            panel.mark_pull_request_read(section_index, number, cx);
                        })
                        .ok();
                })
        })
    })
}

/// Rendering.
impl PullRequestPanel {
    fn render_header(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.total();
        let loading = self
            .sections
            .iter()
            .any(|section| matches!(section.state, LoadState::Loading));

        h_flex()
            .h(rems(2.))
            .px_2()
            .gap_1()
            .flex_none()
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
                                Tooltip::for_action("New Pull Request", &CreatePullRequest, cx)
                            })
                            .on_click(cx.listener(|_, _, window, cx| {
                                // Dispatch through the window rather than the
                                // app: `App::dispatch_action` re-enters the
                                // active window's update from inside that same
                                // update, which fails and is then swallowed by
                                // a log_err, so the click silently does nothing.
                                window.dispatch_action(Box::new(CreatePullRequest), cx);
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
                            this.refresh_all(cx);
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
                                            this.refresh_all(cx);
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
                                    this.refresh_all(cx);
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
                                            this.rebuild_all_rows();
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

    /// One repository's pane. Every section is a flex child with a zero basis so
    /// the available height is divided evenly between the repositories, and each
    /// one scrolls on its own.
    /// The strip shown above results that are still on screen after a
    /// background refresh failed. It reports staleness without taking the list
    /// away, and offers the same recovery action the blocking states do.
    fn render_background_failure(
        &self,
        section_index: usize,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let section = self.sections.get(section_index)?;
        let failure = section.background_failure.clone()?;
        let accent = Color::Warning.color(cx);

        let strip = h_flex()
            .id(("pull-request-panel-stale", section_index))
            .w_full()
            .px_2()
            .py_1()
            .gap_1p5()
            .items_center()
            .bg(accent.opacity(0.12))
            .border_b_1()
            .border_color(accent.opacity(0.4))
            .child(
                Icon::new(IconName::Warning)
                    .size(IconSize::XSmall)
                    .color(Color::Warning),
            );

        let element = match failure {
            BackgroundFailure::AuthExpired { host } => {
                let display = crate::pull_request_view::host_display_name(cx, &host);
                let host_for_action = host.to_string();
                strip
                    .child(
                        Label::new(format!(
                            "{display} sign-in expired. List may be out of date."
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Warning)
                        .truncate(),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new(
                            ("pull-request-panel-stale-reconnect", section_index),
                            "Reconnect",
                        )
                        .label_size(LabelSize::XSmall)
                        .on_click(cx.listener(move |_, _, window, cx| {
                            window.dispatch_action(
                                Box::new(zed_actions::ConnectGitHost {
                                    host: host_for_action.clone(),
                                }),
                                cx,
                            );
                        })),
                    )
            }
            BackgroundFailure::Other(message) => strip
                .child(
                    Label::new(format!("Could not refresh. {}", message.summary))
                        .size(LabelSize::XSmall)
                        .color(Color::Warning)
                        .truncate(),
                )
                .tooltip(move |_, cx| Tooltip::simple(message.detail.clone(), cx))
                .child(div().flex_1())
                .child(
                    Button::new(("pull-request-panel-stale-retry", section_index), "Retry")
                        .label_size(LabelSize::XSmall)
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.refresh_section(section_index, RefreshMode::Interactive, cx)
                        })),
                ),
        };
        Some(element.into_any_element())
    }

    fn render_section(
        &self,
        section_index: usize,
        divider: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(section) = self.sections.get(section_index) else {
            return div().into_any_element();
        };
        let is_collapsed = self.collapsed_sections.contains(&section.id);
        let id = section.id;
        let state = section.state.clone();
        let display_name = section.display_name.clone();
        let count = section.total();
        let loading = matches!(state, LoadState::Loading);
        let list_id: SharedString = format!("pull-request-panel-rows-{}", section.id.0).into();

        let body = match state {
            LoadState::Idle | LoadState::Loading => {
                Self::render_message("Loading pull requests…", Color::Muted, None)
                    .into_any_element()
            }
            LoadState::Loaded(loaded) => {
                if loaded.is_empty() {
                    let queried = section
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
                    let rows = section.rows.clone();
                    let rendered_rows: Vec<AnyElement> = rows
                        .iter()
                        .enumerate()
                        .map(|(index, row)| match row {
                            PanelRow::Header(label) => {
                                render_section_header(label.clone()).into_any_element()
                            }
                            PanelRow::PullRequest(summary) => {
                                self.render_row(section_index, index, summary.clone(), cx)
                            }
                            PanelRow::LoadMore => self.render_load_more(section_index, index, cx),
                        })
                        .collect();
                    v_flex()
                        .id(list_id)
                        .w_full()
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .track_scroll(&section.scroll_handle)
                        .children(rendered_rows)
                        .into_any_element()
                }
            }
            LoadState::NoHost(reason) => {
                Self::render_message("No connected host", Color::Muted, Some(reason))
                    .into_any_element()
            }
            LoadState::Failed(failure) => {
                Self::render_message(failure.summary.clone(), Color::Error, Some(failure.detail))
                    .child(
                        Button::new(("pull-request-panel-retry", section_index), "Try Again")
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.refresh_section(section_index, RefreshMode::Interactive, cx)
                            })),
                    )
                    .into_any_element()
            }
            LoadState::AuthExpired { host } => {
                let host_for_action = host.to_string();
                let display = crate::pull_request_view::host_display_name(cx, &host);
                Self::render_message(
                    "Connection expired",
                    Color::Error,
                    Some(
                        format!(
                            "Your {display} sign-in is no longer valid. Reconnect to continue."
                        )
                        .into(),
                    ),
                )
                .child(
                    Button::new(("pull-request-panel-reconnect", section_index), "Reconnect")
                        .on_click(cx.listener(move |_, _, window, cx| {
                            window.dispatch_action(
                                Box::new(zed_actions::ConnectGitHost {
                                    host: host_for_action.clone(),
                                }),
                                cx,
                            );
                        })),
                )
                .into_any_element()
            }
        };

        v_flex()
            .min_h_0()
            .when(is_collapsed, |this| this.flex_none())
            .when(!is_collapsed, |this| this.flex_1().overflow_hidden())
            .when(divider, |this| {
                this.border_b_1().border_color(cx.theme().colors().border)
            })
            .child(render_repo_label(
                id,
                display_name,
                count,
                loading,
                is_collapsed,
                cx,
            ))
            .when(!is_collapsed, |this| {
                this.children(self.render_background_failure(section_index, cx))
                    .child(body)
            })
            .into_any_element()
    }

    fn render_row(
        &self,
        section_index: usize,
        ix: usize,
        summary: PullRequestSummary,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(section) = self.sections.get(section_index) else {
            return div().into_any_element();
        };
        let title = summary.title.clone();
        let number = summary.number;
        let author = summary.author_login.clone();
        let branch = summary.source_branch.clone();
        let is_draft = summary.is_draft;
        let state_color = match summary.state {
            PullRequestState::Open if is_draft => Color::Warning,
            PullRequestState::Open => Color::Success,
            PullRequestState::Merged => Color::Accent,
            PullRequestState::Closed => Color::Error,
        };

        let is_updated = section.is_updated_since_seen(number);
        let is_open_in_tab = self.opened_pr == Some((section.id, number));
        let is_selected = self.selected == Some((section_index, ix));
        let summary_for_menu = summary.clone();
        let summary_for_click = summary;

        let row = h_flex()
            .id(("pr-row", ix))
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .items_start()
            .border_l_2()
            .border_color(if is_open_in_tab {
                cx.theme().colors().border_focused
            } else {
                gpui::transparent_black()
            })
            // Tint rows the connected account has already reviewed (filled in
            // lazily by `start_review_enrichment`). Selection/hover override it.
            .when_some(section.my_verdict(number), |this, verdict| match verdict {
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
            .on_click(
                cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                    // Cmd/ctrl-click and middle-click open on the host's website,
                    // matching how links behave everywhere else.
                    if event.modifiers().secondary() || event.is_middle_click() {
                        cx.open_url(summary_for_click.url.as_str());
                    } else {
                        this.open_pull_request(
                            section_index,
                            summary_for_click.clone(),
                            window,
                            cx,
                        );
                    }
                }),
            )
            .child(
                // The icon column doubles as the updated-since-seen gutter. The
                // dot sits directly under the icon, which puts it left of the
                // author name on the row below. The placeholder keeps the
                // column a fixed width so titles do not shift horizontally as
                // rows are flagged and cleared.
                v_flex()
                    .flex_none()
                    .items_center()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::PullRequest)
                            .size(IconSize::Small)
                            .color(state_color),
                    )
                    .child(
                        div()
                            .size(px(6.))
                            .rounded_full()
                            .when(is_updated, |this| this.bg(Color::Info.color(cx))),
                    ),
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
                                let warning = Color::Warning.color(cx);
                                this.child(
                                    h_flex()
                                        .px_1()
                                        .rounded_sm()
                                        .border_1()
                                        .border_color(warning.opacity(0.5))
                                        .bg(warning.opacity(0.12))
                                        .child(
                                            Label::new("Draft")
                                                .size(LabelSize::XSmall)
                                                .color(Color::Warning),
                                        ),
                                )
                            })
                            .when(self.reviewing, |this| {
                                let (verdict_label, verdict_color) =
                                    match section.my_verdict(number) {
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
            .when_some(
                self.render_reviewer_rollup(section_index, number, cx),
                |this, rollup| this.child(rollup),
            );

        let panel = cx.entity().downgrade();
        right_click_menu(("pr-row-menu", ix))
            .trigger(move |_, _, _| row)
            .menu(move |window, cx| {
                row_context_menu(
                    section_index,
                    &summary_for_menu,
                    is_updated,
                    panel.clone(),
                    window,
                    cx,
                )
            })
            .into_any_element()
    }

    /// The trailing row that fetches the next page.
    fn render_load_more(
        &self,
        section_index: usize,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_selected = self.selected == Some((section_index, ix));
        let loading = self
            .sections
            .get(section_index)
            .is_some_and(|section| section.loading_more);
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
            .on_click(cx.listener(move |this, _, _window, cx| this.load_more(section_index, cx)))
            .child(
                Label::new(if loading { "Loading…" } else { "Load more" })
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    /// A wrapped reviewer summary for a PR list row. Every verdict-colored dot
    /// is visible: approved reviewers are green, changes-requested reviewers
    /// are red, comments are blue, and pending reviews are hollow.
    /// `None` when reviewers are still loading or the host reports none.
    fn render_reviewer_rollup(
        &self,
        section_index: usize,
        number: u32,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let reviewers = self.sections.get(section_index)?.reviewers_for(number)?;
        if reviewers.is_empty() {
            return None;
        }
        let dots = reviewers.iter().map(|reviewer| {
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
                .w(px(56.))
                .flex_wrap()
                .gap_1()
                .children(dots)
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
            .min_h_0()
            .items_center()
            .justify_center()
            .gap_1()
            .p_2()
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

/// Names the repository a section belongs to and toggles its pull request list.
fn render_repo_label(
    id: RepositoryId,
    name: SharedString,
    count: usize,
    loading: bool,
    is_collapsed: bool,
    cx: &mut Context<PullRequestPanel>,
) -> impl IntoElement {
    h_flex()
        .id(("pull-request-repo-section", id.0))
        .h(rems(1.75))
        .px_2()
        .gap_1p5()
        .flex_none()
        .cursor_pointer()
        .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
        .bg(cx.theme().colors().elevated_surface_background)
        .border_b_1()
        .border_color(cx.theme().colors().border_variant)
        .child(
            Icon::new(if is_collapsed {
                IconName::ChevronRight
            } else {
                IconName::ChevronDown
            })
            .size(IconSize::XSmall)
            .color(Color::Muted),
        )
        .child(
            Icon::new(IconName::GitBranch)
                .size(IconSize::XSmall)
                .color(Color::Muted),
        )
        .child(Label::new(name).size(LabelSize::XSmall).truncate())
        .child(
            Label::new(if loading {
                "loading…".to_string()
            } else {
                format!("({count})")
            })
            .size(LabelSize::XSmall)
            .color(Color::Muted),
        )
        .on_click(cx.listener(move |this, _, _window, cx| {
            this.toggle_section_collapsed(id, cx);
        }))
}

/// A section header row inside a repository's pull request list.
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

/// Queries `candidates` (origin first, then upstream when set) and returns the
/// pull requests of the first remote that resolves to a known host and has a
/// credential. Stopping at the first usable remote is deliberate: a fork shows
/// its own `origin` pull requests (often none) rather than falling through to
/// the `upstream` project and surfacing the canonical repository's PRs.
/// Records the `updated_at` of every PR in `loaded`, inserting into `updated`
/// any whose timestamp moved since it was last seen.
///
/// On the first load nothing is flagged: no PR has a prior `updated_at`
/// recorded, so the comparison cannot fire. That is what keeps opening the
/// panel from lighting up every row at once.
///
/// Entries are never pruned for PRs that drop out of the response. A PR leaves
/// the list whenever the state filter changes, and dropping its seen-state
/// would silently clear an unread mark the user has not looked at yet. The map
/// is bounded by the number of PRs in the repository.
fn note_updated_pull_requests(
    last_seen: &mut HashMap<u32, SharedString>,
    updated: &mut HashSet<u32>,
    loaded: &LoadedPullRequests,
) {
    for summary in loaded.authored.iter().chain(loaded.others.iter()) {
        if let Some(previous) = last_seen.get(&summary.number)
            && previous != &summary.updated_at
        {
            updated.insert(summary.number);
        }
        last_seen.insert(summary.number, summary.updated_at.clone());
    }
}

/// Removes from `updated` every PR whose cached reviewer list carries the
/// viewer's own approval or requested changes.
///
/// Submitting a review is itself an update on the host, so a PR the user has
/// just ruled on comes back with a moved `updated_at` and lights up for the
/// user's own action. Having a verdict on record also means the row has been
/// looked at, which is what the indicator is there to say. A comment-only
/// review is left flagged: it settles nothing, so the row is still worth
/// returning to.
fn clear_updated_for_own_verdict(
    updated: &mut HashSet<u32>,
    reviewers: &HashMap<u32, (SharedString, Vec<PullRequestReviewer>)>,
) {
    updated.retain(|number| {
        let verdict = reviewers
            .get(number)
            .and_then(|(_, reviewers)| reviewers.iter().find(|reviewer| reviewer.is_me))
            .and_then(|reviewer| reviewer.verdict);
        !matches!(
            verdict,
            Some(PullRequestReviewVerdict::Approve | PullRequestReviewVerdict::RequestChanges)
        )
    });
}

/// Loads `pages` pages and concatenates them into one result, mirroring how
/// `load_more` extends a section. Used so a background refresh can rebuild a
/// list the user has paged through at its current length instead of truncating
/// it to the first page.
async fn load_pull_request_pages(
    candidates: Vec<String>,
    registry: Arc<GitHostingProviderRegistry>,
    filter: StateFilter,
    reviewing: bool,
    pages: u32,
    http_client: Arc<dyn HttpClient>,
    cx: &mut gpui::AsyncApp,
) -> Result<LoadOutcome> {
    let mut combined: Option<LoadOutcome> = None;
    for page in 1..=pages.max(1) {
        let outcome = load_pull_requests(
            candidates.clone(),
            registry.clone(),
            filter,
            reviewing,
            page,
            http_client.clone(),
            cx,
        )
        .await?;

        match (&mut combined, outcome) {
            (None, outcome) => combined = Some(outcome),
            // A later page reporting no host is not a reason to discard the
            // pages already collected.
            (Some(_), LoadOutcome::NoHost(_)) => break,
            (
                Some(LoadOutcome::Loaded {
                    loaded: existing, ..
                }),
                LoadOutcome::Loaded { loaded: next, .. },
            ) => {
                // `authored` is re-queried whole on every page, so the first
                // page's copy is already complete. Only the paged partition
                // grows, deduplicated by number in case the host repeats a row
                // across a page boundary.
                let seen: HashSet<u32> = existing
                    .authored
                    .iter()
                    .chain(existing.others.iter())
                    .map(|summary| summary.number)
                    .collect();
                existing.others.extend(
                    next.others
                        .into_iter()
                        .filter(|s| !seen.contains(&s.number)),
                );
                existing.may_have_more = next.may_have_more;
                if !existing.may_have_more {
                    break;
                }
            }
            (Some(LoadOutcome::NoHost(_)), _) => break,
        }
    }
    combined.ok_or_else(|| anyhow::anyhow!("no pull request pages were requested"))
}

async fn load_pull_requests(
    candidates: Vec<String>,
    registry: Arc<GitHostingProviderRegistry>,
    filter: StateFilter,
    reviewing: bool,
    page: u32,
    http_client: Arc<dyn HttpClient>,
    cx: &mut gpui::AsyncApp,
) -> Result<LoadOutcome> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use git::repository::Worktree as GitWorktree;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;
    use workspace::MultiWorkspace;

    fn summary(number: u32, updated_at: &str) -> PullRequestSummary {
        PullRequestSummary {
            number,
            title: format!("PR {number}").into(),
            author_login: "octocat".into(),
            state: PullRequestState::Open,
            source_branch: "feature".into(),
            target_branch: "main".into(),
            url: "https://example.com/pull/1".parse().unwrap(),
            updated_at: updated_at.into(),
            is_draft: false,
        }
    }

    fn loaded(summaries: Vec<PullRequestSummary>) -> LoadedPullRequests {
        LoadedPullRequests {
            authored: Vec::new(),
            others: summaries,
            may_have_more: false,
        }
    }

    #[test]
    fn first_load_flags_nothing() {
        let mut last_seen = HashMap::new();
        let mut updated = HashSet::default();
        note_updated_pull_requests(
            &mut last_seen,
            &mut updated,
            &loaded(vec![summary(1, "2026-09-14T10:00:00Z")]),
        );
        assert!(
            updated.is_empty(),
            "opening the panel should not flag every row"
        );
        assert_eq!(last_seen.len(), 1);
    }

    #[test]
    fn only_a_moved_timestamp_flags_a_pull_request() {
        let mut last_seen = HashMap::new();
        let mut updated = HashSet::default();
        let first = loaded(vec![
            summary(1, "2026-09-14T10:00:00Z"),
            summary(2, "2026-09-14T10:00:00Z"),
        ]);
        note_updated_pull_requests(&mut last_seen, &mut updated, &first);

        // PR 1 changed, PR 2 came back identical.
        let second = loaded(vec![
            summary(1, "2026-09-14T11:30:00Z"),
            summary(2, "2026-09-14T10:00:00Z"),
        ]);
        note_updated_pull_requests(&mut last_seen, &mut updated, &second);

        assert!(updated.contains(&1));
        assert!(!updated.contains(&2), "an unchanged PR must not be flagged");
    }

    #[test]
    fn a_flag_survives_later_refreshes_until_it_is_cleared() {
        let mut last_seen = HashMap::new();
        let mut updated = HashSet::default();
        note_updated_pull_requests(
            &mut last_seen,
            &mut updated,
            &loaded(vec![summary(1, "2026-09-14T10:00:00Z")]),
        );
        note_updated_pull_requests(
            &mut last_seen,
            &mut updated,
            &loaded(vec![summary(1, "2026-09-14T11:00:00Z")]),
        );
        assert!(updated.contains(&1));

        // A refresh that sees no further change must not clear the mark: the
        // user has still not opened it.
        note_updated_pull_requests(
            &mut last_seen,
            &mut updated,
            &loaded(vec![summary(1, "2026-09-14T11:00:00Z")]),
        );
        assert!(
            updated.contains(&1),
            "the indicator clears on open, not on the next refresh"
        );

        // Opening the PR is what clears it.
        updated.remove(&1);
        note_updated_pull_requests(
            &mut last_seen,
            &mut updated,
            &loaded(vec![summary(1, "2026-09-14T11:00:00Z")]),
        );
        assert!(!updated.contains(&1));

        // A change after that flags it again.
        note_updated_pull_requests(
            &mut last_seen,
            &mut updated,
            &loaded(vec![summary(1, "2026-09-14T12:00:00Z")]),
        );
        assert!(updated.contains(&1));
    }

    fn reviewer(
        login: &str,
        is_me: bool,
        verdict: Option<PullRequestReviewVerdict>,
    ) -> PullRequestReviewer {
        PullRequestReviewer {
            login: login.into(),
            verdict,
            is_me,
        }
    }

    fn reviewer_cache(
        entries: Vec<(u32, Vec<PullRequestReviewer>)>,
    ) -> HashMap<u32, (SharedString, Vec<PullRequestReviewer>)> {
        entries
            .into_iter()
            .map(|(number, reviewers)| {
                (
                    number,
                    (SharedString::from("2026-09-14T11:00:00Z"), reviewers),
                )
            })
            .collect()
    }

    #[test]
    fn the_viewers_own_verdict_clears_the_updated_indicator() {
        let mut updated: HashSet<u32> = [1, 2, 3, 4, 5].into_iter().collect();
        let reviewers = reviewer_cache(vec![
            (
                1,
                vec![reviewer(
                    "me",
                    true,
                    Some(PullRequestReviewVerdict::Approve),
                )],
            ),
            (
                2,
                vec![reviewer(
                    "me",
                    true,
                    Some(PullRequestReviewVerdict::RequestChanges),
                )],
            ),
            (
                3,
                vec![reviewer(
                    "me",
                    true,
                    Some(PullRequestReviewVerdict::Comment),
                )],
            ),
            (4, vec![reviewer("me", true, None)]),
            (
                5,
                vec![reviewer(
                    "octocat",
                    false,
                    Some(PullRequestReviewVerdict::Approve),
                )],
            ),
        ]);

        clear_updated_for_own_verdict(&mut updated, &reviewers);

        assert!(!updated.contains(&1), "our approval clears the indicator");
        assert!(
            !updated.contains(&2),
            "our requested changes clear the indicator"
        );
        assert!(
            updated.contains(&3),
            "a comment-only review settles nothing"
        );
        assert!(updated.contains(&4), "a pending review is not a verdict");
        assert!(updated.contains(&5), "someone else's approval is not ours");
    }

    #[test]
    fn a_pull_request_without_cached_reviewers_stays_flagged() {
        let mut updated: HashSet<u32> = [1].into_iter().collect();
        clear_updated_for_own_verdict(&mut updated, &reviewer_cache(vec![(1, Vec::new())]));
        assert!(updated.contains(&1));

        clear_updated_for_own_verdict(&mut updated, &HashMap::new());
        assert!(
            updated.contains(&1),
            "reviewers that have not loaded yet must not clear the indicator"
        );
    }

    #[gpui::test]
    async fn sync_sections_groups_linked_worktrees_with_their_repository(cx: &mut TestAppContext) {
        cx.update(|cx| {
            zlog::init_test();
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            GitHostingProviderRegistry::default_global(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "offline-mode": { ".git": {} },
            }),
        )
        .await;
        fs.set_branch_name(Path::new(path!("/root/offline-mode/.git")), Some("master"));
        fs.insert_branches(
            Path::new(path!("/root/offline-mode/.git")),
            &["master", "pr12-review"],
        );

        // A linked worktree of `offline-mode`, checked out beside it. The git
        // store reports it as its own repository, so without grouping the panel
        // renders a second section titled `.pr12-review`.
        fs.add_linked_worktree_for_repo(
            Path::new(path!("/root/offline-mode/.git")),
            true,
            GitWorktree {
                path: PathBuf::from(path!("/root/.pr12-review")),
                ref_name: Some("refs/heads/pr12-review".into()),
                sha: "abc123".into(),
                is_main: false,
                is_bare: false,
            },
        )
        .await;

        let project = Project::test(
            fs,
            [
                path!("/root/offline-mode").as_ref(),
                path!("/root/.pr12-review").as_ref(),
            ],
            cx,
        )
        .await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |workspace, _| workspace.workspace().clone());
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            PullRequestPanel::new(workspace, window, cx)
        });

        // Without this the test could pass vacuously: if the worktree never
        // registered as its own repository there would be one section anyway,
        // and the grouping below would never have run.
        let repository_count = project.read_with(cx, |project, cx| {
            project.git_store().read(cx).repositories().len()
        });
        assert_eq!(
            repository_count, 2,
            "the linked worktree must register as a separate repository for this \
             test to exercise the grouping at all"
        );

        let display_names = panel.read_with(cx, |panel, _| {
            panel
                .sections
                .iter()
                .map(|section| section.display_name.to_string())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            display_names,
            ["offline-mode"],
            "a linked worktree shares its repository's remote, so its pull requests \
             belong under the main checkout rather than in a section of their own"
        );
    }

    #[gpui::test]
    async fn sync_sections_includes_each_open_repository(cx: &mut TestAppContext) {
        cx.update(|cx| {
            zlog::init_test();
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            GitHostingProviderRegistry::default_global(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "backend": { ".git": {} },
                "management-ui": { ".git": {} },
            }),
        )
        .await;

        let project = Project::test(
            fs,
            [
                path!("/root/backend").as_ref(),
                path!("/root/management-ui").as_ref(),
            ],
            cx,
        )
        .await;
        let scan_complete = project.read_with(cx, |project, cx| project.wait_for_initial_scan(cx));
        scan_complete.await;

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |workspace, _| workspace.workspace().clone());
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            PullRequestPanel::new(workspace, window, cx)
        });

        let display_names = panel.read_with(cx, |panel, _| {
            panel
                .sections
                .iter()
                .map(|section| section.display_name.to_string())
                .collect::<Vec<_>>()
        });
        assert_eq!(display_names, ["backend", "management-ui"]);

        let backend_id = panel.read_with(cx, |panel, _| panel.sections[0].id);
        panel.update(cx, |panel, cx| {
            panel.toggle_section_collapsed(backend_id, cx);
        });
        assert!(panel.read_with(cx, |panel, _| {
            panel.collapsed_sections.contains(&backend_id)
        }));

        panel.update(cx, |panel, cx| {
            panel.toggle_section_collapsed(backend_id, cx);
        });
        assert!(!panel.read_with(cx, |panel, _| {
            panel.collapsed_sections.contains(&backend_id)
        }));
    }
}

impl Render for PullRequestPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel_bg = cx.theme().colors().panel_background;
        let header = self.render_header(window, cx).into_any_element();

        let body = if self.sections.is_empty() {
            Self::render_message(
                "No repositories open",
                Color::Muted,
                Some("Open a folder that is a git repository to see its pull requests.".into()),
            )
            .into_any_element()
        } else {
            let last = self.sections.len() - 1;
            let sections: Vec<AnyElement> = (0..self.sections.len())
                .map(|index| self.render_section(index, index < last, cx))
                .collect();
            v_flex()
                .flex_1()
                .min_h_0()
                .children(sections)
                .into_any_element()
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
        match self.total() {
            0 => None,
            total => Some(total.to_string()),
        }
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn starts_open(&self, _: &Window, _: &App) -> bool {
        false
    }

    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        if self.active == active {
            return;
        }
        self.active = active;
        if active {
            // What is on screen was last accurate when the panel was hidden,
            // which may have been hours ago. Reload immediately rather than
            // making the user wait out a full interval for a correct list.
            self.background_refresh_all(cx);
        }
        self.restart_auto_refresh(cx);
    }

    fn activation_priority(&self) -> u32 {
        // Must be unique across all registered panels (dock.rs panics in debug
        // builds otherwise). 5 collides with CollabPanel; 4 is a free slot.
        4
    }
}
