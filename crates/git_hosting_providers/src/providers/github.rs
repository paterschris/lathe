use std::str::FromStr;
use std::sync::{Arc, LazyLock};

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use futures::AsyncReadExt;
use gpui::SharedString;
use http_client::{AsyncBody, HttpClient, HttpRequestExt, Request};
use regex::Regex;
use serde::Deserialize;
use url::Url;
use urlencoding::encode;
use util::ResultExt as _;

use git::{
    BuildCommitPermalinkParams, BuildPermalinkParams, DiffCommentSide, GitHostAuth,
    GitHostAuthKind, GitHostingProvider, NewPullRequest, PullRequestChecks,
    ReviewerCandidate, ParsedGitRemote, PullRequest, PullRequestAuthError, PullRequestDetail,
    PullRequestListFilter, PullRequestMergeMethod, PullRequestReviewComment,
    PullRequestReviewVerdict, PullRequestReviewer, PullRequestState, PullRequestSummary, RemoteUrl,
};

use crate::get_host_from_git_remote_url;

#[path = "github_lathe.rs"]
mod lathe;

use lathe::*;

fn pull_request_number_regex() -> &'static Regex {
    static PULL_REQUEST_NUMBER_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\(#(\d+)\)$").unwrap());
    &PULL_REQUEST_NUMBER_REGEX
}

#[derive(Debug, Deserialize)]
struct CommitDetails {
    #[expect(
        unused,
        reason = "This field was found to be unused with serde library bump; it's left as is due to insufficient context on PO's side, but it *may* be fine to remove"
    )]
    commit: Commit,
    author: Option<User>,
}

#[derive(Debug, Deserialize)]
struct Commit {
    #[expect(
        unused,
        reason = "This field was found to be unused with serde library bump; it's left as is due to insufficient context on PO's side, but it *may* be fine to remove"
    )]
    author: Author,
}

#[derive(Debug, Deserialize)]
struct Author {
    #[expect(
        unused,
        reason = "This field was found to be unused with serde library bump; it's left as is due to insufficient context on PO's side, but it *may* be fine to remove"
    )]
    email: String,
}

#[derive(Debug, Deserialize)]
struct User {
    #[expect(
        unused,
        reason = "This field was found to be unused with serde library bump; it's left as is due to insufficient context on PO's side, but it *may* be fine to remove"
    )]
    pub id: u64,
    pub avatar_url: String,
}

#[derive(Debug)]
pub struct Github {
    name: String,
    base_url: Url,
}

fn normalize_author_email(email: &str) -> &str {
    email.trim_start_matches('<').trim_end_matches('>')
}

fn build_cdn_avatar_url(email: &str) -> Result<Url> {
    let email = normalize_author_email(email);
    Url::parse(&format!(
        "https://avatars.githubusercontent.com/u/e?email={}&s=128",
        encode(email)
    ))
    .context("failed to construct avatar URL")
}

fn build_cdn_avatar_url_for_author_email(email: &str) -> Result<Option<Url>> {
    let email = normalize_author_email(email);
    if email.ends_with("[bot]@users.noreply.github.com") {
        return Ok(None);
    }

    build_cdn_avatar_url(email).map(Some)
}

impl Github {
    pub fn new(name: impl Into<String>, base_url: Url) -> Self {
        Self {
            name: name.into(),
            base_url,
        }
    }

    pub fn public_instance() -> Self {
        Self::new("GitHub", Url::parse("https://github.com").unwrap())
    }

    pub fn from_remote_url(remote_url: &str) -> Result<Self> {
        let host = get_host_from_git_remote_url(remote_url)?;
        if host == "github.com" {
            bail!("the GitHub instance is not self-hosted");
        }

        // TODO: detecting self hosted instances by checking whether "github" is in the url or not
        // is not very reliable. See https://github.com/zed-industries/zed/issues/26393 for more
        // information.
        if !host.contains("github") {
            bail!("not a GitHub URL");
        }

        Ok(Self::new(
            "GitHub Self-Hosted",
            Url::parse(&format!("https://{}", host))?,
        ))
    }

    async fn fetch_github_commit_author(
        &self,
        repo_owner: &str,
        repo: &str,
        commit: &str,
        client: &Arc<dyn HttpClient>,
    ) -> Result<Option<User>> {
        let api = self.api_base()?;
        let url = format!("{api}/repos/{repo_owner}/{repo}/commits/{commit}");

        let mut request = Request::get(&url)
            .header("Content-Type", "application/json")
            .follow_redirects(http_client::RedirectPolicy::FollowAll);

        if let Ok(github_token) = std::env::var("GITHUB_TOKEN") {
            request = request.header("Authorization", format!("Bearer {}", github_token));
        }

        let mut response = client
            .send(request.body(AsyncBody::default())?)
            .await
            .with_context(|| format!("error fetching GitHub commit details at {:?}", url))?;

        let mut body = Vec::new();
        response.body_mut().read_to_end(&mut body).await?;

        if response.status().is_client_error() {
            let text = String::from_utf8_lossy(body.as_slice());
            bail!(
                "status error {}, response: {text:?}",
                response.status().as_u16()
            );
        }

        let body_str = std::str::from_utf8(&body)?;

        serde_json::from_str::<CommitDetails>(body_str)
            .map(|commit| commit.author)
            .context("failed to deserialize GitHub commit details")
    }
}

