use std::{ops::Range, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;
use derive_more::{Deref, DerefMut};
use gpui::{App, Global, SharedString};
use http_client::HttpClient;
use itertools::Itertools;
use parking_lot::RwLock;
use url::Url;

use crate::repository::RepoPath;

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct PullRequest {
    pub number: u32,
    pub url: Url,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestState {
    Open,
    Closed,
    Merged,
}

/// Lightweight summary of a pull request, enough to populate a PR list panel
/// without round-tripping the full review payload (comments, diffs, etc.).
#[derive(Debug, Clone)]
pub struct PullRequestSummary {
    pub number: u32,
    pub title: SharedString,
    pub author_login: SharedString,
    pub state: PullRequestState,
    pub source_branch: SharedString,
    pub target_branch: SharedString,
    pub url: Url,
    /// ISO-8601 timestamp string as returned by the host; downstream UI parses
    /// it lazily when sorting / displaying.
    pub updated_at: SharedString,
    pub is_draft: bool,
}

/// Filter applied when listing pull requests from a host.
#[derive(Debug, Clone, Default)]
pub struct PullRequestListFilter {
    /// `None` = all states. `Some` restricts to the listed states.
    pub states: Option<Vec<PullRequestState>>,
    /// Restrict to PRs authored by this login (substring match).
    pub author: Option<SharedString>,
    /// Cap on returned PRs. `None` = whatever the provider's default is.
    pub limit: Option<u32>,
}

/// The full picture of a pull request — enough for a PR detail view to render
/// the header, body, branch refs, mergeability, and check whether the local
/// repository is in sync.
#[derive(Debug, Clone)]
pub struct PullRequestDetail {
    pub number: u32,
    pub title: SharedString,
    pub body: SharedString,
    pub state: PullRequestState,
    pub author_login: SharedString,
    pub source_branch: SharedString,
    pub target_branch: SharedString,
    pub head_sha: SharedString,
    pub base_sha: SharedString,
    pub url: Url,
    pub updated_at: SharedString,
    pub is_draft: bool,
    /// `Some(true)` if the host can fast-forward or auto-merge; `Some(false)`
    /// if there's a known conflict; `None` if the host hasn't computed it yet.
    pub is_mergeable: Option<bool>,
    pub additions: u32,
    pub deletions: u32,
    pub changed_files: u32,
}

/// How a pull request should be combined into the target branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestMergeMethod {
    /// Create a merge commit (`git merge --no-ff` semantics).
    Merge,
    /// Squash all commits into one before applying.
    Squash,
    /// Rebase the head onto the base, no merge commit.
    Rebase,
}

/// The action a reviewer is taking when submitting a review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestReviewVerdict {
    /// Approve the PR (allowing merge if the host requires approval).
    Approve,
    /// Block the PR until changes are made.
    RequestChanges,
    /// Leave overall feedback without approving or blocking.
    Comment,
}

/// A single inline review comment posted on a pull request.
#[derive(Debug, Clone)]
pub struct PullRequestReviewComment {
    pub id: u64,
    pub author_login: SharedString,
    pub body: SharedString,
    /// Repo-relative path the comment is anchored to.
    pub path: SharedString,
    /// 1-indexed line in the file the comment is anchored to, when the host
    /// reports one. `None` for file-level comments that don't pin a line.
    pub line: Option<u32>,
    pub created_at: SharedString,
    pub url: Url,
}

#[derive(Clone)]
pub struct GitRemote {
    pub host: Arc<dyn GitHostingProvider + Send + Sync + 'static>,
    pub owner: SharedString,
    pub repo: SharedString,
}

impl std::fmt::Debug for GitRemote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitRemote")
            .field("host", &self.host.name())
            .field("owner", &self.owner)
            .field("repo", &self.repo)
            .finish()
    }
}

impl GitRemote {
    pub fn host_supports_avatars(&self) -> bool {
        self.host.supports_avatars()
    }

    pub async fn avatar_url(
        &self,
        commit: SharedString,
        author_email: Option<SharedString>,
        client: Arc<dyn HttpClient>,
    ) -> Option<Url> {
        self.host
            .commit_author_avatar_url(&self.owner, &self.repo, commit, author_email, client)
            .await
            .ok()
            .flatten()
    }
}

pub struct BuildCommitPermalinkParams<'a> {
    pub sha: &'a str,
}

pub struct BuildPermalinkParams<'a> {
    pub sha: &'a str,
    /// URL-escaped path using unescaped `/` as the directory separator.
    pub path: String,
    pub selection: Option<Range<u32>>,
}

