//! Pull requests for paired phones: lists, details, diffs and reviews for the
//! repositories in open workspaces, through the accounts connected in Lathe,
//! the same way the pull request panel loads them. No host token leaves Lathe.

use crate::{
    bridge::open_workspace,
    crypto::encode_url_safe,
    protocol::{
        DesktopMessage, DiffSide, ErrorBody, ErrorCode, PhoneMessage, PullRequestChecks,
        PullRequestDetail, PullRequestFile, PullRequestFilter, PullRequestRepo,
        PullRequestReviewer, PullRequestState, PullRequestSummary, RepoStatus, ReviewComment,
        ReviewVerdict,
    },
};
use collections::HashMap;
use futures::FutureExt as _;
use git::{GitHostAuth, GitHostingProvider, GitHostingProviderRegistry, ParsedGitRemote};
use gpui::{
    App, AsyncApp, BackgroundExecutor, Entity, EntityId, SharedString, Task,
    http_client::HttpClient,
};
use pr_ui::pull_request_loader::{
    LoadOutcome, failure_summary, load_pull_requests, pull_request_repositories, remote_candidates,
};
use project::git_store::Repository;
use sha2::{Digest as _, Sha256};
use std::{sync::Arc, time::Duration};
use util::ResultExt as _;

/// Room for a message's own fields around what it carries.
const FRAME_OVERHEAD_BYTES: usize = 1024;
/// A description longer than this is cut; the rest is on the host.
const MAX_BODY_CHARS: usize = 32 * 1024;
/// The longest host error passed on to the phone.
const MAX_ERROR_CHARS: usize = 300;
const NO_HOSTS: &str = "Lathe has no git hosts set up.";
/// How long a list waits for reviewers before going without them.
const REVIEWER_TIME_LIMIT: Duration = Duration::from_secs(4);
/// Reviewer requests in flight at once, as in the panel.
const REVIEWER_BATCH: usize = 5;

fn frame_budget() -> usize {
    crate::companion::MAX_FRAME_PLAINTEXT_BYTES - FRAME_OVERHEAD_BYTES
}

/// Answers one of the pull request messages with the messages to send before
/// its ack, or the error to ack with. Any other message is a bug in the caller.
pub(crate) fn handle(
    message: PhoneMessage,
    cx: &mut App,
) -> Task<Result<Vec<DesktopMessage>, ErrorBody>> {
    // Absent only without the git hosting setup (tests); every host call then fails.
    let registry = GitHostingProviderRegistry::try_global(cx);
    let http_client = cx.http_client();
    match message {
        PhoneMessage::ListPullRequests {
            workspace_id,
            filter,
            reviewing,
            repo_id,
            page,
        } => {
            let repos = match workspace_repos(&workspace_id, cx) {
                Ok(repos) => repos,
                Err(error) => return Task::ready(Err(error)),
            };
            let repos = match &repo_id {
                Some(repo_id) => match repos.into_iter().find(|repo| &repo.repo_id == repo_id) {
                    Some(repo) => vec![repo],
                    None => return Task::ready(Err(repo_not_found())),
                },
                None => repos,
            };
            let page = page.unwrap_or(1).max(1);
            // One task per repository, so a slow host doesn't hold up the rest.
            let tasks = repos
                .into_iter()
                .map(|repo| {
                    let registry = registry.clone();
                    let http_client = http_client.clone();
                    cx.spawn(async move |cx| {
                        list_repo(
                            repo,
                            registry.as_ref(),
                            filter,
                            reviewing,
                            page,
                            &http_client,
                            cx,
                        )
                        .await
                    })
                })
                .collect::<Vec<_>>();
            cx.spawn(async move |_| {
                let listed = futures::future::join_all(tasks).await;
                Ok(list_parts(&workspace_id, filter, reviewing, listed))
            })
        }
        PhoneMessage::GetPullRequest {
            workspace_id,
            repo_id,
            number,
        } => {
            let repo = match find_repo(&workspace_id, &repo_id, cx) {
                Ok(repo) => repo,
                Err(error) => return Task::ready(Err(error)),
            };
            cx.spawn(async move |cx| {
                let host = connect(&repo, registry.as_ref(), &http_client, cx).await?;
                let detail = host
                    .provider
                    .get_pull_request(&host.remote, number, host.auth.clone(), http_client.clone())
                    .await
                    .map_err(host_error)?;
                // Comments are extra; a host that fails them still shows the pull request.
                let comments = host
                    .provider
                    .get_pull_request_comments(&host.remote, number, host.auth, http_client)
                    .await
                    .log_err()
                    .unwrap_or_default();
                Ok(vec![detail_message(
                    workspace_id,
                    repo_id,
                    detail,
                    comments,
                )])
            })
        }
        PhoneMessage::GetPullRequestDiff {
            workspace_id,
            repo_id,
            number,
        } => {
            let repo = match find_repo(&workspace_id, &repo_id, cx) {
                Ok(repo) => repo,
                Err(error) => return Task::ready(Err(error)),
            };
            cx.spawn(async move |cx| {
                let host = connect(&repo, registry.as_ref(), &http_client, cx).await?;
                let diff = host
                    .provider
                    .get_pull_request_diff(&host.remote, number, host.auth, http_client)
                    .await
                    .map_err(host_error)?;
                let files = split_diff(&diff);
                Ok(diff_parts(files, frame_budget())
                    .into_iter()
                    .enumerate()
                    .map(|(part, (files, last))| DesktopMessage::PullRequestDiff {
                        workspace_id: workspace_id.clone(),
                        repo_id: repo_id.clone(),
                        number,
                        part: part as u32,
                        last,
                        files,
                    })
                    .collect())
            })
        }
        PhoneMessage::SubmitReview {
            workspace_id,
            repo_id,
            number,
            verdict,
            body,
        } => {
            let repo = match find_repo(&workspace_id, &repo_id, cx) {
                Ok(repo) => repo,
                Err(error) => return Task::ready(Err(error)),
            };
            let body = body
                .map(|body| body.trim().to_string())
                .filter(|body| !body.is_empty());
            cx.spawn(async move |cx| {
                let host = connect(&repo, registry.as_ref(), &http_client, cx).await?;
                host.provider
                    .submit_review(
                        &host.remote,
                        number,
                        host_verdict(verdict),
                        body.map(SharedString::from),
                        host.auth,
                        http_client,
                    )
                    .await
                    .map_err(host_error)?;
                Ok(Vec::new())
            })
        }
        PhoneMessage::ReplyReviewComment {
            workspace_id,
            repo_id,
            number,
            comment_id,
            body,
        } => {
            let Ok(comment_id) = comment_id.parse::<u64>() else {
                return Task::ready(Err(ErrorBody::new(
                    ErrorCode::BadMessage,
                    "That comment id isn't one Lathe gave out.",
                )));
            };
            let body = body.trim().to_string();
            if body.is_empty() {
                return Task::ready(Err(ErrorBody::new(
                    ErrorCode::BadMessage,
                    "A reply needs some text.",
                )));
            }
            let repo = match find_repo(&workspace_id, &repo_id, cx) {
                Ok(repo) => repo,
                Err(error) => return Task::ready(Err(error)),
            };
            cx.spawn(async move |cx| {
                let host = connect(&repo, registry.as_ref(), &http_client, cx).await?;
                host.provider
                    .post_review_comment(
                        &host.remote,
                        number,
                        comment_id,
                        body.into(),
                        host.auth,
                        http_client,
                    )
                    .await
                    .map_err(host_error)?;
                Ok(Vec::new())
            })
        }
        PhoneMessage::CreateReviewComment {
            workspace_id,
            repo_id,
            number,
            path,
            line,
            side,
            body,
        } => {
            let body = body.trim().to_string();
            if body.is_empty() || path.is_empty() || line == 0 {
                return Task::ready(Err(ErrorBody::new(
                    ErrorCode::BadMessage,
                    "An inline comment needs a file, a line and some text.",
                )));
            }
            let repo = match find_repo(&workspace_id, &repo_id, cx) {
                Ok(repo) => repo,
                Err(error) => return Task::ready(Err(error)),
            };
            cx.spawn(async move |cx| {
                let host = connect(&repo, registry.as_ref(), &http_client, cx).await?;
                // Anchor to the head as the host sees it now, not as the phone last
                // loaded it: GitHub refuses a comment on a commit that isn't the head's.
                let head = host
                    .provider
                    .get_pull_request(&host.remote, number, host.auth.clone(), http_client.clone())
                    .await
                    .map_err(host_error)?
                    .head_sha;
                host.provider
                    .create_review_comment(
                        &host.remote,
                        number,
                        head,
                        path.into(),
                        line,
                        match side {
                            DiffSide::New => git::DiffCommentSide::Right,
                            DiffSide::Old => git::DiffCommentSide::Left,
                        },
                        body.into(),
                        host.auth,
                        http_client,
                    )
                    .await
                    .map_err(host_error)?;
                Ok(Vec::new())
            })
        }
        _ => Task::ready(Err(ErrorBody::new(
            ErrorCode::Internal,
            "Not a pull request message.",
        ))),
    }
}