#[async_trait]
impl GitHostingProvider for Github {
    fn auth_kind(&self) -> Option<GitHostAuthKind> {
        Some(GitHostAuthKind::GitHub)
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    fn base_url(&self) -> Url {
        self.base_url.clone()
    }

    fn supports_avatars(&self) -> bool {
        // Avatars are not supported for self-hosted GitHub instances
        // See tracking issue: https://github.com/zed-industries/zed/issues/11043
        &self.name == "GitHub"
    }

    fn format_line_number(&self, line: u32) -> String {
        format!("L{line}")
    }

    fn format_line_numbers(&self, start_line: u32, end_line: u32) -> String {
        format!("L{start_line}-L{end_line}")
    }

    fn parse_remote_url(&self, url: &str) -> Option<ParsedGitRemote> {
        let url = RemoteUrl::from_str(url).ok()?;

        let host = url.host_str()?;
        if host != self.base_url.host_str()? {
            return None;
        }

        let mut path_segments = url.path_segments()?;
        let mut owner = path_segments.next()?;
        if owner.is_empty() {
            owner = path_segments.next()?;
        }

        let repo = path_segments.next()?.trim_end_matches(".git");

        Some(ParsedGitRemote {
            owner: owner.into(),
            repo: repo.into(),
        })
    }

    fn build_commit_permalink(
        &self,
        remote: &ParsedGitRemote,
        params: BuildCommitPermalinkParams,
    ) -> Url {
        let BuildCommitPermalinkParams { sha } = params;
        let ParsedGitRemote { owner, repo } = remote;

        self.base_url()
            .join(&format!("{owner}/{repo}/commit/{sha}"))
            .unwrap()
    }

    fn build_permalink(&self, remote: ParsedGitRemote, params: BuildPermalinkParams) -> Url {
        let ParsedGitRemote { owner, repo } = remote;
        let BuildPermalinkParams {
            sha,
            path,
            selection,
        } = params;

        let mut permalink = self
            .base_url()
            .join(&format!("{owner}/{repo}/blob/{sha}/{path}"))
            .unwrap();
        if path.ends_with(".md") {
            permalink.set_query(Some("plain=1"));
        }
        permalink.set_fragment(
            selection
                .map(|selection| self.line_fragment(&selection))
                .as_deref(),
        );
        permalink
    }

    fn build_create_pull_request_url(
        &self,
        remote: &ParsedGitRemote,
        source_branch: &str,
    ) -> Option<Url> {
        let ParsedGitRemote { owner, repo } = remote;
        let encoded_source = encode(source_branch);

        self.base_url()
            .join(&format!("{owner}/{repo}/pull/new/{encoded_source}"))
            .ok()
    }

    fn extract_pull_request(&self, remote: &ParsedGitRemote, message: &str) -> Option<PullRequest> {
        let line = message.lines().next()?;
        let capture = pull_request_number_regex().captures(line)?;
        let number = capture.get(1)?.as_str().parse::<u32>().ok()?;

        let mut url = self.base_url();
        let path = format!("/{}/{}/pull/{}", remote.owner, remote.repo, number);
        url.set_path(&path);

        Some(PullRequest { number, url })
    }

    async fn commit_author_avatar_url(
        &self,
        repo_owner: &str,
        repo: &str,
        commit: SharedString,
        author_email: Option<SharedString>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<Option<Url>> {
        if let Some(email) = author_email
            && let Some(avatar_url) = build_cdn_avatar_url_for_author_email(&email)?
        {
            return Ok(Some(avatar_url));
        }

        let commit = commit.to_string();
        let avatar_url = self
            .fetch_github_commit_author(repo_owner, repo, &commit, &http_client)
            .await?
            .map(|author| -> Result<Url, url::ParseError> {
                let mut url = Url::parse(&author.avatar_url)?;
                url.set_query(Some("size=128"));
                Ok(url)
            })
            .transpose()?;
        Ok(avatar_url)
    }

    async fn fetch_authenticated_user(
        &self,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<Option<SharedString>> {
        self.fetch_authenticated_login(&auth, &http_client).await
    }

    async fn list_pull_requests(
        &self,
        remote: &ParsedGitRemote,
        filter: PullRequestListFilter,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<Vec<PullRequestSummary>> {
        let api = self.api_base()?;
        // Resolving "review requested from me" needs the caller's own login, so
        // fetch it once up front and match it against each PR's requested
        // reviewers below.
        let authenticated_login = if filter.reviewer_is_me || filter.author_is_me {
            Some(
                self.fetch_authenticated_login(&auth, &http_client)
                    .await?
                    .context("could not determine the authenticated GitHub user")?,
            )
        } else {
            None
        };
        let state = github_list_state(&filter);
        // Resolving "me" (reviewer or author) matches client-side below, so a
        // single page of `limit` rows would miss any match outside the most
        // recently updated window. In that case scan several pages up to a cap,
        // mirroring the Bitbucket provider. A plain listing stays one page.
        let deep_scan = filter.reviewer_is_me || filter.author_is_me;
        let per_page = if deep_scan {
            100
        } else {
            filter.limit.unwrap_or(50).clamp(1, 100)
        };
        let scan_cap = if deep_scan {
            (filter.limit.unwrap_or(50) as usize).max(200)
        } else {
            per_page as usize
        };
        let mut raw: Vec<GithubPullRequest> = Vec::new();
        // A deep scan filters client-side over a wide window, so its results do
        // not line up with any one page of the host's response; only a plain
        // listing honours the caller's page.
        let mut page = if deep_scan {
            1
        } else {
            filter.page.unwrap_or(1).max(1)
        };
        loop {
            let url = format!(
                "{api}/repos/{owner}/{repo}/pulls?state={state}&per_page={per_page}&page={page}&sort=updated&direction=desc",
                owner = remote.owner,
                repo = remote.repo,
            );
            let request = github_request(
                GithubMethod::Get,
                &url,
                "application/vnd.github+json",
                &auth,
                None,
            )?;
            let bytes = github_send(&http_client, request, "listing GitHub pull requests").await?;
            let batch: Vec<GithubPullRequest> =
                serde_json::from_slice(&bytes).context("parsing GitHub pull request list")?;
            let batch_len = batch.len();
            raw.extend(batch);
            if !deep_scan || batch_len < per_page as usize || raw.len() >= scan_cap {
                break;
            }
            page += 1;
        }

        let mut summaries = Vec::new();
        for pr in raw {
            let pr_state = pr.pull_request_state();
            if let Some(states) = &filter.states
                && !states.contains(&pr_state)
            {
                continue;
            }
            if let Some(author) = &filter.author {
                let login = pr
                    .user
                    .as_ref()
                    .map(|user| user.login.as_str())
                    .unwrap_or_default();
                if !login.to_lowercase().contains(&author.to_lowercase()) {
                    continue;
                }
            }
            if filter.author_is_me
                && !pr.user.as_ref().is_some_and(|user| {
                    authenticated_login
                        .as_ref()
                        .is_some_and(|login| user.login.eq_ignore_ascii_case(login.as_ref()))
                })
            {
                continue;
            }
            if filter.reviewer_is_me
                && !pr.requested_reviewers.iter().any(|reviewer| {
                    authenticated_login
                        .as_ref()
                        .is_some_and(|login| reviewer.login.eq_ignore_ascii_case(login.as_ref()))
                })
            {
                continue;
            }
            summaries.push(pr.into_summary(pr_state)?);
            if let Some(limit) = filter.limit
                && summaries.len() as u32 >= limit
            {
                break;
            }
        }
        Ok(summaries)
    }

    async fn create_pull_request(
        &self,
        remote: &ParsedGitRemote,
        request: NewPullRequest,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<PullRequestSummary> {
        let api = self.api_base()?;
        let url = format!(
            "{api}/repos/{owner}/{repo}/pulls",
            owner = remote.owner,
            repo = remote.repo,
        );
        let body = serde_json::json!({
            "title": request.title.to_string(),
            "body": request.body.to_string(),
            "head": request.source_branch.to_string(),
            "base": request.target_branch.to_string(),
            "draft": request.is_draft,
        });
        let http_request = github_request(
            GithubMethod::Post,
            &url,
            "application/vnd.github+json",
            &auth,
            Some(serde_json::to_vec(&body)?),
        )?;
        let bytes =
            github_send(&http_client, http_request, "creating GitHub pull request").await?;
        let created: GithubPullRequest =
            serde_json::from_slice(&bytes).context("parsing created GitHub pull request")?;
        let state = created.pull_request_state();
        created.into_summary(state)
    }

    async fn default_branch(
        &self,
        remote: &ParsedGitRemote,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<Option<SharedString>> {
        let api = self.api_base()?;
        let url = format!(
            "{api}/repos/{owner}/{repo}",
            owner = remote.owner,
            repo = remote.repo,
        );
        let request = github_request(
            GithubMethod::Get,
            &url,
            "application/vnd.github+json",
            &auth,
            None,
        )?;
        let bytes = github_send(&http_client, request, "fetching GitHub repository").await?;
        let repository: GithubRepository =
            serde_json::from_slice(&bytes).context("parsing GitHub repository")?;
        Ok(repository.default_branch.map(SharedString::from))
    }

    async fn get_pull_request(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<PullRequestDetail> {
        let api = self.api_base()?;
        let url = format!(
            "{api}/repos/{owner}/{repo}/pulls/{number}",
            owner = remote.owner,
            repo = remote.repo,
        );
        let request = github_request(
            GithubMethod::Get,
            &url,
            "application/vnd.github+json",
            &auth,
            None,
        )?;
        let bytes = github_send(&http_client, request, "fetching GitHub pull request").await?;
        let pr: GithubPullRequest =
            serde_json::from_slice(&bytes).context("parsing GitHub pull request")?;
        let requested: Vec<SharedString> = pr
            .requested_reviewers
            .iter()
            .map(|user| SharedString::from(user.login.clone()))
            .collect();
        let mut detail = pr.into_detail()?;
        // Resolved once and used for both "is this my PR" and "which reviewer is
        // me". Best-effort: an unauthenticated or restricted token leaves it
        // `None` and the header falls back to its author-agnostic layout.
        let viewer_login = self
            .fetch_authenticated_login(&auth, &http_client)
            .await
            .log_err()
            .flatten();
        detail.viewer_is_author = viewer_login
            .as_deref()
            .map(|login| login.eq_ignore_ascii_case(&detail.author_login));
        // Reviews are best-effort: on failure the detail still renders, just
        // without verdict/reviewers. One reviews fetch feeds both.
        if let Some(reviews) = self
            .fetch_reviews(remote, number, &auth, &http_client)
            .await
            .log_err()
        {
            let reviewers = build_github_reviewers(&reviews, &requested, viewer_login.as_deref());
            detail.viewer_review = reviewers
                .iter()
                .find(|reviewer| reviewer.is_me)
                .and_then(|reviewer| reviewer.verdict);
            detail.reviewers = reviewers;
        }
        // Best-effort: how many commits the PR is behind its base branch. GitHub's
        // compare endpoint reports `behind_by` = commits on base not reachable
        // from head. A failure just leaves the indicator unset.
        let compare_url = format!(
            "{api}/repos/{owner}/{repo}/compare/{base}...{head}",
            owner = remote.owner,
            repo = remote.repo,
            base = detail.target_branch,
            head = detail.source_branch,
        );
        detail.behind_by = async {
            let request = github_request(
                GithubMethod::Get,
                &compare_url,
                "application/vnd.github+json",
                &auth,
                None,
            )?;
            let bytes = github_send(&http_client, request, "comparing GitHub branches").await?;
            let comparison: GithubComparison =
                serde_json::from_slice(&bytes).context("parsing GitHub comparison")?;
            anyhow::Ok(comparison.behind_by)
        }
        .await
        .log_err();
        // Best-effort: a repository with no CI, or a token that cannot read
        // checks, leaves this unset and the header says so rather than showing
        // a false failure.
        detail.checks = self
            .fetch_checks(remote, &detail.head_sha.clone(), &auth, &http_client)
            .await
            .log_err()
            .flatten();
        Ok(detail)
    }

    async fn pull_request_reviewers(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<Vec<PullRequestReviewer>> {
        let api = self.api_base()?;
        let url = format!(
            "{api}/repos/{owner}/{repo}/pulls/{number}",
            owner = remote.owner,
            repo = remote.repo,
        );
        let request = github_request(
            GithubMethod::Get,
            &url,
            "application/vnd.github+json",
            &auth,
            None,
        )?;
        let bytes = github_send(&http_client, request, "fetching GitHub pull request").await?;
        let pr: GithubPullRequest =
            serde_json::from_slice(&bytes).context("parsing GitHub pull request")?;
        let requested: Vec<SharedString> = pr
            .requested_reviewers
            .iter()
            .map(|user| SharedString::from(user.login.clone()))
            .collect();
        let reviews = self
            .fetch_reviews(remote, number, &auth, &http_client)
            .await?;
        let viewer_login = self
            .fetch_authenticated_login(&auth, &http_client)
            .await
            .log_err()
            .flatten();
        Ok(build_github_reviewers(
            &reviews,
            &requested,
            viewer_login.as_deref(),
        ))
    }

    async fn pull_request_review_state(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<Option<PullRequestReviewVerdict>> {
        let Some(login) = self.fetch_authenticated_login(&auth, &http_client).await? else {
            return Ok(None);
        };
        let reviews = self
            .fetch_reviews(remote, number, &auth, &http_client)
            .await?;
        Ok(viewer_verdict_from_reviews(&reviews, login.as_ref()))
    }

    async fn get_pull_request_diff(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<String> {
        let api = self.api_base()?;
        let url = format!(
            "{api}/repos/{owner}/{repo}/pulls/{number}",
            owner = remote.owner,
            repo = remote.repo,
        );
        let request = github_request(
            GithubMethod::Get,
            &url,
            "application/vnd.github.v3.diff",
            &auth,
            None,
        )?;
        let bytes = github_send(&http_client, request, "fetching GitHub pull request diff").await?;
        String::from_utf8(bytes).context("GitHub pull request diff was not valid UTF-8")
    }

    async fn get_file_content(
        &self,
        remote: &ParsedGitRemote,
        path: &str,
        revision: &str,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<String> {
        let api = self.api_base()?;
        // The `raw` media type returns the file bytes directly rather than the
        // JSON envelope. `path` is already repo-relative with `/` separators,
        // which is exactly what the contents API expects.
        let url = format!(
            "{api}/repos/{owner}/{repo}/contents/{path}?ref={revision}",
            owner = remote.owner,
            repo = remote.repo,
        );
        let request = github_request(
            GithubMethod::Get,
            &url,
            "application/vnd.github.raw",
            &auth,
            None,
        )?;
        let bytes = github_send(&http_client, request, "fetching GitHub file content").await?;
        String::from_utf8(bytes).context("GitHub file content was not valid UTF-8")
    }

    async fn get_pull_request_comments(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<Vec<PullRequestReviewComment>> {
        let api = self.api_base()?;
        let url = format!(
            "{api}/repos/{owner}/{repo}/pulls/{number}/comments?per_page=100",
            owner = remote.owner,
            repo = remote.repo,
        );
        let request = github_request(
            GithubMethod::Get,
            &url,
            "application/vnd.github+json",
            &auth,
            None,
        )?;
        let bytes = github_send(&http_client, request, "fetching GitHub review comments").await?;
        let raw: Vec<GithubReviewComment> =
            serde_json::from_slice(&bytes).context("parsing GitHub review comments")?;
        raw.into_iter()
            .map(GithubReviewComment::into_comment)
            .collect()
    }

    fn supports_draft_pull_requests(&self) -> bool {
        true
    }

    async fn close_pull_request(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<()> {
        self.set_pull_request_state(remote, number, "closed", &auth, &http_client)
            .await
    }

    async fn reopen_pull_request(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<()> {
        self.set_pull_request_state(remote, number, "open", &auth, &http_client)
            .await
    }

    async fn list_reviewer_candidates(
        &self,
        remote: &ParsedGitRemote,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<Vec<ReviewerCandidate>> {
        let api = self.api_base()?;
        let url = format!(
            "{api}/repos/{owner}/{repo}/collaborators?per_page=100",
            owner = remote.owner,
            repo = remote.repo,
        );
        let request = github_request(
            GithubMethod::Get,
            &url,
            "application/vnd.github+json",
            &auth,
            None,
        )?;
        let bytes = github_send(&http_client, request, "listing GitHub collaborators").await?;
        let collaborators: Vec<GithubUserRef> =
            serde_json::from_slice(&bytes).context("parsing GitHub collaborators")?;
        let mut candidates: Vec<ReviewerCandidate> = collaborators
            .into_iter()
            .map(|user| ReviewerCandidate {
                // GitHub's reviewer-request endpoint takes logins directly.
                handle: user.login.clone().into(),
                login: user.login.into(),
                // The collaborators listing does not include real names, and
                // fetching one per account would mean a request per member just
                // to open a picker.
                display_name: None,
            })
            .collect();
        candidates.sort_by_key(|candidate| candidate.login.to_lowercase());
        Ok(candidates)
    }

    async fn request_reviewers(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        reviewers: Vec<SharedString>,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<()> {
        let logins: Vec<String> = reviewers
            .iter()
            .map(|reviewer| reviewer.trim().to_string())
            .filter(|reviewer| !reviewer.is_empty())
            .collect();
        if logins.is_empty() {
            return Ok(());
        }
        let api = self.api_base()?;
        let url = format!(
            "{api}/repos/{owner}/{repo}/pulls/{number}/requested_reviewers",
            owner = remote.owner,
            repo = remote.repo,
        );
        // GitHub adds to the existing request list rather than replacing it, so
        // no read-modify-write is needed here.
        let body = serde_json::json!({ "reviewers": logins });
        let request = github_request(
            GithubMethod::Post,
            &url,
            "application/vnd.github+json",
            &auth,
            Some(serde_json::to_vec(&body)?),
        )?;
        github_send(&http_client, request, "requesting GitHub reviewers").await?;
        Ok(())
    }

    async fn set_pull_request_draft(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        draft: bool,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<()> {
        // GitHub's REST API cannot move a pull request between draft and ready;
        // only these GraphQL mutations can.
        let node_id = self
            .fetch_pull_request_node_id(remote, number, &auth, &http_client)
            .await?;
        let (query, context) = if draft {
            (
                "mutation($id:ID!){convertPullRequestToDraft(input:{pullRequestId:$id}){clientMutationId}}",
                "converting GitHub pull request to draft",
            )
        } else {
            (
                "mutation($id:ID!){markPullRequestReadyForReview(input:{pullRequestId:$id}){clientMutationId}}",
                "marking GitHub pull request ready for review",
            )
        };
        self.run_graphql_mutation(query, &node_id, &auth, &http_client, context)
            .await
    }

    async fn merge_pull_request(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        method: PullRequestMergeMethod,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<()> {
        let api = self.api_base()?;
        let url = format!(
            "{api}/repos/{owner}/{repo}/pulls/{number}/merge",
            owner = remote.owner,
            repo = remote.repo,
        );
        let merge_method = match method {
            PullRequestMergeMethod::Merge => "merge",
            PullRequestMergeMethod::Squash => "squash",
            PullRequestMergeMethod::Rebase => "rebase",
        };
        let body = serde_json::to_vec(&serde_json::json!({ "merge_method": merge_method }))?;
        let request = github_request(
            GithubMethod::Put,
            &url,
            "application/vnd.github+json",
            &auth,
            Some(body),
        )?;
        github_send(&http_client, request, "merging GitHub pull request").await?;
        Ok(())
    }

    async fn submit_review(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        verdict: PullRequestReviewVerdict,
        body: Option<SharedString>,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<()> {
        let api = self.api_base()?;
        let url = format!(
            "{api}/repos/{owner}/{repo}/pulls/{number}/reviews",
            owner = remote.owner,
            repo = remote.repo,
        );
        let event = match verdict {
            PullRequestReviewVerdict::Approve => "APPROVE",
            PullRequestReviewVerdict::RequestChanges => "REQUEST_CHANGES",
            PullRequestReviewVerdict::Comment => "COMMENT",
        };
        let mut payload = serde_json::Map::new();
        payload.insert("event".into(), serde_json::Value::from(event));
        if let Some(body) = body {
            payload.insert("body".into(), serde_json::Value::from(body.to_string()));
        }
        let body = serde_json::to_vec(&serde_json::Value::Object(payload))?;
        let request = github_request(
            GithubMethod::Post,
            &url,
            "application/vnd.github+json",
            &auth,
            Some(body),
        )?;
        github_send(&http_client, request, "submitting GitHub review").await?;
        Ok(())
    }

    async fn remove_review(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        verdict: PullRequestReviewVerdict,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<()> {
        self.remove_review_with_verdict(remote, number, verdict, auth, http_client)
            .await
    }

    async fn post_review_comment(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        in_reply_to: u64,
        body: SharedString,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<()> {
        let api = self.api_base()?;
        let url = format!(
            "{api}/repos/{owner}/{repo}/pulls/{number}/comments/{in_reply_to}/replies",
            owner = remote.owner,
            repo = remote.repo,
        );
        let body = serde_json::to_vec(&serde_json::json!({ "body": body.to_string() }))?;
        let request = github_request(
            GithubMethod::Post,
            &url,
            "application/vnd.github+json",
            &auth,
            Some(body),
        )?;
        github_send(&http_client, request, "posting GitHub review comment").await?;
        Ok(())
    }

    async fn create_review_comment(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        commit_id: SharedString,
        path: SharedString,
        line: u32,
        side: DiffCommentSide,
        body: SharedString,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<()> {
        let api = self.api_base()?;
        let url = format!(
            "{api}/repos/{owner}/{repo}/pulls/{number}/comments",
            owner = remote.owner,
            repo = remote.repo,
        );
        let side = match side {
            DiffCommentSide::Right => "RIGHT",
            DiffCommentSide::Left => "LEFT",
        };
        let body = serde_json::to_vec(&serde_json::json!({
            "body": body.to_string(),
            "commit_id": commit_id.to_string(),
            "path": path.to_string(),
            "line": line,
            "side": side,
        }))?;
        let request = github_request(
            GithubMethod::Post,
            &url,
            "application/vnd.github+json",
            &auth,
            Some(body),
        )?;
        github_send(&http_client, request, "creating GitHub review comment").await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use git::repository::repo_path;
    use indoc::indoc;
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_remote_url_with_root_slash() {
        let remote_url = "git@github.com:/zed-industries/zed";
        let parsed_remote = Github::public_instance()
            .parse_remote_url(remote_url)
            .unwrap();

        assert_eq!(
            parsed_remote,
            ParsedGitRemote {
                owner: "zed-industries".into(),
                repo: "zed".into(),
            }
        );
    }

    #[test]
    fn test_invalid_self_hosted_remote_url() {
        let remote_url = "git@github.com:zed-industries/zed.git";
        let github = Github::from_remote_url(remote_url);
        assert!(github.is_err());
    }

    #[test]
    fn test_from_remote_url_ssh() {
        let remote_url = "git@github.my-enterprise.com:zed-industries/zed.git";
        let github = Github::from_remote_url(remote_url).unwrap();

        assert!(!github.supports_avatars());
        assert_eq!(github.name, "GitHub Self-Hosted".to_string());
        assert_eq!(
            github.base_url,
            Url::parse("https://github.my-enterprise.com").unwrap()
        );
    }

    #[test]
    fn test_from_remote_url_https() {
        let remote_url = "https://github.my-enterprise.com/zed-industries/zed.git";
        let github = Github::from_remote_url(remote_url).unwrap();

        assert!(!github.supports_avatars());
        assert_eq!(github.name, "GitHub Self-Hosted".to_string());
        assert_eq!(
            github.base_url,
            Url::parse("https://github.my-enterprise.com").unwrap()
        );
    }

    #[test]
    fn test_parse_remote_url_given_self_hosted_ssh_url() {
        let remote_url = "git@github.my-enterprise.com:zed-industries/zed.git";
        let parsed_remote = Github::from_remote_url(remote_url)
            .unwrap()
            .parse_remote_url(remote_url)
            .unwrap();

        assert_eq!(
            parsed_remote,
            ParsedGitRemote {
                owner: "zed-industries".into(),
                repo: "zed".into(),
            }
        );
    }

    #[test]
    fn test_parse_remote_url_given_self_hosted_https_url_with_subgroup() {
        let remote_url = "https://github.my-enterprise.com/zed-industries/zed.git";
        let parsed_remote = Github::from_remote_url(remote_url)
            .unwrap()
            .parse_remote_url(remote_url)
            .unwrap();

        assert_eq!(
            parsed_remote,
            ParsedGitRemote {
                owner: "zed-industries".into(),
                repo: "zed".into(),
            }
        );
    }

    #[test]
    fn test_parse_remote_url_given_ssh_url() {
        let parsed_remote = Github::public_instance()
            .parse_remote_url("git@github.com:zed-industries/zed.git")
            .unwrap();

        assert_eq!(
            parsed_remote,
            ParsedGitRemote {
                owner: "zed-industries".into(),
                repo: "zed".into(),
            }
        );
    }

    #[test]
    fn test_parse_remote_url_given_https_url() {
        let parsed_remote = Github::public_instance()
            .parse_remote_url("https://github.com/zed-industries/zed.git")
            .unwrap();

        assert_eq!(
            parsed_remote,
            ParsedGitRemote {
                owner: "zed-industries".into(),
                repo: "zed".into(),
            }
        );
    }

    #[test]
    fn test_parse_remote_url_given_https_url_with_username() {
        let parsed_remote = Github::public_instance()
            .parse_remote_url("https://jlannister@github.com/some-org/some-repo.git")
            .unwrap();

        assert_eq!(
            parsed_remote,
            ParsedGitRemote {
                owner: "some-org".into(),
                repo: "some-repo".into(),
            }
        );
    }

    #[test]
    fn test_build_github_permalink_from_ssh_url() {
        let remote = ParsedGitRemote {
            owner: "zed-industries".into(),
            repo: "zed".into(),
        };
        let permalink = Github::public_instance().build_permalink(
            remote,
            BuildPermalinkParams::new(
                "e6ebe7974deb6bb6cc0e2595c8ec31f0c71084b7",
                &repo_path("crates/editor/src/git/permalink.rs"),
                None,
            ),
        );

        let expected_url = "https://github.com/zed-industries/zed/blob/e6ebe7974deb6bb6cc0e2595c8ec31f0c71084b7/crates/editor/src/git/permalink.rs";
        assert_eq!(permalink.to_string(), expected_url.to_string())
    }

    #[test]
    fn test_build_github_permalink() {
        let permalink = Github::public_instance().build_permalink(
            ParsedGitRemote {
                owner: "zed-industries".into(),
                repo: "zed".into(),
            },
            BuildPermalinkParams::new(
                "b2efec9824c45fcc90c9a7eb107a50d1772a60aa",
                &repo_path("crates/zed/src/main.rs"),
                None,
            ),
        );

        let expected_url = "https://github.com/zed-industries/zed/blob/b2efec9824c45fcc90c9a7eb107a50d1772a60aa/crates/zed/src/main.rs";
        assert_eq!(permalink.to_string(), expected_url.to_string())
    }

    #[test]
    fn test_build_github_permalink_with_single_line_selection() {
        let permalink = Github::public_instance().build_permalink(
            ParsedGitRemote {
                owner: "zed-industries".into(),
                repo: "zed".into(),
            },
            BuildPermalinkParams::new(
                "e6ebe7974deb6bb6cc0e2595c8ec31f0c71084b7",
                &repo_path("crates/editor/src/git/permalink.rs"),
                Some(6..6),
            ),
        );

        let expected_url = "https://github.com/zed-industries/zed/blob/e6ebe7974deb6bb6cc0e2595c8ec31f0c71084b7/crates/editor/src/git/permalink.rs#L7";
        assert_eq!(permalink.to_string(), expected_url.to_string())
    }

    #[test]
    fn test_build_github_permalink_with_multi_line_selection() {
        let permalink = Github::public_instance().build_permalink(
            ParsedGitRemote {
                owner: "zed-industries".into(),
                repo: "zed".into(),
            },
            BuildPermalinkParams::new(
                "e6ebe7974deb6bb6cc0e2595c8ec31f0c71084b7",
                &repo_path("crates/editor/src/git/permalink.rs"),
                Some(23..47),
            ),
        );

        let expected_url = "https://github.com/zed-industries/zed/blob/e6ebe7974deb6bb6cc0e2595c8ec31f0c71084b7/crates/editor/src/git/permalink.rs#L24-L48";
        assert_eq!(permalink.to_string(), expected_url.to_string())
    }

    #[test]
    fn test_build_github_create_pr_url() {
        let remote = ParsedGitRemote {
            owner: "zed-industries".into(),
            repo: "zed".into(),
        };

        let provider = Github::public_instance();

        let url = provider
            .build_create_pull_request_url(&remote, "feature/something cool")
            .expect("url should be constructed");

        assert_eq!(
            url.as_str(),
            "https://github.com/zed-industries/zed/pull/new/feature%2Fsomething%20cool"
        );
    }

    #[test]
    fn test_github_pull_requests() {
        let remote = ParsedGitRemote {
            owner: "zed-industries".into(),
            repo: "zed".into(),
        };

        let github = Github::public_instance();
        let message = "This does not contain a pull request";
        assert!(github.extract_pull_request(&remote, message).is_none());

        // Pull request number at end of first line
        let message = indoc! {r#"
            project panel: do not expand collapsed worktrees on "collapse all entries" (#10687)

            Fixes #10597

            Release Notes:

            - Fixed "project panel: collapse all entries" expanding collapsed worktrees.
            "#
        };

        assert_eq!(
            github
                .extract_pull_request(&remote, message)
                .unwrap()
                .url
                .as_str(),
            "https://github.com/zed-industries/zed/pull/10687"
        );

        // Pull request number in middle of line, which we want to ignore
        let message = indoc! {r#"
            Follow-up to #10687 to fix problems

            See the original PR, this is a fix.
            "#
        };
        assert_eq!(github.extract_pull_request(&remote, message), None);
    }

    /// Regression test for issue #39875
    #[test]
    fn test_git_permalink_url_escaping() {
        let permalink = Github::public_instance().build_permalink(
            ParsedGitRemote {
                owner: "zed-industries".into(),
                repo: "nonexistent".into(),
            },
            BuildPermalinkParams::new(
                "3ef1539900037dd3601be7149b2b39ed6d0ce3db",
                &repo_path("app/blog/[slug]/page.tsx"),
                Some(7..7),
            ),
        );

        let expected_url = "https://github.com/zed-industries/nonexistent/blob/3ef1539900037dd3601be7149b2b39ed6d0ce3db/app/blog/%5Bslug%5D/page.tsx#L8";
        assert_eq!(permalink.to_string(), expected_url.to_string())
    }

    #[test]
    fn test_build_create_pull_request_url() {
        let remote = ParsedGitRemote {
            owner: "zed-industries".into(),
            repo: "zed".into(),
        };

        let github = Github::public_instance();
        let url = github
            .build_create_pull_request_url(&remote, "feature/new-feature")
            .unwrap();

        assert_eq!(
            url.as_str(),
            "https://github.com/zed-industries/zed/pull/new/feature%2Fnew-feature"
        );

        let base_url = Url::parse("https://github.zed.com").unwrap();
        let github = Github::new("GitHub Self-Hosted", base_url);
        let url = github
            .build_create_pull_request_url(&remote, "feature/new-feature")
            .expect("should be able to build pull request url");

        assert_eq!(
            url.as_str(),
            "https://github.zed.com/zed-industries/zed/pull/new/feature%2Fnew-feature"
        );
    }

    #[test]
    fn test_build_cdn_avatar_url_simple_email() {
        let url = build_cdn_avatar_url("user@example.com").unwrap();
        assert_eq!(
            url.as_str(),
            "https://avatars.githubusercontent.com/u/e?email=user%40example.com&s=128"
        );
    }

    #[test]
    fn test_build_cdn_avatar_url_with_angle_brackets() {
        let url = build_cdn_avatar_url("<user@example.com>").unwrap();
        assert_eq!(
            url.as_str(),
            "https://avatars.githubusercontent.com/u/e?email=user%40example.com&s=128"
        );
    }

    #[test]
    fn test_build_cdn_avatar_url_with_special_chars() {
        let url = build_cdn_avatar_url("user+tag@example.com").unwrap();
        assert_eq!(
            url.as_str(),
            "https://avatars.githubusercontent.com/u/e?email=user%2Btag%40example.com&s=128"
        );
    }

    #[test]
    fn test_build_cdn_avatar_url_for_author_email_skips_bot_noreply_emails() {
        for email in [
            "41898282+github-actions[bot]@users.noreply.github.com",
            "<41898282+github-actions[bot]@users.noreply.github.com>",
        ] {
            assert_eq!(build_cdn_avatar_url_for_author_email(email).unwrap(), None);
        }
    }

    #[test]
    fn test_build_cdn_avatar_url_for_author_email_uses_user_noreply_emails() {
        let url = build_cdn_avatar_url_for_author_email("12345+octocat@users.noreply.github.com")
            .unwrap()
            .unwrap();

        assert_eq!(
            url.as_str(),
            "https://avatars.githubusercontent.com/u/e?email=12345%2Boctocat%40users.noreply.github.com&s=128"
        );
    }

    /// GitHub logins are case-insensitive, so a viewer who typed "Octocat" into
    /// their credential must still match an `octocat`-authored pull request.
    #[test]
    fn test_github_detail_resolves_viewer_as_author_ignoring_case() {
        let detail = github_detail_for_author_login("octocat", Some("OCTOCAT"));
        assert_eq!(detail.viewer_is_author, Some(true));
    }

    #[test]
    fn test_github_detail_resolves_viewer_as_non_author() {
        let detail = github_detail_for_author_login("octocat", Some("someone-else"));
        assert_eq!(detail.viewer_is_author, Some(false));
    }

    /// A token that cannot read `/user` leaves authorship unknown rather than
    /// asserting the pull request belongs to someone else.
    #[test]
    fn test_github_detail_leaves_authorship_unknown_when_viewer_unresolved() {
        let detail = github_detail_for_author_login("octocat", None);
        assert_eq!(detail.viewer_is_author, None);
    }

    /// Drives `get_pull_request` against a fake host. `viewer_login` of `None`
    /// makes `/user` fail the way a scopeless token does.
    fn github_detail_for_author_login(
        author_login: &'static str,
        viewer_login: Option<&'static str>,
    ) -> PullRequestDetail {
        let client: Arc<dyn HttpClient> =
            http_client::FakeHttpClient::create(move |request| async move {
                let path = request.uri().path();
                let (status, body) = if path == "/user" {
                    match viewer_login {
                        Some(login) => (200, format!(r#"{{"login":"{login}"}}"#)),
                        None => (403, r#"{"message":"forbidden"}"#.to_string()),
                    }
                } else if path == "/repos/owner/repo/pulls/7" {
                    (
                        200,
                        format!(
                            r#"{{"number":7,"title":"Ownership","state":"open",
                            "user":{{"login":"{author_login}"}},
                            "head":{{"ref":"feature","sha":"a1"}},
                            "base":{{"ref":"main","sha":"b1"}},
                            "html_url":"https://github.com/owner/repo/pull/7",
                            "created_at":"2026-01-01T00:00:00Z",
                            "updated_at":"2026-01-02T00:00:00Z"}}"#
                        ),
                    )
                } else {
                    (200, "[]".to_string())
                };
                Ok(http_client::Response::builder()
                    .status(status)
                    .body(body.into())
                    .unwrap())
            });
        let remote = ParsedGitRemote {
            owner: "owner".into(),
            repo: "repo".into(),
        };
        futures::executor::block_on(Github::public_instance().get_pull_request(
            &remote,
            7,
            Some(GitHostAuth::Bearer("token".into())),
            client,
        ))
        .unwrap()
    }
}