impl<'a> BuildPermalinkParams<'a> {
    pub fn new(sha: &'a str, path: &RepoPath, selection: Option<Range<u32>>) -> Self {
        Self {
            sha,
            path: path.components().map(urlencoding::encode).join("/"),
            selection,
        }
    }
}

/// A Git hosting provider.
#[async_trait]
pub trait GitHostingProvider {
    /// Returns the name of the provider.
    fn name(&self) -> String;

    /// Returns the base URL of the provider.
    fn base_url(&self) -> Url;

    /// Returns a permalink to a Git commit on this hosting provider.
    fn build_commit_permalink(
        &self,
        remote: &ParsedGitRemote,
        params: BuildCommitPermalinkParams,
    ) -> Url;

    /// Returns a permalink to a file and/or selection on this hosting provider.
    fn build_permalink(&self, remote: ParsedGitRemote, params: BuildPermalinkParams) -> Url;

    /// Returns a URL to create a pull request on this hosting provider.
    fn build_create_pull_request_url(
        &self,
        _remote: &ParsedGitRemote,
        _source_branch: &str,
    ) -> Option<Url> {
        None
    }

    /// Returns whether this provider supports avatars.
    fn supports_avatars(&self) -> bool;

    /// Returns a URL fragment to the given line selection.
    fn line_fragment(&self, selection: &Range<u32>) -> String {
        if selection.start == selection.end {
            let line = selection.start + 1;

            self.format_line_number(line)
        } else {
            let start_line = selection.start + 1;
            let end_line = selection.end + 1;

            self.format_line_numbers(start_line, end_line)
        }
    }

    /// Returns a formatted line number to be placed in a permalink URL.
    fn format_line_number(&self, line: u32) -> String;

    /// Returns a formatted range of line numbers to be placed in a permalink URL.
    fn format_line_numbers(&self, start_line: u32, end_line: u32) -> String;

    fn parse_remote_url(&self, url: &str) -> Option<ParsedGitRemote>;

    fn extract_pull_request(
        &self,
        _remote: &ParsedGitRemote,
        _message: &str,
    ) -> Option<PullRequest> {
        None
    }

    async fn commit_author_avatar_url(
        &self,
        _repo_owner: &str,
        _repo: &str,
        _commit: SharedString,
        _author_email: Option<SharedString>,
        _http_client: Arc<dyn HttpClient>,
    ) -> Result<Option<Url>> {
        Ok(None)
    }

    /// List pull requests on the host for the given remote. The default
    /// implementation returns an empty list — providers that support the
    /// concept (GitHub, GitLab, Bitbucket, etc.) override it with the relevant
    /// HTTP call. Errors surface as `Err`; callers typically render them
    /// inline in the PR panel.
    async fn list_pull_requests(
        &self,
        _remote: &ParsedGitRemote,
        _filter: PullRequestListFilter,
        _http_client: Arc<dyn HttpClient>,
    ) -> Result<Vec<PullRequestSummary>> {
        Ok(Vec::new())
    }

    /// Fetch the full detail for a single pull request by number. Default
    /// implementation reports "not supported" so the PR panel can fall back to
    /// just opening the host URL.
    async fn get_pull_request(
        &self,
        _remote: &ParsedGitRemote,
        _number: u32,
        _http_client: Arc<dyn HttpClient>,
    ) -> Result<PullRequestDetail> {
        anyhow::bail!("pull request detail not supported by this hosting provider")
    }

    /// Fetch the unified-diff representation of a pull request. Returned as a
    /// raw string so callers can hand it to the existing diff plumbing without
    /// imposing a structured shape here.
    async fn get_pull_request_diff(
        &self,
        _remote: &ParsedGitRemote,
        _number: u32,
        _http_client: Arc<dyn HttpClient>,
    ) -> Result<String> {
        anyhow::bail!("pull request diff not supported by this hosting provider")
    }

    /// Fetch every inline review comment on a pull request.
    async fn get_pull_request_comments(
        &self,
        _remote: &ParsedGitRemote,
        _number: u32,
        _http_client: Arc<dyn HttpClient>,
    ) -> Result<Vec<PullRequestReviewComment>> {
        Ok(Vec::new())
    }

    /// Combine the pull request into its target branch using the host's API.
    async fn merge_pull_request(
        &self,
        _remote: &ParsedGitRemote,
        _number: u32,
        _method: PullRequestMergeMethod,
        _http_client: Arc<dyn HttpClient>,
    ) -> Result<()> {
        anyhow::bail!("merging pull requests is not supported by this hosting provider")
    }

