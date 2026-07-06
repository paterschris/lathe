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
    /// When `true`, restrict to PRs where the authenticated user is a requested
    /// reviewer. The provider resolves "me" itself (GitHub matches the login in
    /// `requested_reviewers`; Bitbucket queries `reviewers.uuid`). Note the
    /// semantics differ slightly per host: GitHub drops a reviewer from
    /// `requested_reviewers` once they submit a review, so this surfaces PRs
    /// still awaiting your review, whereas Bitbucket keeps you in `reviewers`
    /// regardless of whether you have already reviewed.
    pub reviewer_is_me: bool,
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
    /// ISO-8601 timestamp string for when the PR was opened, as returned by the
    /// host. The detail view renders it as a calendar date in the header.
    pub created_at: SharedString,
    pub updated_at: SharedString,
    pub is_draft: bool,
    /// `Some(true)` if the host can fast-forward or auto-merge; `Some(false)`
    /// if there's a known conflict; `None` if the host hasn't computed it yet.
    pub is_mergeable: Option<bool>,
    pub additions: u32,
    pub deletions: u32,
    pub changed_files: u32,
    /// Number of commits on the PR when the host reports it. `None` when the
    /// host doesn't expose a count; the header omits the commit chip in that case.
    pub commits: Option<u32>,
    /// The authenticated user's own current review on this PR, when known.
    /// `Some(Approve)` if they have approved, `Some(RequestChanges)` if they
    /// have a blocking review, `Some(Comment)` for a comment-only review, and
    /// `None` when they have not reviewed (or the host didn't report it). Lets
    /// the detail view reflect what the viewer has already done rather than
    /// always offering a fresh Approve / Request changes. Best-effort: providers
    /// resolve it with extra requests and leave it `None` on any failure.
    pub viewer_review: Option<PullRequestReviewVerdict>,
    /// All reviewers on the PR and their latest verdict. `verdict: None` means a
    /// requested reviewer who has not submitted a review yet (pending). Best-effort:
    /// providers populate it where the host exposes it and leave it empty on failure.
    pub reviewers: Vec<PullRequestReviewer>,
}

/// A reviewer on a pull request and their latest review state.
#[derive(Debug, Clone)]
pub struct PullRequestReviewer {
    pub login: SharedString,
    /// Latest verdict, or `None` if requested but not yet reviewed (pending).
    pub verdict: Option<PullRequestReviewVerdict>,
    /// True when this reviewer is the authenticated user.
    pub is_me: bool,
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
    /// For threaded review replies, the id of the comment this one replies to
    /// (`in_reply_to_id` on GitHub, `parent.id` on Bitbucket). `None` for the
    /// top-level comment of a thread. Used to group comments into threads and
    /// indent replies in the diff view.
    pub parent_id: Option<u64>,
    /// Whether this comment's thread is marked resolved on the host. Only the
    /// thread's root comment carries this. GitHub's REST API does not expose it,
    /// so it is always `false` there.
    pub is_resolved: bool,
    pub created_at: SharedString,
    pub url: Url,
}

/// Authentication material for a hosting provider's REST API.
///
/// Resolved per host by the caller (which has keychain access via
/// [`crate::git_host_credentials`]) and threaded into the pull-request methods
/// below. `None` means no stored credential is available, in which case a
/// provider may fall back to an environment token (e.g. `GITHUB_TOKEN`) or make
/// an unauthenticated request.
#[derive(Clone)]
pub enum GitHostAuth {
    /// `Authorization: Bearer <token>` — GitHub OAuth/device tokens and PATs,
    /// or Bitbucket Cloud OAuth access tokens.
    Bearer(String),
    /// HTTP Basic credentials — Bitbucket Cloud username + App Password (or
    /// Atlassian account email + API token).
    Basic { username: String, secret: String },
}

impl std::fmt::Debug for GitHostAuth {
    /// Redacts secrets so a credential can never be leaked through a `{:?}`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitHostAuth::Bearer(_) => f.write_str("GitHostAuth::Bearer(<redacted>)"),
            GitHostAuth::Basic { username, .. } => f
                .debug_struct("GitHostAuth::Basic")
                .field("username", username)
                .field("secret", &"<redacted>")
                .finish(),
        }
    }
}

