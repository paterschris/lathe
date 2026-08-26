//! Pull-request data types shared across the git hosting providers.
//!
//! Lathe-added for the PR-review feature: these describe pull requests, their
//! reviewers and review comments, merge methods, and host authentication. They
//! are re-exported from [`crate::hosting_provider`] (and thus from the `git`
//! crate root) so the `GitHostingProvider` trait methods that return/accept them
//! and cross-crate callers keep their existing `git::...` paths unchanged.

use gpui::SharedString;
use url::Url;

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

/// Everything needed to open a new pull request on a host.
#[derive(Debug, Clone)]
pub struct NewPullRequest {
    pub title: SharedString,
    pub body: SharedString,
    /// Branch carrying the changes.
    pub source_branch: SharedString,
    /// Branch the changes are proposed against.
    pub target_branch: SharedString,
    /// Open as a draft. Hosts that have no draft concept ignore this rather than
    /// failing the call.
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
    /// When `true`, restrict to PRs authored by the authenticated user. The
    /// provider resolves "me" itself (GitHub matches `user.login`; Bitbucket
    /// matches `author.uuid`). Errors when the authenticated user cannot be
    /// determined, mirroring `reviewer_is_me`. Combines with `reviewer_is_me`
    /// as an intersection when both are set.
    pub author_is_me: bool,
    /// Cap on returned PRs. `None` = whatever the provider's default is. When
    /// `page` is set this is the page size.
    pub limit: Option<u32>,
    /// 1-based page of results to fetch, for callers that page through a long
    /// list rather than taking a fixed prefix of it. `None` is the first page.
    ///
    /// Only meaningful for a plain listing: the "mine" filters below resolve the
    /// authenticated user client-side over a wide scan, so their result set does
    /// not correspond to any single page of the host's response and providers
    /// ignore this field for them.
    pub page: Option<u32>,
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
    /// How many commits the PR's target branch is ahead of its source branch,
    /// i.e. how far behind the base the PR has fallen. `Some(0)` means up to
    /// date; `None` when the host hasn't reported it. Providers resolve it with
    /// an extra compare request and leave it `None` on any failure.
    pub behind_by: Option<u32>,
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
    /// CI results for the head commit. Best-effort: providers resolve it with an
    /// extra request and leave it `None` when the host reports nothing or the
    /// call fails, which the header renders as "no checks" rather than as a
    /// failure.
    pub checks: Option<PullRequestChecks>,
}

/// Roll-up of a pull request's CI results, as reported by the host for the head
/// commit. Reviewing without knowing whether the build is green means leaving
/// the editor to find out, so the detail view surfaces this alongside the
/// merge state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestChecks {
    pub succeeded: u32,
    pub failed: u32,
    pub pending: u32,
    /// Checks the host reports as neither passing nor failing (skipped,
    /// cancelled, neutral). Counted separately so they cannot masquerade as
    /// either a pass or a failure.
    pub neutral: u32,
}

impl PullRequestChecks {
    pub fn total(&self) -> u32 {
        self.succeeded + self.failed + self.pending + self.neutral
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    /// The single state that best describes the run as a whole. A failure
    /// dominates anything else, and a pending check outranks success so a
    /// half-finished run never reads as green.
    pub fn overall(&self) -> CheckState {
        if self.failed > 0 {
            CheckState::Failed
        } else if self.pending > 0 {
            CheckState::Pending
        } else if self.succeeded > 0 {
            CheckState::Succeeded
        } else {
            CheckState::Neutral
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    Succeeded,
    Failed,
    Pending,
    Neutral,
}

/// An account the host will accept as a reviewer on this repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewerCandidate {
    /// What [`crate::GitHostingProvider::request_reviewers`] expects back for
    /// this account. Hosts disagree on what identifies a user: Bitbucket wants
    /// an opaque uuid, GitHub a login, GitLab a username. Callers pass this
    /// through untouched rather than trying to guess.
    pub handle: SharedString,
    /// The host's handle for the account, shown as secondary text.
    pub login: SharedString,
    /// Human-readable name, where the host reports one. GitHub's collaborator
    /// listing does not, so it is `None` there.
    pub display_name: Option<SharedString>,
}

impl ReviewerCandidate {
    /// Name to lead with: the full name when known, else the handle.
    pub fn primary_label(&self) -> SharedString {
        self.display_name.clone().unwrap_or_else(|| self.login.clone())
    }

    /// Text a fuzzy match should run against, so typing either a name or a
    /// handle finds the person.
    pub fn match_text(&self) -> String {
        match &self.display_name {
            Some(name) => format!("{name} {}", self.login),
            None => self.login.to_string(),
        }
    }
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

/// The authentication protocol a git host speaks, and therefore which connect
/// flow the UI offers and which [`GitHostAuth`] shape its credential produces.
///
/// Deliberately separate from the host itself: one kind covers the vendor's
/// public instance *and* every enterprise or self-hosted deployment of the same
/// product, so `github.acme.com` authenticates the same way `github.com` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitHostAuthKind {
    GitHub,
    GitLab,
    Bitbucket,
}

impl GitHostAuthKind {
    /// Product name, used in menu entries and connect-modal copy.
    pub fn product_name(self) -> &'static str {
        match self {
            GitHostAuthKind::GitHub => "GitHub",
            GitHostAuthKind::GitLab => "GitLab",
            GitHostAuthKind::Bitbucket => "Bitbucket",
        }
    }

    /// The hostname of the vendor's public instance. A host that differs from
    /// this is an enterprise or self-hosted deployment, which matters because
    /// the GitHub device flow is registered against `github.com` only and such
    /// hosts must authenticate with a personal access token instead.
    pub fn public_host(self) -> &'static str {
        match self {
            GitHostAuthKind::GitHub => "github.com",
            GitHostAuthKind::GitLab => "gitlab.com",
            GitHostAuthKind::Bitbucket => "bitbucket.org",
        }
    }

    /// Whether this kind stores a username alongside the secret. Token-based
    /// hosts do not, so their connect modal shows a single field.
    pub fn needs_username(self) -> bool {
        matches!(self, GitHostAuthKind::Bitbucket)
    }

    /// Builds the API auth value for a stored `(username, secret)` credential.
    /// GitHub and GitLab use bearer tokens; Bitbucket uses HTTP Basic.
    pub fn auth(self, username: String, secret: String) -> GitHostAuth {
        match self {
            GitHostAuthKind::GitHub | GitHostAuthKind::GitLab => GitHostAuth::Bearer(secret),
            GitHostAuthKind::Bitbucket => GitHostAuth::Basic { username, secret },
        }
    }
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