/// A repository of an open workspace, captured on the main thread for a
/// background load.
struct RepoTarget {
    repo_id: String,
    name: String,
    candidates: Vec<String>,
    /// Picks the account among several connected to the same host.
    workspace: EntityId,
}

fn workspace_repos(workspace_id: &str, cx: &App) -> Result<Vec<RepoTarget>, ErrorBody> {
    let workspace = open_workspace(workspace_id, cx).ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::WorkspaceNotOpen,
            "Open this project in Lathe to see its pull requests.",
        )
    })?;
    let workspace_entity = workspace.entity_id();
    let project = workspace.read(cx).project().read(cx);
    Ok(pull_request_repositories(project, cx)
        .into_iter()
        .map(|entry| {
            let repository = entry.repository.read(cx);
            RepoTarget {
                repo_id: repo_id(entry.identity.as_deref(), &entry.repository, cx),
                name: entry.label.to_string(),
                candidates: remote_candidates(repository),
                workspace: workspace_entity,
            }
        })
        .collect())
}

fn find_repo(workspace_id: &str, repo_id: &str, cx: &App) -> Result<RepoTarget, ErrorBody> {
    workspace_repos(workspace_id, cx)?
        .into_iter()
        .find(|repo| repo.repo_id == repo_id)
        .ok_or_else(repo_not_found)
}

fn repo_not_found() -> ErrorBody {
    ErrorBody::new(
        ErrorCode::RepoNotFound,
        "That repository isn't open in Lathe any more.",
    )
}

/// Stable while the repository stays where it is, so a list loaded before a
/// Lathe restart still opens. Linked worktrees share their checkout's id.
fn repo_id(
    identity: Option<&std::path::Path>,
    repository: &Entity<Repository>,
    cx: &App,
) -> String {
    let path = match identity {
        Some(identity) => identity.to_path_buf(),
        None => repository
            .read(cx)
            .snapshot()
            .work_directory_abs_path
            .to_path_buf(),
    };
    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    encode_url_safe(&hasher.finalize()[..12])
}

async fn list_repo(
    repo: RepoTarget,
    registry: Option<&Arc<GitHostingProviderRegistry>>,
    filter: PullRequestFilter,
    reviewing: bool,
    page: u32,
    http_client: &Arc<dyn HttpClient>,
    cx: &mut AsyncApp,
) -> PullRequestRepo {
    let states = match filter {
        PullRequestFilter::Open => Some(vec![git::PullRequestState::Open]),
        PullRequestFilter::Closed => Some(vec![git::PullRequestState::Closed]),
        PullRequestFilter::Merged => Some(vec![git::PullRequestState::Merged]),
        PullRequestFilter::All => None,
    };
    let mut listed = PullRequestRepo {
        repo_id: repo.repo_id,
        name: repo.name,
        host: None,
        status: RepoStatus::Loaded,
        message: None,
        authored: Vec::new(),
        others: Vec::new(),
        page,
        may_have_more: false,
    };
    if repo.candidates.is_empty() {
        listed.status = RepoStatus::NoHost;
        listed.message = Some(
            "This repository has no origin or upstream remote, so there is no host to ask.".into(),
        );
        return listed;
    }
    let Some(registry) = registry else {
        listed.status = RepoStatus::Failed;
        listed.message = Some(NO_HOSTS.into());
        return listed;
    };
    let result = load_pull_requests(
        repo.candidates,
        registry.clone(),
        states,
        reviewing,
        page,
        http_client.clone(),
        repo.workspace,
        cx,
    )
    .await;
    match result {
        Ok(LoadOutcome::Loaded {
            provider,
            remote,
            loaded,
        }) => {
            listed.host = Some(provider.name());
            listed.authored = loaded.authored.iter().map(summary).collect();
            listed.others = loaded.others.iter().map(summary).collect();
            listed.may_have_more = loaded.may_have_more;
            let numbers = listed
                .authored
                .iter()
                .chain(&listed.others)
                .map(|pull| pull.number)
                .collect();
            let mut reviewers = fetch_reviewers(
                provider,
                remote,
                repo.workspace,
                numbers,
                http_client.clone(),
                cx,
            )
            .await;
            for pull in listed.authored.iter_mut().chain(listed.others.iter_mut()) {
                pull.reviewers = reviewers.remove(&pull.number);
            }
        }
        Ok(LoadOutcome::NoHost(reason)) => {
            listed.status = RepoStatus::NoHost;
            listed.message = Some(reason.to_string());
        }
        Err(error) => match error.downcast_ref::<git::PullRequestAuthError>() {
            Some(auth_error) => {
                listed.status = RepoStatus::AuthExpired;
                listed.message = Some(expired_message(&auth_error.host));
            }
            None => {
                log::warn!("companion: listing pull requests failed: {error:#}");
                listed.status = RepoStatus::Failed;
                listed.message = Some(failure_summary(&error).to_string());
            }
        },
    }
    listed
}