    /// Submit a review with the given verdict, optionally accompanied by a
    /// summary body. Hosts that don't model reviews (e.g. unauthenticated
    /// clients) return an error here so the UI can render a clear toast.
    async fn submit_review(
        &self,
        _remote: &ParsedGitRemote,
        _number: u32,
        _verdict: PullRequestReviewVerdict,
        _body: Option<SharedString>,
        _http_client: Arc<dyn HttpClient>,
    ) -> Result<()> {
        anyhow::bail!("submitting reviews is not supported by this hosting provider")
    }

    /// Post a reply to an existing inline review comment. `in_reply_to` is the
    /// id of the parent `PullRequestReviewComment`. Hosts that don't model
    /// threaded review replies report "not supported".
    async fn post_review_comment(
        &self,
        _remote: &ParsedGitRemote,
        _number: u32,
        _in_reply_to: u64,
        _body: SharedString,
        _http_client: Arc<dyn HttpClient>,
    ) -> Result<()> {
        anyhow::bail!("posting review comments is not supported by this hosting provider")
    }
}

#[derive(Default, Deref, DerefMut)]
struct GlobalGitHostingProviderRegistry(Arc<GitHostingProviderRegistry>);

impl Global for GlobalGitHostingProviderRegistry {}

#[derive(Default)]
struct GitHostingProviderRegistryState {
    default_providers: Vec<Arc<dyn GitHostingProvider + Send + Sync + 'static>>,
    setting_providers: Vec<Arc<dyn GitHostingProvider + Send + Sync + 'static>>,
}

#[derive(Default)]
pub struct GitHostingProviderRegistry {
    state: RwLock<GitHostingProviderRegistryState>,
}

impl GitHostingProviderRegistry {
    /// Returns the global [`GitHostingProviderRegistry`].
    #[track_caller]
    pub fn global(cx: &App) -> Arc<Self> {
        cx.global::<GlobalGitHostingProviderRegistry>().0.clone()
    }

    /// Returns the global [`GitHostingProviderRegistry`], if one is set.
    pub fn try_global(cx: &App) -> Option<Arc<Self>> {
        cx.try_global::<GlobalGitHostingProviderRegistry>()
            .map(|registry| registry.0.clone())
    }

    /// Returns the global [`GitHostingProviderRegistry`].
    ///
    /// Inserts a default [`GitHostingProviderRegistry`] if one does not yet exist.
    pub fn default_global(cx: &mut App) -> Arc<Self> {
        cx.default_global::<GlobalGitHostingProviderRegistry>()
            .0
            .clone()
    }

    /// Sets the global [`GitHostingProviderRegistry`].
    pub fn set_global(registry: Arc<GitHostingProviderRegistry>, cx: &mut App) {
        cx.set_global(GlobalGitHostingProviderRegistry(registry));
    }

    /// Returns a new [`GitHostingProviderRegistry`].
    pub fn new() -> Self {
        Self {
            state: RwLock::new(GitHostingProviderRegistryState {
                setting_providers: Vec::default(),
                default_providers: Vec::default(),
            }),
        }
    }

    /// Returns the list of all [`GitHostingProvider`]s in the registry.
    pub fn list_hosting_providers(
        &self,
    ) -> Vec<Arc<dyn GitHostingProvider + Send + Sync + 'static>> {
        let state = self.state.read();
        state
            .default_providers
            .iter()
            .cloned()
            .chain(state.setting_providers.iter().cloned())
            .collect()
    }

    pub fn set_setting_providers(
        &self,
        providers: impl IntoIterator<Item = Arc<dyn GitHostingProvider + Send + Sync + 'static>>,
    ) {
        let mut state = self.state.write();
        state.setting_providers.clear();
        state.setting_providers.extend(providers);
    }

    /// Adds the provided [`GitHostingProvider`] to the registry.
    pub fn register_hosting_provider(
        &self,
        provider: Arc<dyn GitHostingProvider + Send + Sync + 'static>,
    ) {
        self.state.write().default_providers.push(provider);
    }
}

#[derive(Debug, PartialEq)]
pub struct ParsedGitRemote {
    pub owner: Arc<str>,
    pub repo: Arc<str>,
}

pub fn parse_git_remote_url(
    provider_registry: Arc<GitHostingProviderRegistry>,
    url: &str,
) -> Option<(
    Arc<dyn GitHostingProvider + Send + Sync + 'static>,
    ParsedGitRemote,
)> {
    provider_registry
        .list_hosting_providers()
        .into_iter()
        .find_map(|provider| {
            provider
                .parse_remote_url(url)
                .map(|parsed_remote| (provider, parsed_remote))
        })
}
