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

use git::{
    BuildCommitPermalinkParams, BuildPermalinkParams, GitHostAuth, GitHostingProvider,
    ParsedGitRemote, PullRequest, PullRequestDetail, PullRequestListFilter, PullRequestMergeMethod,
    PullRequestReviewComment, PullRequestReviewVerdict, PullRequestState, PullRequestSummary,
    RemoteUrl,
};

use crate::get_host_from_git_remote_url;

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
        let Some(host) = self.base_url.host_str() else {
            bail!("failed to get host from github base url");
        };
        let url = format!("https://api.{host}/repos/{repo_owner}/{repo}/commits/{commit}");

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

    async fn list_pull_requests(
        &self,
        remote: &ParsedGitRemote,
        filter: PullRequestListFilter,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<Vec<PullRequestSummary>> {
        let host = self
            .base_url
            .host_str()
            .context("GitHub base URL has no host")?;
        let state = github_list_state(&filter);
        let per_page = filter.limit.unwrap_or(50).clamp(1, 100);
        let url = format!(
            "https://api.{host}/repos/{owner}/{repo}/pulls?state={state}&per_page={per_page}&sort=updated&direction=desc",
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
        let raw: Vec<GithubPullRequest> =
            serde_json::from_slice(&bytes).context("parsing GitHub pull request list")?;

        let mut summaries = Vec::new();
        for pr in raw {
            let pr_state = pr.pull_request_state();
            if let Some(states) = &filter.states
                && !states.contains(&pr_state)
            {
                continue;
            }
            if let Some(author) = &filter.author {
                let login = pr.user.as_ref().map(|user| user.login.as_str()).unwrap_or_default();
                if !login.to_lowercase().contains(&author.to_lowercase()) {
                    continue;
                }
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

    async fn get_pull_request(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<PullRequestDetail> {
        let host = self
            .base_url
            .host_str()
            .context("GitHub base URL has no host")?;
        let url = format!(
            "https://api.{host}/repos/{owner}/{repo}/pulls/{number}",
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
        pr.into_detail()
    }

    async fn get_pull_request_diff(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<String> {
        let host = self
            .base_url
            .host_str()
            .context("GitHub base URL has no host")?;
        let url = format!(
            "https://api.{host}/repos/{owner}/{repo}/pulls/{number}",
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
        let host = self
            .base_url
            .host_str()
            .context("GitHub base URL has no host")?;
        // The `raw` media type returns the file bytes directly rather than the
        // JSON envelope. `path` is already repo-relative with `/` separators,
        // which is exactly what the contents API expects.
        let url = format!(
            "https://api.{host}/repos/{owner}/{repo}/contents/{path}?ref={revision}",
            owner = remote.owner,
            repo = remote.repo,
        );
        let request =
            github_request(GithubMethod::Get, &url, "application/vnd.github.raw", &auth, None)?;
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
        let host = self
            .base_url
            .host_str()
            .context("GitHub base URL has no host")?;
        let url = format!(
            "https://api.{host}/repos/{owner}/{repo}/pulls/{number}/comments?per_page=100",
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

    async fn merge_pull_request(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        method: PullRequestMergeMethod,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<()> {
        let host = self
            .base_url
            .host_str()
            .context("GitHub base URL has no host")?;
        let url = format!(
            "https://api.{host}/repos/{owner}/{repo}/pulls/{number}/merge",
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
        let host = self
            .base_url
            .host_str()
            .context("GitHub base URL has no host")?;
        let url = format!(
            "https://api.{host}/repos/{owner}/{repo}/pulls/{number}/reviews",
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

    async fn post_review_comment(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        in_reply_to: u64,
        body: SharedString,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<()> {
        let host = self
            .base_url
            .host_str()
            .context("GitHub base URL has no host")?;
        let url = format!(
            "https://api.{host}/repos/{owner}/{repo}/pulls/{number}/comments/{in_reply_to}/replies",
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
}

enum GithubMethod {
    Get,
    Post,
    Put,
}

/// Builds the `Authorization` header value for a GitHub API request. Falls back
/// to the `GITHUB_TOKEN` environment variable when no stored credential is
/// supplied, preserving the previous headless/CI behavior.
fn github_auth_header(auth: &Option<GitHostAuth>) -> Option<String> {
    match auth {
        Some(GitHostAuth::Bearer(token)) => Some(format!("Bearer {token}")),
        // GitHub authenticates with bearer tokens; Basic credentials belong to
        // other hosts and are not applicable here.
        Some(GitHostAuth::Basic { .. }) => None,
        None => std::env::var("GITHUB_TOKEN")
            .ok()
            .filter(|token| !token.is_empty())
            .map(|token| format!("Bearer {token}")),
    }
}

fn github_request(
    method: GithubMethod,
    url: &str,
    accept: &str,
    auth: &Option<GitHostAuth>,
    json_body: Option<Vec<u8>>,
) -> Result<Request<AsyncBody>> {
    let builder = match method {
        GithubMethod::Get => Request::get(url),
        GithubMethod::Post => Request::post(url),
        GithubMethod::Put => Request::put(url),
    };
    let mut builder = builder
        .header("Accept", accept)
        .header("User-Agent", "Lathe")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .follow_redirects(http_client::RedirectPolicy::FollowAll);
    if let Some(value) = github_auth_header(auth) {
        builder = builder.header("Authorization", value);
    }
    let request = match json_body {
        Some(body) => builder
            .header("Content-Type", "application/json")
            .body(AsyncBody::from(body))?,
        None => builder.body(AsyncBody::default())?,
    };
    Ok(request)
}

async fn github_send(
    client: &Arc<dyn HttpClient>,
    request: Request<AsyncBody>,
    context: &str,
) -> Result<Vec<u8>> {
    let mut response = client
        .send(request)
        .await
        .with_context(|| format!("error while {context}"))?;
    let mut bytes = Vec::new();
    response.body_mut().read_to_end(&mut bytes).await?;
    if !response.status().is_success() {
        let text = String::from_utf8_lossy(&bytes);
        bail!(
            "{context} failed ({}): {text:?}",
            response.status().as_u16()
        );
    }
    Ok(bytes)
}

/// Maps a [`PullRequestListFilter`] to GitHub's single-valued `state` query
/// parameter. GitHub has no "merged" state (merged PRs are closed with a
/// `merged_at`), so merged is folded into closed and refined client-side.
fn github_list_state(filter: &PullRequestListFilter) -> &'static str {
    match &filter.states {
        None => "all",
        Some(states) => {
            let wants_open = states.contains(&PullRequestState::Open);
            let wants_closed = states.contains(&PullRequestState::Closed)
                || states.contains(&PullRequestState::Merged);
            match (wants_open, wants_closed) {
                (true, false) => "open",
                (false, true) => "closed",
                _ => "all",
            }
        }
    }
}

#[derive(Deserialize)]
struct GithubUserRef {
    login: String,
}

#[derive(Deserialize)]
struct GithubBranchRef {
    #[serde(rename = "ref")]
    ref_name: String,
    sha: String,
}

#[derive(Deserialize)]
struct GithubPullRequest {
    number: u32,
    title: String,
    #[serde(default)]
    body: Option<String>,
    state: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    user: Option<GithubUserRef>,
    head: GithubBranchRef,
    base: GithubBranchRef,
    html_url: String,
    updated_at: String,
    #[serde(default)]
    merged_at: Option<String>,
    #[serde(default)]
    mergeable: Option<bool>,
    #[serde(default)]
    additions: Option<u32>,
    #[serde(default)]
    deletions: Option<u32>,
    #[serde(default)]
    changed_files: Option<u32>,
}

impl GithubPullRequest {
    fn pull_request_state(&self) -> PullRequestState {
        if self.merged_at.is_some() {
            PullRequestState::Merged
        } else if self.state == "closed" {
            PullRequestState::Closed
        } else {
            PullRequestState::Open
        }
    }

    fn author_login(&self) -> SharedString {
        self.user
            .as_ref()
            .map(|user| SharedString::from(user.login.clone()))
            .unwrap_or_default()
    }

    fn into_summary(self, state: PullRequestState) -> Result<PullRequestSummary> {
        let author_login = self.author_login();
        let url = Url::parse(&self.html_url).context("parsing pull request URL")?;
        Ok(PullRequestSummary {
            number: self.number,
            title: self.title.into(),
            author_login,
            state,
            source_branch: self.head.ref_name.into(),
            target_branch: self.base.ref_name.into(),
            url,
            updated_at: self.updated_at.into(),
            is_draft: self.draft,
        })
    }

    fn into_detail(self) -> Result<PullRequestDetail> {
        let state = self.pull_request_state();
        let author_login = self.author_login();
        let url = Url::parse(&self.html_url).context("parsing pull request URL")?;
        Ok(PullRequestDetail {
            number: self.number,
            title: self.title.into(),
            body: self.body.unwrap_or_default().into(),
            state,
            author_login,
            source_branch: self.head.ref_name.into(),
            target_branch: self.base.ref_name.into(),
            head_sha: self.head.sha.into(),
            base_sha: self.base.sha.into(),
            url,
            updated_at: self.updated_at.into(),
            is_draft: self.draft,
            is_mergeable: self.mergeable,
            additions: self.additions.unwrap_or(0),
            deletions: self.deletions.unwrap_or(0),
            changed_files: self.changed_files.unwrap_or(0),
        })
    }
}

#[derive(Deserialize)]
struct GithubReviewComment {
    id: u64,
    #[serde(default)]
    user: Option<GithubUserRef>,
    body: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    line: Option<u32>,
    /// Present on replies; references the top-level comment of the thread.
    #[serde(default)]
    in_reply_to_id: Option<u64>,
    created_at: String,
    html_url: String,
}

impl GithubReviewComment {
    fn into_comment(self) -> Result<PullRequestReviewComment> {
        let url = Url::parse(&self.html_url).context("parsing review comment URL")?;
        Ok(PullRequestReviewComment {
            id: self.id,
            author_login: self
                .user
                .map(|user| SharedString::from(user.login))
                .unwrap_or_default(),
            body: self.body.into(),
            path: self.path.unwrap_or_default().into(),
            line: self.line,
            parent_id: self.in_reply_to_id,
            // GitHub's REST review-comment API does not report thread resolution.
            is_resolved: false,
            created_at: self.created_at.into(),
            url,
        })
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

    #[test]
    fn test_github_pull_request_into_summary() {
        let json = r#"{
            "number": 42,
            "title": "Add device flow",
            "body": "Body text",
            "state": "open",
            "draft": true,
            "user": { "login": "octocat" },
            "head": { "ref": "feature", "sha": "aaa111" },
            "base": { "ref": "main", "sha": "bbb222" },
            "html_url": "https://github.com/owner/repo/pull/42",
            "updated_at": "2026-01-02T03:04:05Z"
        }"#;
        let pr: GithubPullRequest = serde_json::from_str(json).unwrap();
        let state = pr.pull_request_state();
        let summary = pr.into_summary(state).unwrap();

        assert_eq!(summary.number, 42);
        assert_eq!(summary.title.to_string(), "Add device flow");
        assert_eq!(summary.author_login.to_string(), "octocat");
        assert_eq!(summary.state, PullRequestState::Open);
        assert_eq!(summary.source_branch.to_string(), "feature");
        assert_eq!(summary.target_branch.to_string(), "main");
        assert_eq!(summary.url.as_str(), "https://github.com/owner/repo/pull/42");
        assert_eq!(summary.updated_at.to_string(), "2026-01-02T03:04:05Z");
        assert!(summary.is_draft);
    }

    #[test]
    fn test_github_pull_request_into_detail_merged_with_stats() {
        // `merged_at` present must win over a `state` of "closed".
        let json = r#"{
            "number": 7,
            "title": "Ship it",
            "body": "Detailed body",
            "state": "closed",
            "user": { "login": "octocat" },
            "head": { "ref": "topic", "sha": "headsha" },
            "base": { "ref": "main", "sha": "basesha" },
            "html_url": "https://github.com/owner/repo/pull/7",
            "updated_at": "2026-01-03T00:00:00Z",
            "merged_at": "2026-01-03T00:00:00Z",
            "mergeable": true,
            "additions": 12,
            "deletions": 3,
            "changed_files": 2
        }"#;
        let pr: GithubPullRequest = serde_json::from_str(json).unwrap();
        let detail = pr.into_detail().unwrap();

        assert_eq!(detail.state, PullRequestState::Merged);
        assert_eq!(detail.body.to_string(), "Detailed body");
        assert_eq!(detail.head_sha.to_string(), "headsha");
        assert_eq!(detail.base_sha.to_string(), "basesha");
        assert_eq!(detail.is_mergeable, Some(true));
        assert_eq!(detail.additions, 12);
        assert_eq!(detail.deletions, 3);
        assert_eq!(detail.changed_files, 2);
    }

    #[test]
    fn test_github_pull_request_missing_optional_fields_default() {
        // No `user`, no `body`, no stats: mapper must not panic and must default.
        let json = r#"{
            "number": 1,
            "title": "Minimal",
            "state": "closed",
            "head": { "ref": "h", "sha": "hs" },
            "base": { "ref": "b", "sha": "bs" },
            "html_url": "https://github.com/owner/repo/pull/1",
            "updated_at": "2026-01-01T00:00:00Z"
        }"#;
        let pr: GithubPullRequest = serde_json::from_str(json).unwrap();
        let detail = pr.into_detail().unwrap();

        assert_eq!(detail.state, PullRequestState::Closed);
        assert_eq!(detail.author_login.to_string(), "");
        assert_eq!(detail.body.to_string(), "");
        assert_eq!(detail.is_mergeable, None);
        assert_eq!(detail.additions, 0);
        assert!(!detail.is_draft);
    }

    #[test]
    fn test_github_review_comment_into_comment() {
        let json = r#"{
            "id": 9001,
            "user": { "login": "reviewer" },
            "body": "Nit: rename this",
            "path": "src/main.rs",
            "line": 120,
            "created_at": "2026-01-04T05:06:07Z",
            "html_url": "https://github.com/owner/repo/pull/7#discussion_r9001"
        }"#;
        let comment: GithubReviewComment = serde_json::from_str(json).unwrap();
        let mapped = comment.into_comment().unwrap();

        assert_eq!(mapped.id, 9001);
        assert_eq!(mapped.author_login.to_string(), "reviewer");
        assert_eq!(mapped.body.to_string(), "Nit: rename this");
        assert_eq!(mapped.path.to_string(), "src/main.rs");
        assert_eq!(mapped.line, Some(120));
        assert_eq!(
            mapped.url.as_str(),
            "https://github.com/owner/repo/pull/7#discussion_r9001"
        );
    }
}