/// Reviewers for each pull request in a list, like the panel's row badges.
async fn fetch_reviewers(
    provider: Arc<dyn GitHostingProvider + Send + Sync>,
    remote: ParsedGitRemote,
    workspace: EntityId,
    numbers: Vec<u32>,
    http_client: Arc<dyn HttpClient>,
    cx: &mut AsyncApp,
) -> HashMap<u32, Vec<PullRequestReviewer>> {
    if numbers.is_empty() {
        return HashMap::default();
    }
    let auth = git::git_host_credentials::auth_for_repository(
        cx,
        Some(workspace),
        provider.clone(),
        &remote,
        http_client.clone(),
    )
    .await
    .ok()
    .flatten();
    collect_reviewers(
        provider,
        remote,
        auth,
        numbers,
        http_client,
        cx.background_executor().clone(),
        REVIEWER_TIME_LIMIT,
    )
    .await
}

/// Best effort under `limit`: requests go out a few at a time, and any pull
/// request the host hasn't answered for by then is left out.
async fn collect_reviewers(
    provider: Arc<dyn GitHostingProvider + Send + Sync>,
    remote: ParsedGitRemote,
    auth: Option<GitHostAuth>,
    numbers: Vec<u32>,
    http_client: Arc<dyn HttpClient>,
    executor: BackgroundExecutor,
    limit: Duration,
) -> HashMap<u32, Vec<PullRequestReviewer>> {
    let mut found = HashMap::default();
    let deadline = executor.now() + limit;
    for chunk in numbers.chunks(REVIEWER_BATCH) {
        let now = executor.now();
        if now >= deadline {
            break;
        }
        let requests = futures::future::join_all(chunk.iter().map(|&number| {
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
                    .await;
                (number, reviewers)
            }
        }));
        let results = futures::select_biased! {
            results = requests.fuse() => results,
            _ = executor.timer(deadline - now).fuse() => break,
        };
        for (number, reviewers) in results {
            if let Some(reviewers) = reviewers.log_err() {
                found.insert(number, reviewers.iter().map(reviewer).collect());
            }
        }
    }
    found
}

fn reviewer(reviewer: &git::PullRequestReviewer) -> PullRequestReviewer {
    PullRequestReviewer {
        login: reviewer.login.to_string(),
        verdict: reviewer.verdict.map(verdict),
        is_me: reviewer.is_me,
    }
}

/// Splits a list into parts that each fit in a frame. A repository too large
/// for a frame on its own still goes, alone; a page of summaries never is.
fn list_parts(
    workspace_id: &str,
    filter: PullRequestFilter,
    reviewing: bool,
    repos: Vec<PullRequestRepo>,
) -> Vec<DesktopMessage> {
    let budget = frame_budget();
    let mut parts: Vec<Vec<PullRequestRepo>> = vec![Vec::new()];
    let mut used = 0;
    for repo in repos {
        let size = json_size(&repo);
        if used + size > budget && parts.last().is_some_and(|part| !part.is_empty()) {
            parts.push(Vec::new());
            used = 0;
        }
        used += size;
        if let Some(part) = parts.last_mut() {
            part.push(repo);
        }
    }
    let count = parts.len();
    parts
        .into_iter()
        .enumerate()
        .map(|(part, repos)| DesktopMessage::PullRequests {
            workspace_id: workspace_id.to_string(),
            filter,
            reviewing,
            part: part as u32,
            last: part + 1 == count,
            repos,
        })
        .collect()
}

/// The host, remote and account to use for a repository: its first remote on
/// a known host with an account connected in Lathe, like the panel.
struct Host {
    provider: Arc<dyn GitHostingProvider + Send + Sync>,
    remote: ParsedGitRemote,
    auth: Option<GitHostAuth>,
}

async fn connect(
    repo: &RepoTarget,
    registry: Option<&Arc<GitHostingProviderRegistry>>,
    http_client: &Arc<dyn HttpClient>,
    cx: &mut AsyncApp,
) -> Result<Host, ErrorBody> {
    let Some(registry) = registry else {
        return Err(ErrorBody::new(ErrorCode::HostError, NO_HOSTS));
    };
    let mut unconnected = None;
    for remote_url in &repo.candidates {
        let Some((provider, remote)) = git::parse_git_remote_url(registry.clone(), remote_url)
        else {
            continue;
        };
        let auth = git::git_host_credentials::auth_for_repository(
            cx,
            Some(repo.workspace),
            provider.clone(),
            &remote,
            http_client.clone(),
        )
        .await
        .ok()
        .flatten();
        if auth.is_none() {
            unconnected.get_or_insert(provider.name());
            continue;
        }
        return Ok(Host {
            provider,
            remote,
            auth,
        });
    }
    Err(ErrorBody::new(
        ErrorCode::HostError,
        match unconnected {
            Some(host) => format!("Connect {host} in Lathe's account menu first."),
            None => "This repository's remote doesn't match any host Lathe knows.".to_string(),
        },
    ))
}

fn host_error(error: anyhow::Error) -> ErrorBody {
    if let Some(auth_error) = error.downcast_ref::<git::PullRequestAuthError>() {
        return ErrorBody::new(ErrorCode::HostError, expired_message(&auth_error.host));
    }
    log::warn!("companion: pull request request failed: {error:#}");
    let mut message = format!("{error:#}");
    if message.chars().count() > MAX_ERROR_CHARS {
        message = message.chars().take(MAX_ERROR_CHARS).collect::<String>() + "...";
    }
    ErrorBody::new(ErrorCode::HostError, message)
}

fn expired_message(host: &str) -> String {
    format!("The {host} account connected in Lathe has expired. Reconnect it in Lathe.")
}

fn state(state: git::PullRequestState) -> PullRequestState {
    match state {
        git::PullRequestState::Open => PullRequestState::Open,
        git::PullRequestState::Closed => PullRequestState::Closed,
        git::PullRequestState::Merged => PullRequestState::Merged,
    }
}

fn verdict(verdict: git::PullRequestReviewVerdict) -> ReviewVerdict {
    match verdict {
        git::PullRequestReviewVerdict::Approve => ReviewVerdict::Approve,
        git::PullRequestReviewVerdict::RequestChanges => ReviewVerdict::RequestChanges,
        git::PullRequestReviewVerdict::Comment => ReviewVerdict::Comment,
    }
}