/// Returned by pull-request operations when the host rejects the supplied
/// credential with HTTP 401 (an expired or revoked token, or a wrong app
/// password). Carries the canonical host so the UI can offer a targeted
/// reconnect for that account, rather than the generic error shown for other
/// 4xx responses (rate limits, permission denials) that reconnecting cannot fix.
#[derive(Debug, Clone)]
pub struct PullRequestAuthError {
    pub host: SharedString,
}

impl std::fmt::Display for PullRequestAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "authentication with {} failed; the stored credential is expired or invalid",
            self.host
        )
    }
}

impl std::error::Error for PullRequestAuthError {}

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
        _auth: Option<GitHostAuth>,
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
        _auth: Option<GitHostAuth>,
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
        _auth: Option<GitHostAuth>,
        _http_client: Arc<dyn HttpClient>,
    ) -> Result<String> {
        anyhow::bail!("pull request diff not supported by this hosting provider")
    }

    /// Fetch every inline review comment on a pull request.
    async fn get_pull_request_comments(
        &self,
        _remote: &ParsedGitRemote,
        _number: u32,
        _auth: Option<GitHostAuth>,
        _http_client: Arc<dyn HttpClient>,
    ) -> Result<Vec<PullRequestReviewComment>> {
        Ok(Vec::new())
    }

    /// Fetch the full UTF-8 content of a file at a given revision (commit SHA or
    /// ref). The PR view uses this to anchor review comments to their exact line
    /// even when that line is unchanged and so absent from the diff.
    async fn get_file_content(
        &self,
        _remote: &ParsedGitRemote,
        _path: &str,
        _revision: &str,
        _auth: Option<GitHostAuth>,
        _http_client: Arc<dyn HttpClient>,
    ) -> Result<String> {
        anyhow::bail!("fetching file content is not supported by this hosting provider")
    }

    /// Combine the pull request into its target branch using the host's API.
    async fn merge_pull_request(
        &self,
        _remote: &ParsedGitRemote,
        _number: u32,
        _method: PullRequestMergeMethod,
        _auth: Option<GitHostAuth>,
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
        _auth: Option<GitHostAuth>,
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
        _auth: Option<GitHostAuth>,
        _http_client: Arc<dyn HttpClient>,
    ) -> Result<()> {
        anyhow::bail!("posting review comments is not supported by this hosting provider")
    }

    /// Create a NEW top-level inline review comment on a diff line (as opposed to
    /// `post_review_comment`, which replies to an existing thread). `line` is the
    /// 1-based line number on `side` of the diff for `path`; `commit_id` is the
    /// head commit the comment anchors to (required by GitHub, ignored by hosts
    /// that anchor by line alone). Hosts without inline review comments report
    /// "not supported".
    async fn create_review_comment(
        &self,
        _remote: &ParsedGitRemote,
        _number: u32,
        _commit_id: SharedString,
        _path: SharedString,
        _line: u32,
        _side: DiffCommentSide,
        _body: SharedString,
        _auth: Option<GitHostAuth>,
        _http_client: Arc<dyn HttpClient>,
    ) -> Result<()> {
        anyhow::bail!("creating review comments is not supported by this hosting provider")
    }

    /// The authenticated user's own latest review verdict on a single PR, or
    /// `None` if they haven't reviewed it. Lighter than `get_pull_request`: used
    /// to tint already-reviewed rows in the PR list. Hosts that can't report it
    /// return `Ok(None)`.
    async fn pull_request_review_state(
        &self,
        _remote: &ParsedGitRemote,
        _number: u32,
        _auth: Option<GitHostAuth>,
        _http_client: Arc<dyn HttpClient>,
    ) -> Result<Option<PullRequestReviewVerdict>> {
        Ok(None)
    }
}

/// Which side of a diff an inline review comment anchors to. `Right` is the
/// post-image (added / context lines); `Left` is the pre-image (deleted lines).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffCommentSide {
    Right,
    Left,
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