fn host_verdict(verdict: ReviewVerdict) -> git::PullRequestReviewVerdict {
    match verdict {
        ReviewVerdict::Approve => git::PullRequestReviewVerdict::Approve,
        ReviewVerdict::RequestChanges => git::PullRequestReviewVerdict::RequestChanges,
        ReviewVerdict::Comment => git::PullRequestReviewVerdict::Comment,
    }
}

fn summary(pull: &git::PullRequestSummary) -> PullRequestSummary {
    PullRequestSummary {
        number: pull.number,
        title: pull.title.to_string(),
        author: pull.author_login.to_string(),
        state: state(pull.state),
        source_branch: pull.source_branch.to_string(),
        target_branch: pull.target_branch.to_string(),
        url: pull.url.to_string(),
        updated_at: pull.updated_at.to_string(),
        is_draft: pull.is_draft,
        reviewers: None,
    }
}

fn detail_message(
    workspace_id: String,
    repo_id: String,
    detail: git::PullRequestDetail,
    comments: Vec<git::PullRequestReviewComment>,
) -> DesktopMessage {
    let body = detail.body.to_string();
    let mut detail = PullRequestDetail {
        summary: PullRequestSummary {
            number: detail.number,
            title: detail.title.to_string(),
            author: detail.author_login.to_string(),
            state: state(detail.state),
            source_branch: detail.source_branch.to_string(),
            target_branch: detail.target_branch.to_string(),
            url: detail.url.to_string(),
            updated_at: detail.updated_at.to_string(),
            is_draft: detail.is_draft,
            reviewers: None,
        },
        body: String::new(),
        created_at: detail.created_at.to_string(),
        head_sha: detail.head_sha.to_string(),
        is_mergeable: detail.is_mergeable,
        additions: detail.additions,
        deletions: detail.deletions,
        changed_files: detail.changed_files,
        commits: detail.commits,
        behind_by: detail.behind_by,
        viewer_review: detail.viewer_review.map(verdict),
        viewer_is_author: detail.viewer_is_author,
        reviewers: detail.reviewers.iter().map(reviewer).collect(),
        checks: detail.checks.map(|checks| PullRequestChecks {
            succeeded: checks.succeeded,
            failed: checks.failed,
            pending: checks.pending,
            neutral: checks.neutral,
        }),
    };
    // The cap counts characters, but escaping can make each one several bytes
    // of JSON, so the description also gives way to keep the detail in a frame
    // with room left for comments.
    let mut keep = MAX_BODY_CHARS;
    loop {
        detail.body = description(&body, keep);
        if json_size(&detail) <= frame_budget() / 2 || keep == 0 {
            break;
        }
        keep /= 2;
    }
    let comments = comments
        .into_iter()
        .map(|comment| ReviewComment {
            comment_id: comment.id.to_string(),
            author: comment.author_login.to_string(),
            body: comment.body.to_string(),
            path: comment.path.to_string(),
            line: comment.line,
            side: comment.side.map(|side| match side {
                git::DiffCommentSide::Right => DiffSide::New,
                git::DiffCommentSide::Left => DiffSide::Old,
            }),
            parent_id: comment.parent_id.map(|id| id.to_string()),
            is_resolved: comment.is_resolved,
            created_at: comment.created_at.to_string(),
            url: comment.url.to_string(),
        })
        .collect::<Vec<_>>();
    let (comments, comments_truncated) =
        fit_comments(comments, frame_budget().saturating_sub(json_size(&detail)));
    DesktopMessage::PullRequest {
        workspace_id,
        repo_id,
        detail,
        comments,
        comments_truncated,
    }
}

/// The first `keep` characters of a description, noting when the rest was cut.
fn description(body: &str, keep: usize) -> String {
    if body.chars().count() <= keep {
        return body.to_string();
    }
    body.chars().take(keep).collect::<String>()
        + "\n\n_The rest of the description is on the host._"
}

/// Keeps the newest comments that fit in `budget`, in their original order.
fn fit_comments(comments: Vec<ReviewComment>, budget: usize) -> (Vec<ReviewComment>, bool) {
    let mut used = 0;
    let mut keep = comments.len();
    for (index, comment) in comments.iter().enumerate().rev() {
        let size = json_size(comment) + 1;
        if used + size > budget {
            break;
        }
        used += size;
        keep = index;
    }
    let truncated = keep > 0;
    (comments.into_iter().skip(keep).collect(), truncated)
}

/// Splits a unified diff into files, keeping each file's hunks from its first
/// `@@` line. Handles `git diff` output and plain `---`/`+++` headers.
fn split_diff(diff: &str) -> Vec<PullRequestFile> {
    let lines: Vec<&str> = diff.lines().collect();
    let git_format = lines.iter().any(|line| line.starts_with("diff --git "));
    let mut files = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let starts_file = |i: usize| {
            if git_format {
                lines[i].starts_with("diff --git ")
            } else {
                lines[i].starts_with("--- ")
                    && lines
                        .get(i + 1)
                        .is_some_and(|next| next.starts_with("+++ "))
            }
        };
        if !starts_file(index) {
            index += 1;
            continue;
        }
        let start = index;
        index += 1;
        while index < lines.len() && !starts_file(index) {
            index += 1;
        }
        let block = &lines[start..index];
        let header_end = block
            .iter()
            .position(|line| line.starts_with("@@"))
            .unwrap_or(block.len());
        let path = file_path(&block[..header_end]);
        files.push(PullRequestFile {
            path,
            unified_diff: block[header_end..].join("\n"),
            truncated: false,
        });
    }
    files
}

/// The file's path after the change, or before it for a deleted file.
fn file_path(header: &[&str]) -> String {
    let path = |line: &str, prefix: &str| -> Option<String> {
        let path = line.strip_prefix(prefix)?;
        // Git ends a path that contains a space with a tab, so the tab can't be
        // part of the name.
        let path = path.strip_suffix('\t').unwrap_or(path);
        if path == "/dev/null" {
            return None;
        }
        let path = unquote(path);
        Some(
            path.strip_prefix("a/")
                .or_else(|| path.strip_prefix("b/"))
                .unwrap_or(&path)
                .to_string(),
        )
    };
    let find = |prefix: &str| header.iter().find_map(|line| path(line, prefix));
    // A rename or copy without content changes has no `---`/`+++` lines.
    if let Some(path) = find("+++ ")
        .or_else(|| find("rename to "))
        .or_else(|| find("copy to "))
        .or_else(|| find("--- "))
    {
        return path;
    }
    // A binary file has only `diff --git a/x b/y`.
    header
        .first()
        .and_then(|line| line.strip_prefix("diff --git "))
        .map(git_line_new_path)
        .unwrap_or_default()
}

/// The new path from the rest of a `diff --git` line. Unquoted paths can
/// contain ` b/`, so when both sides name the same file (every case without a
/// rename, which has its own line) the line is split in half.
fn git_line_new_path(rest: &str) -> String {
    if rest.starts_with('"') {
        if let Some(index) = rest.rfind(" \"b/") {
            return unquote(&rest[index + 1..])
                .trim_start_matches("b/")
                .to_string();
        }
    }
    if let Some(both) = rest.strip_prefix("a/")
        && both.len() >= 3
        && (both.len() - 3).is_multiple_of(2)
    {
        let half = (both.len() - 3) / 2;
        if both.is_char_boundary(half)
            && both[half..].starts_with(" b/")
            && both[..half] == both[half + 3..]
        {
            return both[..half].to_string();
        }
    }
    rest.rsplit_once(" b/")
        .map(|(_, new)| unquote(new))
        .unwrap_or_default()
}

/// Reverses git's C-style quoting of paths with special characters:
/// `"caf\303\251.txt"` is `café.txt`. Unquoted paths come back unchanged.
fn unquote(path: &str) -> String {
    let Some(inner) = path
        .strip_prefix('"')
        .and_then(|path| path.strip_suffix('"'))
    else {
        return path.to_string();
    };
    let mut bytes = Vec::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(char) = chars.next() {
        if char != '\\' {
            let mut buffer = [0u8; 4];
            bytes.extend_from_slice(char.encode_utf8(&mut buffer).as_bytes());
            continue;
        }
        match chars.next() {
            Some('n') => bytes.push(b'\n'),
            Some('t') => bytes.push(b'\t'),
            Some('r') => bytes.push(b'\r'),
            Some('a') => bytes.push(0x07),
            Some('b') => bytes.push(0x08),
            Some('f') => bytes.push(0x0c),
            Some('v') => bytes.push(0x0b),
            Some(digit @ '0'..='7') => {
                let mut value = digit.to_digit(8).unwrap_or(0);
                for _ in 0..2 {
                    match chars.peek().and_then(|next| next.to_digit(8)) {
                        Some(next) => {
                            value = value * 8 + next;
                            chars.next();
                        }
                        None => break,
                    }
                }
                bytes.push(value as u8);
            }
            Some(other) => {
                let mut buffer = [0u8; 4];
                bytes.extend_from_slice(other.encode_utf8(&mut buffer).as_bytes());
            }
            None => bytes.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Packs files into parts that each fit in `budget`, cutting a file that is
/// too large on its own. Always at least one part, so an empty diff still
/// answers. Each part comes with whether it is the last.
fn diff_parts(files: Vec<PullRequestFile>, budget: usize) -> Vec<(Vec<PullRequestFile>, bool)> {
    let mut parts: Vec<Vec<PullRequestFile>> = vec![Vec::new()];
    let mut used = 0;
    for mut file in files {
        let mut size = json_size(&file) + 1;
        if size > budget {
            cut_to_fit(&mut file, budget);
            size = json_size(&file) + 1;
        }
        if used + size > budget && parts.last().is_some_and(|part| !part.is_empty()) {
            parts.push(Vec::new());
            used = 0;
        }
        used += size;
        if let Some(part) = parts.last_mut() {
            part.push(file);
        }
    }
    let count = parts.len();
    parts
        .into_iter()
        .enumerate()
        .map(|(index, files)| (files, index + 1 == count))
        .collect()
}

/// Drops whole lines from the end of a file's diff until it fits.
fn cut_to_fit(file: &mut PullRequestFile, budget: usize) {
    file.truncated = true;
    let mut lines: Vec<&str> = file.unified_diff.lines().collect();
    // JSON escaping can grow a line, so measure rather than count bytes.
    let fixed = json_size(&PullRequestFile {
        path: file.path.clone(),
        unified_diff: String::new(),
        truncated: true,
    }) + 1;
    let mut used = fixed;
    let mut keep = 0;
    for line in &lines {
        let size = serde_json::to_string(line).map_or(line.len(), |json| json.len());
        if used + size > budget {
            break;
        }
        used += size;
        keep += 1;
    }
    lines.truncate(keep);
    file.unified_diff = lines.join("\n");
}

fn json_size<T: serde::Serialize>(value: &T) -> usize {
    serde_json::to_string(value).map_or(0, |json| json.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// Writes one of each pull request message, built by the real conversion
    /// code, for the app's schemas to check, and parses the phone messages
    /// the app wrote:
    /// `LATHE_COMPANION_PR_SAMPLES=<dir> cargo test -p lathe_companion -- --ignored pr_samples`.
    #[test]
    #[ignore]
    fn pr_samples_match_the_app() {
        let Ok(dir) = std::env::var("LATHE_COMPANION_PR_SAMPLES") else {
            return;
        };
        let dir = std::path::Path::new(&dir);
        let summary = |number: u32| PullRequestSummary {
            number,
            title: "Fix the relay".into(),
            author: "octo".into(),
            state: PullRequestState::Open,
            source_branch: "fix".into(),
            target_branch: "main".into(),
            url: "https://github.com/o/r/pull/1".into(),
            updated_at: "2026-10-28T10:00:00Z".into(),
            is_draft: false,
            reviewers: number.is_multiple_of(2).then(|| {
                vec![
                    PullRequestReviewer {
                        login: "me".into(),
                        verdict: None,
                        is_me: true,
                    },
                    PullRequestReviewer {
                        login: "them".into(),
                        verdict: Some(ReviewVerdict::RequestChanges),
                        is_me: false,
                    },
                ]
            }),
        };
        let repo = |status: RepoStatus, message: Option<&str>| PullRequestRepo {
            repo_id: "r1".into(),
            name: "lathe".into(),
            host: matches!(status, RepoStatus::Loaded).then(|| "GitHub".into()),
            status,
            message: message.map(Into::into),
            authored: vec![summary(1)],
            others: vec![summary(2)],
            page: 1,
            may_have_more: true,
        };
        let mut desktop = list_parts(
            "w",
            PullRequestFilter::Open,
            true,
            vec![
                repo(RepoStatus::Loaded, None),
                repo(RepoStatus::NoHost, Some("Connect GitHub")),
                repo(RepoStatus::AuthExpired, Some("Reconnect")),
                repo(RepoStatus::Failed, Some("Could not load")),
            ],
        );
        let url = |path: &str| url::Url::parse(&format!("https://github.com/o/r/{path}")).unwrap();
        let detail = git::PullRequestDetail {
            number: 7,
            title: "Big change".into(),
            body: "x".repeat(MAX_BODY_CHARS + 10).into(),
            state: git::PullRequestState::Merged,
            author_login: "octo".into(),
            source_branch: "feature".into(),
            target_branch: "main".into(),
            head_sha: "abc123".into(),
            base_sha: "def456".into(),
            url: url("pull/7"),
            created_at: "2026-10-27T10:00:00Z".into(),
            updated_at: "2026-10-28T10:00:00Z".into(),
            is_draft: true,
            is_mergeable: Some(false),
            additions: 10,
            deletions: 2,
            changed_files: 3,
            commits: Some(4),
            behind_by: None,
            viewer_review: Some(git::PullRequestReviewVerdict::RequestChanges),
            reviewers: vec![
                git::PullRequestReviewer {
                    login: "me".into(),
                    verdict: None,
                    is_me: true,
                },
                git::PullRequestReviewer {
                    login: "them".into(),
                    verdict: Some(git::PullRequestReviewVerdict::Approve),
                    is_me: false,
                },
            ],
            checks: Some(git::PullRequestChecks {
                succeeded: 3,
                failed: 1,
                pending: 0,
                neutral: 2,
            }),
            viewer_is_author: Some(false),
        };
        let comment = |id: u64| git::PullRequestReviewComment {
            id: u64::MAX - id,
            author_login: "rev".into(),
            body: "y".repeat(2_000).into(),
            path: "src/my file.rs".into(),
            line: if id.is_multiple_of(2) { Some(3) } else { None },
            side: id.is_multiple_of(2).then_some(git::DiffCommentSide::Left),
            parent_id: if id.is_multiple_of(3) {
                Some(u64::MAX)
            } else {
                None
            },
            is_resolved: id.is_multiple_of(5),
            created_at: "2026-10-28T10:00:00Z".into(),
            url: url("pull/7#discussion_r1"),
        };
        desktop.push(detail_message(
            "w".into(),
            "r1".into(),
            detail,
            (0..200).map(comment).collect(),
        ));
        let files =
            split_diff("diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-a\n+b\n");
        for (part, (files, last)) in diff_parts(files, frame_budget()).into_iter().enumerate() {
            desktop.push(DesktopMessage::PullRequestDiff {
                workspace_id: "w".into(),
                repo_id: "r1".into(),
                number: 7,
                part: part as u32,
                last,
                files,
            });
        }
        desktop.push(DesktopMessage::PullRequestDiff {
            workspace_id: "w".into(),
            repo_id: "r1".into(),
            number: 8,
            part: 0,
            last: true,
            files: vec![PullRequestFile {
                path: "huge.rs".into(),
                unified_diff: "@@ -1 +1 @@".into(),
                truncated: true,
            }],
        });
        let lines = desktop
            .iter()
            .map(|message| serde_json::to_string(message).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.join("desktop.jsonl"), lines).unwrap();

        // Phone messages the app's own schemas produced.
        let Ok(phone) = std::fs::read_to_string(dir.join("phone.jsonl")) else {
            return;
        };
        for line in phone.lines().filter(|line| !line.is_empty()) {
            let parsed = serde_json::from_str::<crate::protocol::PhoneEnvelope>(line);
            assert!(parsed.is_ok(), "Lathe can't parse {line}: {parsed:?}");
        }
    }

    /// Answers reviewers for odd numbers after `delay` and never for even ones.
    struct SlowReviewHost {
        executor: BackgroundExecutor,
        delay: Duration,
    }

    #[async_trait::async_trait]
    impl GitHostingProvider for SlowReviewHost {
        fn name(&self) -> String {
            "Slow".into()
        }
        fn base_url(&self) -> url::Url {
            url::Url::parse("https://slow.example").unwrap()
        }
        fn build_commit_permalink(
            &self,
            _: &ParsedGitRemote,
            _: git::BuildCommitPermalinkParams,
        ) -> url::Url {
            self.base_url()
        }
        fn build_permalink(&self, _: ParsedGitRemote, _: git::BuildPermalinkParams) -> url::Url {
            self.base_url()
        }
        fn supports_avatars(&self) -> bool {
            false
        }
        fn format_line_number(&self, line: u32) -> String {
            line.to_string()
        }
        fn format_line_numbers(&self, start: u32, end: u32) -> String {
            format!("{start}-{end}")
        }
        fn parse_remote_url(&self, _: &str) -> Option<ParsedGitRemote> {
            None
        }
        async fn pull_request_reviewers(
            &self,
            _: &ParsedGitRemote,
            number: u32,
            _: Option<GitHostAuth>,
            _: Arc<dyn HttpClient>,
        ) -> anyhow::Result<Vec<git::PullRequestReviewer>> {
            if number.is_multiple_of(2) {
                futures::future::pending::<()>().await;
            }
            self.executor.timer(self.delay).await;
            Ok(vec![git::PullRequestReviewer {
                login: format!("reviewer{number}").into(),
                verdict: Some(git::PullRequestReviewVerdict::Approve),
                is_me: false,
            }])
        }
    }

    #[gpui::test]
    async fn list_reviewers_stop_at_the_time_limit(cx: &mut gpui::TestAppContext) {
        let executor = cx.executor();
        let host = Arc::new(SlowReviewHost {
            executor: executor.clone(),
            delay: Duration::from_millis(100),
        });
        let remote = || ParsedGitRemote {
            owner: "o".into(),
            repo: "r".into(),
        };
        let http_client: Arc<dyn HttpClient> = http_client::FakeHttpClient::with_404_response();

        // Every request answers within the limit.
        let task = executor.spawn(collect_reviewers(
            host.clone(),
            remote(),
            None,
            vec![1, 3, 5, 7, 9, 11, 13],
            http_client.clone(),
            executor.clone(),
            Duration::from_secs(4),
        ));
        executor.advance_clock(Duration::from_secs(1));
        let found = task.await;
        assert_eq!(found.len(), 7);
        assert_eq!(found[&13][0].login, "reviewer13");

        // A batch that never finishes is cut off at the limit, and later
        // batches aren't started; earlier batches' answers are kept.
        let task = executor.spawn(collect_reviewers(
            host,
            remote(),
            None,
            vec![1, 3, 5, 7, 9, 2, 11, 13],
            http_client,
            executor.clone(),
            Duration::from_secs(4),
        ));
        executor.advance_clock(Duration::from_secs(5));
        let found = task.await;
        let mut numbers = found.keys().copied().collect::<Vec<_>>();
        numbers.sort();
        assert_eq!(numbers, vec![1, 3, 5, 7, 9]);
    }

    #[test]
    fn splits_git_diffs_into_files() {
        let diff = "diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-old\n+new\ndiff --git a/gone.txt b/gone.txt\ndeleted file mode 100644\n--- a/gone.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\ndiff --git a/img.png b/img.png\nBinary files differ\n";
        let files = split_diff(diff);
        assert_eq!(
            files
                .iter()
                .map(|file| (file.path.as_str(), file.unified_diff.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("src/a.rs", "@@ -1 +1 @@\n-old\n+new"),
                ("gone.txt", "@@ -1 +0,0 @@\n-bye"),
                ("img.png", "")
            ]
        );
    }

    /// Captured from `git diff -M`, which GitHub and Bitbucket serve as is.
    /// Covers a new file, a deleted file, a binary file, a pure rename, a
    /// missing final newline, and the paths git quotes (non-ASCII, a quote
    /// mark) or ends with a tab (a space, including one before `b/`).
    #[test]
    fn splits_real_git_output() {
        let diff = concat!(
            "diff --git a/added.txt b/added.txt\n",
            "new file mode 100644\n",
            "index 0000000..3e75765\n",
            "--- /dev/null\n",
            "+++ b/added.txt\n",
            "@@ -0,0 +1 @@\n",
            "+new\n",
            "diff --git \"a/caf\\303\\251.txt\" \"b/caf\\303\\251.txt\"\n",
            "index a9074c7..bf7243d 100644\n",
            "--- \"a/caf\\303\\251.txt\"\n",
            "+++ \"b/caf\\303\\251.txt\"\n",
            "@@ -1 +1 @@\n",
            "-caf\n",
            "+café au lait\n",
            "diff --git a/gone.txt b/gone.txt\n",
            "deleted file mode 100644\n",
            "index 3b1bafd..0000000\n",
            "--- a/gone.txt\n",
            "+++ /dev/null\n",
            "@@ -1 +0,0 @@\n",
            "-to delete\n",
            "diff --git a/img.bin b/img.bin\n",
            "index 6772730..9202842 100644\n",
            "Binary files a/img.bin and b/img.bin differ\n",
            "diff --git a/my dir/space file.txt b/my dir/space file.txt\n",
            "index 94954ab..363f0a5 100644\n",
            "--- a/my dir/space file.txt\t\n",
            "+++ b/my dir/space file.txt\t\n",
            "@@ -1,2 +1,3 @@\n",
            " hello\n",
            "+there\n",
            " world\n",
            "diff --git a/nonl.txt b/nonl.txt\n",
            "index 20cbb4d..0ba6e66 100644\n",
            "--- a/nonl.txt\n",
            "+++ b/nonl.txt\n",
            "@@ -1 +1 @@\n",
            "-no newline\n",
            "\\ No newline at end of file\n",
            "+no newline changed\n",
            "\\ No newline at end of file\n",
            "diff --git \"a/quote\\\"d.txt\" \"b/quote\\\"d.txt\"\n",
            "index 587be6b..975fbec 100644\n",
            "--- \"a/quote\\\"d.txt\"\n",
            "+++ \"b/quote\\\"d.txt\"\n",
            "@@ -1 +1 @@\n",
            "-x\n",
            "+y\n",
            "diff --git a/rename-me.txt b/renamed.txt\n",
            "similarity index 100%\n",
            "rename from rename-me.txt\n",
            "rename to renamed.txt\n",
            "diff --git a/weird b/thing.txt b/weird b/thing.txt\n",
            "index 422c2b7..0f7bc76 100644\n",
            "--- a/weird b/thing.txt\t\n",
            "+++ b/weird b/thing.txt\t\n",
            "@@ -1,2 +1,2 @@\n",
            " a\n",
            "-b\n",
            "+c\n",
        );
        let files = split_diff(diff);
        assert_eq!(
            files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec![
                "added.txt",
                "café.txt",
                "gone.txt",
                "img.bin",
                "my dir/space file.txt",
                "nonl.txt",
                "quote\"d.txt",
                "renamed.txt",
                "weird b/thing.txt",
            ]
        );
        let diff_of = |path: &str| {
            files
                .iter()
                .find(|file| file.path == path)
                .map(|file| file.unified_diff.as_str())
                .unwrap()
        };
        assert_eq!(diff_of("café.txt"), "@@ -1 +1 @@\n-caf\n+café au lait");
        assert_eq!(diff_of("gone.txt"), "@@ -1 +0,0 @@\n-to delete");
        assert_eq!(diff_of("img.bin"), "");
        assert_eq!(diff_of("renamed.txt"), "");
        assert_eq!(
            diff_of("nonl.txt"),
            "@@ -1 +1 @@\n-no newline\n\\ No newline at end of file\n+no newline changed\n\\ No newline at end of file"
        );
        assert_eq!(diff_of("weird b/thing.txt"), "@@ -1,2 +1,2 @@\n a\n-b\n+c");
        assert!(files.iter().all(|file| !file.truncated));
    }

    /// Bitbucket Cloud's `/diff` and GitHub's `.diff` carry `index` lines and
    /// mode changes; a binary file sometimes comes as `GIT binary patch`.
    #[test]
    fn splits_host_shaped_git_diffs() {
        let diff = concat!(
            "diff --git a/src/app.ts b/src/app.ts\n",
            "index 3b18e51..a9c5c1f 100644\n",
            "--- a/src/app.ts\n",
            "+++ b/src/app.ts\n",
            "@@ -10,7 +10,7 @@ export function main() {\n",
            " const a = 1;\n",
            "-const b = 2;\n",
            "+const b = 3;\n",
            "diff --git a/script.sh b/script.sh\n",
            "old mode 100644\n",
            "new mode 100755\n",
            "diff --git a/docs/old.md b/docs/new.md\n",
            "similarity index 90%\n",
            "rename from docs/old.md\n",
            "rename to docs/new.md\n",
            "index 1111111..2222222 100644\n",
            "--- a/docs/old.md\n",
            "+++ b/docs/new.md\n",
            "@@ -1 +1 @@\n",
            "-Old\n",
            "+New\n",
            "diff --git a/logo.png b/logo.png\n",
            "new file mode 100644\n",
            "index 0000000..e69de29\n",
            "GIT binary patch\n",
            "literal 12\n",
            "TcmZQzWMXDuWnf@n00093\n",
            "\n",
            "literal 0\n",
            "HcmV?d00001\n",
        );
        let files = split_diff(diff);
        assert_eq!(
            files
                .iter()
                .map(|file| (file.path.as_str(), file.unified_diff.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (
                    "src/app.ts",
                    "@@ -10,7 +10,7 @@ export function main() {\n const a = 1;\n-const b = 2;\n+const b = 3;"
                ),
                ("script.sh", ""),
                ("docs/new.md", "@@ -1 +1 @@\n-Old\n+New"),
                ("logo.png", ""),
            ]
        );
    }

    /// What `gitlab_unified_diff` builds from GitLab's per-file hunks: always
    /// `---`/`+++` lines, never quoted, no trailing tab after a space.
    #[test]
    fn splits_gitlab_reassembled_diffs() {
        let diff = concat!(
            "diff --git a/my dir/a b.txt b/my dir/a b.txt\n",
            "--- a/my dir/a b.txt\n",
            "+++ b/my dir/a b.txt\n",
            "@@ -1 +1 @@\n",
            "-x\n",
            "+y\n",
            "diff --git a/removed.txt b/removed.txt\n",
            "--- a/removed.txt\n",
            "+++ /dev/null\n",
            "@@ -1 +0,0 @@\n",
            "-gone\n",
            "diff --git a/old name.txt b/new name.txt\n",
            "--- a/old name.txt\n",
            "+++ b/new name.txt\n",
        );
        assert_eq!(
            split_diff(diff)
                .iter()
                .map(|file| (file.path.as_str(), file.unified_diff.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("my dir/a b.txt", "@@ -1 +1 @@\n-x\n+y"),
                ("removed.txt", "@@ -1 +0,0 @@\n-gone"),
                ("new name.txt", ""),
            ]
        );
    }

    #[test]
    fn unquotes_git_paths() {
        assert_eq!(unquote("\"caf\\303\\251.txt\""), "café.txt");
        assert_eq!(unquote("\"tab\\there\\\\\\\"\""), "tab\there\\\"");
        assert_eq!(unquote("plain.txt"), "plain.txt");
        assert_eq!(git_line_new_path("a/x b/y b/x b/y"), "x b/y");
        assert_eq!(git_line_new_path("\"a/q\\\"\" \"b/q\\\"\""), "q\"");
    }

    #[test]
    fn splits_plain_diffs_into_files() {
        let files = split_diff(
            "--- a/x.ts\n+++ b/x.ts\n@@ -1 +1 @@\n-a\n+b\n--- a/y.ts\n+++ b/y.ts\n@@ -2 +2 @@\n-c\n+d\n",
        );
        assert_eq!(
            files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["x.ts", "y.ts"]
        );
        assert_eq!(files[1].unified_diff, "@@ -2 +2 @@\n-c\n+d");
    }

    #[test]
    fn diff_parts_stay_within_the_budget() {
        let file = |path: &str, lines: usize| PullRequestFile {
            path: path.into(),
            unified_diff: std::iter::once("@@ -1 +1 @@".to_string())
                .chain((0..lines).map(|i| format!("+line \"{i}\"")))
                .collect::<Vec<_>>()
                .join("\n"),
            truncated: false,
        };
        let budget = 2_000;
        let parts = diff_parts(
            vec![
                file("a", 10),
                file("b", 10),
                file("huge", 1_000),
                file("c", 10),
            ],
            budget,
        );
        assert!(parts.len() > 1);
        assert_eq!(parts.iter().filter(|(_, last)| *last).count(), 1);
        assert!(parts.last().is_some_and(|(_, last)| *last));
        for (files, _) in &parts {
            assert!(
                json_size(files) <= budget + files.len(),
                "{}",
                json_size(files)
            );
        }
        let huge = parts
            .iter()
            .flat_map(|(files, _)| files)
            .find(|file| file.path == "huge")
            .unwrap();
        assert!(huge.truncated);
        assert!(huge.unified_diff.starts_with("@@"));
        assert_eq!(diff_parts(Vec::new(), budget), vec![(Vec::new(), true)]);
    }

    #[test]
    fn descriptions_that_escape_large_still_fit() {
        let detail = |body: String| git::PullRequestDetail {
            number: 1,
            title: "t".into(),
            body: body.into(),
            state: git::PullRequestState::Open,
            author_login: "a".into(),
            source_branch: "s".into(),
            target_branch: "t".into(),
            head_sha: "h".into(),
            base_sha: "b".into(),
            url: url::Url::parse("https://github.com/o/r/pull/1").unwrap(),
            created_at: String::new().into(),
            updated_at: String::new().into(),
            is_draft: false,
            is_mergeable: None,
            additions: 0,
            deletions: 0,
            changed_files: 0,
            commits: None,
            behind_by: None,
            viewer_review: None,
            reviewers: Vec::new(),
            checks: None,
            viewer_is_author: None,
        };
        // Each control character serializes as six bytes.
        let message = detail_message(
            "w".into(),
            "r".into(),
            detail("\u{1}".repeat(MAX_BODY_CHARS)),
            Vec::new(),
        );
        assert!(json_size(&message) <= frame_budget());
        let DesktopMessage::PullRequest { detail: sent, .. } = &message else {
            panic!()
        };
        assert!(
            sent.body
                .ends_with("_The rest of the description is on the host._")
        );

        let message = detail_message("w".into(), "r".into(), detail("Short.".into()), Vec::new());
        let DesktopMessage::PullRequest { detail: sent, .. } = &message else {
            panic!()
        };
        assert_eq!(sent.body, "Short.");
    }

    #[test]
    fn keeps_the_newest_comments_that_fit() {
        let comment = |id: u32| ReviewComment {
            comment_id: id.to_string(),
            author: "a".into(),
            body: "x".repeat(100),
            path: "p".into(),
            line: Some(1),
            side: Some(DiffSide::New),
            parent_id: None,
            is_resolved: false,
            created_at: String::new(),
            url: String::new(),
        };
        let all = (0..10).map(comment).collect::<Vec<_>>();
        let one = json_size(&all[0]) + 1;
        let (kept, truncated) = fit_comments(all.clone(), one * 3);
        assert!(truncated);
        assert_eq!(
            kept.iter()
                .map(|c| c.comment_id.as_str())
                .collect::<Vec<_>>(),
            vec!["7", "8", "9"]
        );
        assert_eq!(fit_comments(all, usize::MAX).1, false);
    }

    #[test]
    fn list_parts_split_large_workspaces() {
        let repo = |id: &str, count: u32| PullRequestRepo {
            repo_id: id.into(),
            name: id.into(),
            host: Some("GitHub".into()),
            status: RepoStatus::Loaded,
            message: None,
            authored: Vec::new(),
            others: (0..count)
                .map(|number| PullRequestSummary {
                    number,
                    title: "t".repeat(200),
                    author: "a".into(),
                    state: PullRequestState::Open,
                    source_branch: "b".into(),
                    target_branch: "main".into(),
                    url: "https://example.com".into(),
                    updated_at: String::new(),
                    is_draft: false,
                    reviewers: None,
                })
                .collect(),
            page: 1,
            may_have_more: false,
        };
        let repos = (0..12)
            .map(|i| repo(&format!("r{i}"), 50))
            .collect::<Vec<_>>();
        let parts = list_parts("w", PullRequestFilter::All, false, repos);
        assert!(parts.len() > 1);
        for message in &parts {
            assert!(json_size(message) <= crate::companion::MAX_FRAME_PLAINTEXT_BYTES);
        }
        assert!(matches!(
            parts.last(),
            Some(DesktopMessage::PullRequests { last: true, .. })
        ));
        assert!(matches!(
            parts.first(),
            Some(DesktopMessage::PullRequests {
                part: 0,
                last: false,
                ..
            })
        ));
    }
}
