use std::collections::HashMap;
use std::sync::LazyLock;
use std::{str::FromStr, sync::Arc};

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use base64::Engine as _;
use futures::AsyncReadExt;
use gpui::SharedString;
use http_client::{AsyncBody, HttpClient, HttpRequestExt, Request};
use itertools::Itertools as _;
use regex::Regex;
use serde::{Deserialize, de::DeserializeOwned};
use url::Url;

use git::{
    BuildCommitPermalinkParams, BuildPermalinkParams, GitHostAuth, GitHostingProvider,
    ParsedGitRemote, PullRequest, PullRequestDetail, PullRequestListFilter, PullRequestMergeMethod,
    PullRequestReviewComment, PullRequestReviewVerdict, PullRequestState, PullRequestSummary,
    RemoteUrl,
};
use urlencoding::encode;

use crate::get_host_from_git_remote_url;

/// Matches a Bitbucket `@`-mention token in a comment body, e.g.
/// `@{62f4fec150bd9783f62da8af}`; capture group 1 is the account id.
fn bitbucket_mention_regex() -> &'static Regex {
    static MENTION_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"@\{([^}]+)\}").unwrap());
    &MENTION_REGEX
}

/// Reduces a Bitbucket account id / uuid to a comparison key by dropping braces,
/// dashes, and colons and lowercasing, so the differently-formatted ids in a
/// mention token and an account object can be matched.
fn normalize_mention_key(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Rewrites `@{account_id}` mention tokens to `@DisplayName` using a map built
/// from the thread participants, so comments read the way Bitbucket renders them.
/// Tokens for accounts not in the map are left untouched.
fn resolve_bitbucket_mentions(body: &str, names: &HashMap<String, SharedString>) -> String {
    bitbucket_mention_regex()
        .replace_all(body, |captures: &regex::Captures| {
            let key = normalize_mention_key(&captures[1]);
            match names.get(&key) {
                Some(name) => format!("@{name}"),
                None => captures[0].to_string(),
            }
        })
        .into_owned()
}

fn pull_request_regex() -> &'static Regex {
    static PULL_REQUEST_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        // This matches Bitbucket PR reference pattern: (pull request #xxx)
        Regex::new(r"\(pull request #(\d+)\)").unwrap()
    });
    &PULL_REQUEST_REGEX
}

#[derive(Debug, Deserialize)]
struct CommitDetails {
    author: Author,
}

#[derive(Debug, Deserialize)]
struct Author {
    user: Account,
}

#[derive(Debug, Deserialize)]
struct Account {
    links: AccountLinks,
}

#[derive(Debug, Deserialize)]
struct AccountLinks {
    avatar: Option<Link>,
}

#[derive(Debug, Deserialize)]
struct Link {
    href: String,
}

#[derive(Debug, Deserialize)]
struct CommitDetailsSelfHosted {
    author: AuthorSelfHosted,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthorSelfHosted {
    avatar_url: Option<String>,
}

pub struct Bitbucket {
    name: String,
    base_url: Url,
}

impl Bitbucket {
    pub fn new(name: impl Into<String>, base_url: Url) -> Self {
        Self {
            name: name.into(),
            base_url,
        }
    }

    pub fn public_instance() -> Self {
        Self::new("Bitbucket", Url::parse("https://bitbucket.org").unwrap())
    }

    pub fn from_remote_url(remote_url: &str) -> Result<Self> {
        let host = get_host_from_git_remote_url(remote_url)?;
        if host == "bitbucket.org" {
            bail!("the BitBucket instance is not self-hosted");
        }

        // TODO: detecting self hosted instances by checking whether "bitbucket" is in the url or not
        // is not very reliable. See https://github.com/zed-industries/zed/issues/26393 for more
        // information.
        if !host.contains("bitbucket") {
            bail!("not a BitBucket URL");
        }

        Ok(Self::new(
            "BitBucket Self-Hosted",
            Url::parse(&format!("https://{}", host))?,
        ))
    }

    fn is_self_hosted(&self) -> bool {
        self.base_url
            .host_str()
            .is_some_and(|host| host != "bitbucket.org")
    }

    async fn fetch_bitbucket_commit_author(
        &self,
        repo_owner: &str,
        repo: &str,
        commit: &str,
        client: &Arc<dyn HttpClient>,
    ) -> Result<Option<String>> {
        let Some(host) = self.base_url.host_str() else {
            bail!("failed to get host from bitbucket base url");
        };
        let is_self_hosted = self.is_self_hosted();
        let url = if is_self_hosted {
            format!(
                "https://{host}/rest/api/latest/projects/{repo_owner}/repos/{repo}/commits/{commit}?avatarSize=128"
            )
        } else {
            format!("https://api.{host}/2.0/repositories/{repo_owner}/{repo}/commit/{commit}")
        };

        let request = Request::get(&url)
            .header("Content-Type", "application/json")
            .follow_redirects(http_client::RedirectPolicy::FollowAll);

        let mut response = client
            .send(request.body(AsyncBody::default())?)
            .await
            .with_context(|| format!("error fetching BitBucket commit details at {:?}", url))?;

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

        if is_self_hosted {
            serde_json::from_str::<CommitDetailsSelfHosted>(body_str)
                .map(|commit| commit.author.avatar_url)
        } else {
            serde_json::from_str::<CommitDetails>(body_str)
                .map(|commit| commit.author.user.links.avatar.map(|link| link.href))
        }
        .context("failed to deserialize BitBucket commit details")
    }
}

#[async_trait]
impl GitHostingProvider for Bitbucket {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn base_url(&self) -> Url {
        self.base_url.clone()
    }

    fn supports_avatars(&self) -> bool {
        true
    }

    fn format_line_number(&self, line: u32) -> String {
        if self.is_self_hosted() {
            return format!("{line}");
        }
        format!("lines-{line}")
    }

    fn format_line_numbers(&self, start_line: u32, end_line: u32) -> String {
        if self.is_self_hosted() {
            return format!("{start_line}-{end_line}");
        }
        format!("lines-{start_line}:{end_line}")
    }

    fn parse_remote_url(&self, url: &str) -> Option<ParsedGitRemote> {
        let url = RemoteUrl::from_str(url).ok()?;

        let host = url.host_str()?;
        if host != self.base_url.host_str()? {
            return None;
        }

        let mut path_segments = url.path_segments()?.collect::<Vec<_>>();
        let repo = path_segments.pop()?.trim_end_matches(".git");
        let owner = if path_segments.get(0).is_some_and(|v| *v == "scm") && path_segments.len() > 1
        {
            // Skip the "scm" segment if it's not the only segment
            // https://github.com/gitkraken/vscode-gitlens/blob/a6e3c6fbb255116507eaabaa9940c192ed7bb0e1/src/git/remotes/bitbucket-server.ts#L72-L74
            path_segments.into_iter().skip(1).join("/")
        } else {
            path_segments.into_iter().join("/")
        };

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
        if self.is_self_hosted() {
            return self
                .base_url()
                .join(&format!("projects/{owner}/repos/{repo}/commits/{sha}"))
                .unwrap();
        }
        self.base_url()
            .join(&format!("{owner}/{repo}/commits/{sha}"))
            .unwrap()
    }

    fn build_permalink(&self, remote: ParsedGitRemote, params: BuildPermalinkParams) -> Url {
        let ParsedGitRemote { owner, repo } = remote;
        let BuildPermalinkParams {
            sha,
            path,
            selection,
        } = params;

        let mut permalink = if self.is_self_hosted() {
            self.base_url()
                .join(&format!(
                    "projects/{owner}/repos/{repo}/browse/{path}?at={sha}"
                ))
                .unwrap()
        } else {
            self.base_url()
                .join(&format!("{owner}/{repo}/src/{sha}/{path}"))
                .unwrap()
        };

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

        if self.is_self_hosted() {
            let mut url = self
                .base_url()
                .join(&format!("projects/{owner}/repos/{repo}/compare/commits"))
                .ok()?;
            let source_ref = format!("refs/heads/{source_branch}");
            let encoded_ref = encode(&source_ref);
            url.set_query(Some(&format!("sourceBranch={encoded_ref}")));
            Some(url)
        } else {
            let mut url = self
                .base_url()
                .join(&format!("{owner}/{repo}/pull-requests/new"))
                .ok()?;
            let encoded_branch = encode(source_branch);
            url.set_query(Some(&format!("source={encoded_branch}")));
            Some(url)
        }
    }

    fn extract_pull_request(&self, remote: &ParsedGitRemote, message: &str) -> Option<PullRequest> {
        // Check first line of commit message for PR references
        let first_line = message.lines().next()?;

        // Try to match against our PR patterns
        let capture = pull_request_regex().captures(first_line)?;
        let number = capture.get(1)?.as_str().parse::<u32>().ok()?;

        // Construct the PR URL in Bitbucket format
        let mut url = self.base_url();
        let path = if self.is_self_hosted() {
            format!(
                "/projects/{}/repos/{}/pull-requests/{}",
                remote.owner, remote.repo, number
            )
        } else {
            format!("/{}/{}/pull-requests/{}", remote.owner, remote.repo, number)
        };
        url.set_path(&path);

        Some(PullRequest { number, url })
    }

    async fn commit_author_avatar_url(
        &self,
        repo_owner: &str,
        repo: &str,
        commit: SharedString,
        _author_email: Option<SharedString>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<Option<Url>> {
        let commit = commit.to_string();
        let avatar_url = self
            .fetch_bitbucket_commit_author(repo_owner, repo, &commit, &http_client)
            .await?
            .map(|avatar_url| Url::parse(&avatar_url))
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
        if self.is_self_hosted() {
            bail!("pull request operations are only supported on Bitbucket Cloud");
        }
        let api_base = bitbucket_cloud_api_base(&self.base_url, remote)?;
        let state_query = bitbucket_states(&filter)
            .iter()
            .map(|state| format!("state={state}"))
            .collect::<Vec<_>>()
            .join("&");
        let limit = filter.limit.unwrap_or(50);
        let first_url = format!("{api_base}/pullrequests?{state_query}&pagelen=50&sort=-updated_on");
        let raw: Vec<BitbucketPullRequest> = bitbucket_get_paginated(
            &http_client,
            first_url,
            &auth,
            "listing Bitbucket pull requests",
            limit as usize,
        )
        .await?;

        let mut summaries = Vec::new();
        for pr in raw {
            let pr_state = pr.pull_request_state();
            if let Some(author) = &filter.author {
                let login = pr
                    .author
                    .as_ref()
                    .map(|account| account.login())
                    .unwrap_or_default();
                if !login.to_lowercase().contains(&author.to_lowercase()) {
                    continue;
                }
            }
            summaries.push(pr.into_summary(pr_state)?);
            if summaries.len() as u32 >= limit {
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
        if self.is_self_hosted() {
            bail!("pull request operations are only supported on Bitbucket Cloud");
        }
        let api_base = bitbucket_cloud_api_base(&self.base_url, remote)?;
        let url = format!("{api_base}/pullrequests/{number}");
        let request = bitbucket_request(BitbucketMethod::Get, &url, &auth, None)?;
        let bytes = bitbucket_send(&http_client, request, "fetching Bitbucket pull request").await?;
        let pr: BitbucketPullRequest =
            serde_json::from_slice(&bytes).context("parsing Bitbucket pull request")?;
        pr.into_detail()
    }

    async fn get_pull_request_diff(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<String> {
        if self.is_self_hosted() {
            bail!("pull request operations are only supported on Bitbucket Cloud");
        }
        let api_base = bitbucket_cloud_api_base(&self.base_url, remote)?;
        let url = format!("{api_base}/pullrequests/{number}/diff");
        let request = bitbucket_request(BitbucketMethod::Get, &url, &auth, None)?;
        let bytes =
            bitbucket_send(&http_client, request, "fetching Bitbucket pull request diff").await?;
        String::from_utf8(bytes).context("Bitbucket pull request diff was not valid UTF-8")
    }

    async fn get_file_content(
        &self,
        remote: &ParsedGitRemote,
        path: &str,
        revision: &str,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<String> {
        if self.is_self_hosted() {
            bail!("pull request operations are only supported on Bitbucket Cloud");
        }
        let api_base = bitbucket_cloud_api_base(&self.base_url, remote)?;
        // The `src` endpoint returns the raw file at a revision; `path` is already
        // repo-relative with `/` separators.
        let url = format!("{api_base}/src/{revision}/{path}");
        let request = bitbucket_request(BitbucketMethod::Get, &url, &auth, None)?;
        let bytes = bitbucket_send(&http_client, request, "fetching Bitbucket file content").await?;
        String::from_utf8(bytes).context("Bitbucket file content was not valid UTF-8")
    }

    async fn get_pull_request_comments(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<Vec<PullRequestReviewComment>> {
        if self.is_self_hosted() {
            bail!("pull request operations are only supported on Bitbucket Cloud");
        }
        let api_base = bitbucket_cloud_api_base(&self.base_url, remote)?;
        let first_url = format!("{api_base}/pullrequests/{number}/comments?pagelen=100");
        let raw: Vec<BitbucketComment> = bitbucket_get_paginated(
            &http_client,
            first_url,
            &auth,
            "fetching Bitbucket review comments",
            500,
        )
        .await?;
        // Build an id -> display-name map from every participant so `@{id}`
        // mention tokens can be rewritten to the names Bitbucket shows.
        let mut names: HashMap<String, SharedString> = HashMap::default();
        for comment in &raw {
            if let Some(user) = comment.user.as_ref()
                && let Some(name) = user.mention_name()
            {
                for key in user.mention_keys() {
                    names.insert(key, name.clone());
                }
            }
        }
        raw.into_iter()
            .filter(|comment| !comment.deleted)
            .map(|comment| {
                let mut comment = comment.into_comment()?;
                comment.body = resolve_bitbucket_mentions(&comment.body, &names).into();
                Ok(comment)
            })
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
        if self.is_self_hosted() {
            bail!("pull request operations are only supported on Bitbucket Cloud");
        }
        let api_base = bitbucket_cloud_api_base(&self.base_url, remote)?;
        let url = format!("{api_base}/pullrequests/{number}/merge");
        // Bitbucket has no rebase merge; `fast_forward` is the closest analogue.
        let strategy = match method {
            PullRequestMergeMethod::Merge => "merge_commit",
            PullRequestMergeMethod::Squash => "squash",
            PullRequestMergeMethod::Rebase => "fast_forward",
        };
        let body = serde_json::to_vec(&serde_json::json!({ "merge_strategy": strategy }))?;
        let request = bitbucket_request(BitbucketMethod::Post, &url, &auth, Some(body))?;
        bitbucket_send(&http_client, request, "merging Bitbucket pull request").await?;
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
        if self.is_self_hosted() {
            bail!("pull request operations are only supported on Bitbucket Cloud");
        }
        let api_base = bitbucket_cloud_api_base(&self.base_url, remote)?;
        // Bitbucket Cloud has no unified review endpoint; each verdict maps to a
        // distinct action.
        let (url, json_body) = match verdict {
            PullRequestReviewVerdict::Approve => {
                (format!("{api_base}/pullrequests/{number}/approve"), None)
            }
            PullRequestReviewVerdict::RequestChanges => (
                format!("{api_base}/pullrequests/{number}/request-changes"),
                None,
            ),
            PullRequestReviewVerdict::Comment => {
                let raw = body
                    .map(|body| body.to_string())
                    .filter(|raw| !raw.trim().is_empty())
                    .context("Bitbucket requires a non-empty comment to leave review feedback")?;
                let payload = serde_json::to_vec(&serde_json::json!({ "content": { "raw": raw } }))?;
                (
                    format!("{api_base}/pullrequests/{number}/comments"),
                    Some(payload),
                )
            }
        };
        let request = bitbucket_request(BitbucketMethod::Post, &url, &auth, json_body)?;
        bitbucket_send(&http_client, request, "submitting Bitbucket review").await?;
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
        if self.is_self_hosted() {
            bail!("pull request operations are only supported on Bitbucket Cloud");
        }
        let api_base = bitbucket_cloud_api_base(&self.base_url, remote)?;
        let url = format!("{api_base}/pullrequests/{number}/comments");
        let payload = serde_json::json!({
            "content": { "raw": body.to_string() },
            "parent": { "id": in_reply_to },
        });
        let body = serde_json::to_vec(&payload)?;
        let request = bitbucket_request(BitbucketMethod::Post, &url, &auth, Some(body))?;
        bitbucket_send(&http_client, request, "posting Bitbucket review comment").await?;
        Ok(())
    }
}

enum BitbucketMethod {
    Get,
    Post,
}

fn bitbucket_cloud_api_base(base_url: &Url, remote: &ParsedGitRemote) -> Result<String> {
    let host = base_url
        .host_str()
        .context("Bitbucket base URL has no host")?;
    Ok(format!(
        "https://api.{host}/2.0/repositories/{owner}/{repo}",
        owner = remote.owner,
        repo = remote.repo,
    ))
}

/// Builds the `Authorization` header for a Bitbucket Cloud request. App
/// Passwords and Atlassian API tokens both authenticate via HTTP Basic; OAuth
/// access tokens use Bearer.
fn bitbucket_auth_header(auth: &Option<GitHostAuth>) -> Option<String> {
    match auth {
        Some(GitHostAuth::Basic { username, secret }) => {
            let encoded =
                base64::engine::general_purpose::STANDARD.encode(format!("{username}:{secret}"));
            Some(format!("Basic {encoded}"))
        }
        Some(GitHostAuth::Bearer(token)) => Some(format!("Bearer {token}")),
        None => None,
    }
}

fn bitbucket_request(
    method: BitbucketMethod,
    url: &str,
    auth: &Option<GitHostAuth>,
    json_body: Option<Vec<u8>>,
) -> Result<Request<AsyncBody>> {
    let builder = match method {
        BitbucketMethod::Get => Request::get(url),
        BitbucketMethod::Post => Request::post(url),
    };
    let mut builder = builder
        .header("Accept", "application/json")
        .header("User-Agent", "Lathe")
        .follow_redirects(http_client::RedirectPolicy::FollowAll);
    if let Some(value) = bitbucket_auth_header(auth) {
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

async fn bitbucket_send(
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

/// Fetches a Bitbucket Cloud paginated collection, following `next` links until
/// `max_items` are collected or the pages run out.
async fn bitbucket_get_paginated<T: DeserializeOwned>(
    client: &Arc<dyn HttpClient>,
    first_url: String,
    auth: &Option<GitHostAuth>,
    context: &str,
    max_items: usize,
) -> Result<Vec<T>> {
    #[derive(Deserialize)]
    struct Page<T> {
        #[serde(default = "Vec::new")]
        values: Vec<T>,
        #[serde(default)]
        next: Option<String>,
    }

    let mut items = Vec::new();
    let mut next = Some(first_url);
    while let Some(url) = next {
        let request = bitbucket_request(BitbucketMethod::Get, &url, auth, None)?;
        let bytes = bitbucket_send(client, request, context).await?;
        let page: Page<T> =
            serde_json::from_slice(&bytes).with_context(|| format!("parsing {context}"))?;
        items.extend(page.values);
        if items.len() >= max_items {
            break;
        }
        next = page.next;
    }
    Ok(items)
}

/// Maps a [`PullRequestListFilter`] to Bitbucket's repeatable `state` query
/// values. Bitbucket defaults to OPEN only, so an absent filter must list every
/// state explicitly.
fn bitbucket_states(filter: &PullRequestListFilter) -> Vec<&'static str> {
    match &filter.states {
        None => vec!["OPEN", "MERGED", "DECLINED", "SUPERSEDED"],
        Some(states) => {
            let mut result = Vec::new();
            for state in states {
                match state {
                    PullRequestState::Open => result.push("OPEN"),
                    PullRequestState::Merged => result.push("MERGED"),
                    PullRequestState::Closed => {
                        result.push("DECLINED");
                        result.push("SUPERSEDED");
                    }
                }
            }
            result
        }
    }
}

#[derive(Deserialize)]
struct BitbucketAccount {
    #[serde(default)]
    nickname: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
}

impl BitbucketAccount {
    fn login(&self) -> SharedString {
        self.nickname
            .clone()
            .or_else(|| self.display_name.clone())
            .map(SharedString::from)
            .unwrap_or_default()
    }

    /// The name Bitbucket shows for an `@`-mention (full display name preferred).
    fn mention_name(&self) -> Option<SharedString> {
        self.display_name
            .clone()
            .or_else(|| self.nickname.clone())
            .map(SharedString::from)
    }

    /// Normalized ids this account can be mentioned by, for matching the
    /// `@{...}` token in comment bodies against the thread participants.
    fn mention_keys(&self) -> Vec<String> {
        [self.account_id.as_deref(), self.uuid.as_deref()]
            .into_iter()
            .flatten()
            .map(normalize_mention_key)
            .filter(|key| !key.is_empty())
            .collect()
    }
}

#[derive(Deserialize)]
struct BitbucketNamed {
    name: String,
}

#[derive(Deserialize)]
struct BitbucketCommitRef {
    hash: String,
}

#[derive(Deserialize)]
struct BitbucketEndpoint {
    #[serde(default)]
    branch: Option<BitbucketNamed>,
    #[serde(default)]
    commit: Option<BitbucketCommitRef>,
}

#[derive(Deserialize)]
struct BitbucketLink {
    href: String,
}

#[derive(Deserialize)]
struct BitbucketLinks {
    #[serde(default)]
    html: Option<BitbucketLink>,
}

#[derive(Deserialize)]
struct BitbucketContent {
    #[serde(default)]
    raw: Option<String>,
}

#[derive(Deserialize)]
struct BitbucketPullRequest {
    id: u32,
    title: String,
    state: String,
    #[serde(default)]
    author: Option<BitbucketAccount>,
    #[serde(default)]
    source: Option<BitbucketEndpoint>,
    #[serde(default)]
    destination: Option<BitbucketEndpoint>,
    #[serde(default)]
    links: Option<BitbucketLinks>,
    #[serde(default)]
    updated_on: Option<String>,
    #[serde(default)]
    summary: Option<BitbucketContent>,
    #[serde(default)]
    draft: bool,
}

impl BitbucketPullRequest {
    fn pull_request_state(&self) -> PullRequestState {
        match self.state.as_str() {
            "MERGED" => PullRequestState::Merged,
            "DECLINED" | "SUPERSEDED" => PullRequestState::Closed,
            _ => PullRequestState::Open,
        }
    }

    fn branch_name(endpoint: &Option<BitbucketEndpoint>) -> SharedString {
        endpoint
            .as_ref()
            .and_then(|endpoint| endpoint.branch.as_ref())
            .map(|branch| SharedString::from(branch.name.clone()))
            .unwrap_or_default()
    }

    fn commit_hash(endpoint: &Option<BitbucketEndpoint>) -> SharedString {
        endpoint
            .as_ref()
            .and_then(|endpoint| endpoint.commit.as_ref())
            .map(|commit| SharedString::from(commit.hash.clone()))
            .unwrap_or_default()
    }

    fn html_url(&self) -> Result<Url> {
        let href = self
            .links
            .as_ref()
            .and_then(|links| links.html.as_ref())
            .map(|link| link.href.as_str())
            .context("Bitbucket pull request had no HTML link")?;
        Url::parse(href).context("parsing Bitbucket pull request URL")
    }

    fn into_summary(self, state: PullRequestState) -> Result<PullRequestSummary> {
        let author_login = self.author.as_ref().map(|a| a.login()).unwrap_or_default();
        let source_branch = Self::branch_name(&self.source);
        let target_branch = Self::branch_name(&self.destination);
        let url = self.html_url()?;
        let updated_at: SharedString = self.updated_on.clone().unwrap_or_default().into();
        Ok(PullRequestSummary {
            number: self.id,
            title: self.title.into(),
            author_login,
            state,
            source_branch,
            target_branch,
            url,
            updated_at,
            is_draft: self.draft,
        })
    }

    fn into_detail(self) -> Result<PullRequestDetail> {
        let state = self.pull_request_state();
        let author_login = self.author.as_ref().map(|a| a.login()).unwrap_or_default();
        let source_branch = Self::branch_name(&self.source);
        let target_branch = Self::branch_name(&self.destination);
        let head_sha = Self::commit_hash(&self.source);
        let base_sha = Self::commit_hash(&self.destination);
        let url = self.html_url()?;
        let updated_at: SharedString = self.updated_on.clone().unwrap_or_default().into();
        let body: SharedString = self
            .summary
            .as_ref()
            .and_then(|summary| summary.raw.clone())
            .unwrap_or_default()
            .into();
        Ok(PullRequestDetail {
            number: self.id,
            title: self.title.into(),
            body,
            state,
            author_login,
            source_branch,
            target_branch,
            head_sha,
            base_sha,
            url,
            updated_at,
            is_draft: self.draft,
            // Bitbucket does not expose mergeability or line-change stats on the
            // pull request object.
            is_mergeable: None,
            additions: 0,
            deletions: 0,
            changed_files: 0,
        })
    }
}

#[derive(Deserialize)]
struct BitbucketInline {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    to: Option<u32>,
    #[serde(default)]
    from: Option<u32>,
}

#[derive(Deserialize)]
struct BitbucketParent {
    id: u64,
}

/// Present on a thread's root comment once the thread has been resolved. Only
/// its presence matters here, so the body is intentionally empty (unknown JSON
/// fields are ignored).
#[derive(Deserialize)]
struct BitbucketResolution {}

#[derive(Deserialize)]
struct BitbucketComment {
    id: u64,
    #[serde(default)]
    user: Option<BitbucketAccount>,
    #[serde(default)]
    content: Option<BitbucketContent>,
    #[serde(default)]
    created_on: Option<String>,
    #[serde(default)]
    inline: Option<BitbucketInline>,
    /// Present on replies; references the comment this one replies to.
    #[serde(default)]
    parent: Option<BitbucketParent>,
    /// Present (on the thread root) once the thread is resolved.
    #[serde(default)]
    resolution: Option<BitbucketResolution>,
    #[serde(default)]
    links: Option<BitbucketLinks>,
    #[serde(default)]
    deleted: bool,
}

impl BitbucketComment {
    fn into_comment(self) -> Result<PullRequestReviewComment> {
        let author_login = self.user.as_ref().map(|user| user.login()).unwrap_or_default();
        let body: SharedString = self
            .content
            .as_ref()
            .and_then(|content| content.raw.clone())
            .unwrap_or_default()
            .into();
        let path: SharedString = self
            .inline
            .as_ref()
            .and_then(|inline| inline.path.clone())
            .unwrap_or_default()
            .into();
        let line = self
            .inline
            .as_ref()
            .and_then(|inline| inline.to.or(inline.from));
        let created_at: SharedString = self.created_on.clone().unwrap_or_default().into();
        let url = match self
            .links
            .as_ref()
            .and_then(|links| links.html.as_ref())
            .map(|link| link.href.clone())
        {
            Some(href) => Url::parse(&href).context("parsing Bitbucket comment URL")?,
            None => Url::parse("https://bitbucket.org").context("constructing Bitbucket URL")?,
        };
        Ok(PullRequestReviewComment {
            id: self.id,
            author_login,
            body,
            path,
            line,
            parent_id: self.parent.as_ref().map(|parent| parent.id),
            is_resolved: self.resolution.is_some(),
            created_at,
            url,
        })
    }
}

#[cfg(test)]
mod tests {
    use git::repository::repo_path;
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_parse_remote_url_given_ssh_url() {
        let parsed_remote = Bitbucket::public_instance()
            .parse_remote_url("git@bitbucket.org:zed-industries/zed.git")
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
        let parsed_remote = Bitbucket::public_instance()
            .parse_remote_url("https://bitbucket.org/zed-industries/zed.git")
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
        let parsed_remote = Bitbucket::public_instance()
            .parse_remote_url("https://thorstenballzed@bitbucket.org/zed-industries/zed.git")
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
    fn test_parse_remote_url_given_self_hosted_ssh_url() {
        let remote_url = "git@bitbucket.company.com:zed-industries/zed.git";

        let parsed_remote = Bitbucket::from_remote_url(remote_url)
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
    fn test_parse_remote_url_given_self_hosted_https_url() {
        let remote_url = "https://bitbucket.company.com/zed-industries/zed.git";

        let parsed_remote = Bitbucket::from_remote_url(remote_url)
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

        // Test with "scm" in the path
        let remote_url = "https://bitbucket.company.com/scm/zed-industries/zed.git";

        let parsed_remote = Bitbucket::from_remote_url(remote_url)
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

        // Test with only "scm" as owner
        let remote_url = "https://bitbucket.company.com/scm/zed.git";

        let parsed_remote = Bitbucket::from_remote_url(remote_url)
            .unwrap()
            .parse_remote_url(remote_url)
            .unwrap();

        assert_eq!(
            parsed_remote,
            ParsedGitRemote {
                owner: "scm".into(),
                repo: "zed".into(),
            }
        );
    }

    #[test]
    fn test_parse_remote_url_given_self_hosted_https_url_with_username() {
        let remote_url = "https://thorstenballzed@bitbucket.company.com/zed-industries/zed.git";

        let parsed_remote = Bitbucket::from_remote_url(remote_url)
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
    fn test_build_bitbucket_permalink() {
        let permalink = Bitbucket::public_instance().build_permalink(
            ParsedGitRemote {
                owner: "zed-industries".into(),
                repo: "zed".into(),
            },
            BuildPermalinkParams::new("f00b4r", &repo_path("main.rs"), None),
        );

        let expected_url = "https://bitbucket.org/zed-industries/zed/src/f00b4r/main.rs";
        assert_eq!(permalink.to_string(), expected_url.to_string())
    }

    #[test]
    fn test_build_bitbucket_self_hosted_permalink() {
        let permalink =
            Bitbucket::from_remote_url("git@bitbucket.company.com:zed-industries/zed.git")
                .unwrap()
                .build_permalink(
                    ParsedGitRemote {
                        owner: "zed-industries".into(),
                        repo: "zed".into(),
                    },
                    BuildPermalinkParams::new("f00b4r", &repo_path("main.rs"), None),
                );

        let expected_url = "https://bitbucket.company.com/projects/zed-industries/repos/zed/browse/main.rs?at=f00b4r";
        assert_eq!(permalink.to_string(), expected_url.to_string())
    }

    #[test]
    fn test_build_bitbucket_permalink_with_single_line_selection() {
        let permalink = Bitbucket::public_instance().build_permalink(
            ParsedGitRemote {
                owner: "zed-industries".into(),
                repo: "zed".into(),
            },
            BuildPermalinkParams::new("f00b4r", &repo_path("main.rs"), Some(6..6)),
        );

        let expected_url = "https://bitbucket.org/zed-industries/zed/src/f00b4r/main.rs#lines-7";
        assert_eq!(permalink.to_string(), expected_url.to_string())
    }

    #[test]
    fn test_build_bitbucket_self_hosted_permalink_with_single_line_selection() {
        let permalink =
            Bitbucket::from_remote_url("https://bitbucket.company.com/zed-industries/zed.git")
                .unwrap()
                .build_permalink(
                    ParsedGitRemote {
                        owner: "zed-industries".into(),
                        repo: "zed".into(),
                    },
                    BuildPermalinkParams::new("f00b4r", &repo_path("main.rs"), Some(6..6)),
                );

        let expected_url = "https://bitbucket.company.com/projects/zed-industries/repos/zed/browse/main.rs?at=f00b4r#7";
        assert_eq!(permalink.to_string(), expected_url.to_string())
    }

    #[test]
    fn test_build_bitbucket_permalink_with_multi_line_selection() {
        let permalink = Bitbucket::public_instance().build_permalink(
            ParsedGitRemote {
                owner: "zed-industries".into(),
                repo: "zed".into(),
            },
            BuildPermalinkParams::new("f00b4r", &repo_path("main.rs"), Some(23..47)),
        );

        let expected_url =
            "https://bitbucket.org/zed-industries/zed/src/f00b4r/main.rs#lines-24:48";
        assert_eq!(permalink.to_string(), expected_url.to_string())
    }

    #[test]
    fn test_build_bitbucket_self_hosted_permalink_with_multi_line_selection() {
        let permalink =
            Bitbucket::from_remote_url("git@bitbucket.company.com:zed-industries/zed.git")
                .unwrap()
                .build_permalink(
                    ParsedGitRemote {
                        owner: "zed-industries".into(),
                        repo: "zed".into(),
                    },
                    BuildPermalinkParams::new("f00b4r", &repo_path("main.rs"), Some(23..47)),
                );

        let expected_url = "https://bitbucket.company.com/projects/zed-industries/repos/zed/browse/main.rs?at=f00b4r#24-48";
        assert_eq!(permalink.to_string(), expected_url.to_string())
    }

    #[test]
    fn test_build_bitbucket_create_pr_url() {
        let remote = ParsedGitRemote {
            owner: "zed-industries".into(),
            repo: "zed".into(),
        };

        let url = Bitbucket::public_instance()
            .build_create_pull_request_url(&remote, "feature/my-branch")
            .expect("url should be constructed");

        assert_eq!(
            url.as_str(),
            "https://bitbucket.org/zed-industries/zed/pull-requests/new?source=feature%2Fmy-branch"
        );
    }

    #[test]
    fn test_build_bitbucket_self_hosted_create_pr_url() {
        let remote = ParsedGitRemote {
            owner: "zed-industries".into(),
            repo: "zed".into(),
        };

        let url =
            Bitbucket::from_remote_url("https://bitbucket.company.com/zed-industries/zed.git")
                .unwrap()
                .build_create_pull_request_url(&remote, "feature/my-branch")
                .expect("url should be constructed");

        assert_eq!(
            url.as_str(),
            "https://bitbucket.company.com/projects/zed-industries/repos/zed/compare/commits?sourceBranch=refs%2Fheads%2Ffeature%2Fmy-branch"
        );
    }

    #[test]
    fn test_bitbucket_pull_requests() {
        use indoc::indoc;

        let remote = ParsedGitRemote {
            owner: "zed-industries".into(),
            repo: "zed".into(),
        };

        let bitbucket = Bitbucket::public_instance();

        // Test message without PR reference
        let message = "This does not contain a pull request";
        assert!(bitbucket.extract_pull_request(&remote, message).is_none());

        // Pull request number at end of first line
        let message = indoc! {r#"
            Merged in feature-branch (pull request #123)

            Some detailed description of the changes.
        "#};

        let pr = bitbucket.extract_pull_request(&remote, message).unwrap();
        assert_eq!(pr.number, 123);
        assert_eq!(
            pr.url.as_str(),
            "https://bitbucket.org/zed-industries/zed/pull-requests/123"
        );
    }

    #[test]
    fn test_bitbucket_self_hosted_pull_requests() {
        use indoc::indoc;

        let remote = ParsedGitRemote {
            owner: "zed-industries".into(),
            repo: "zed".into(),
        };

        let bitbucket =
            Bitbucket::from_remote_url("https://bitbucket.company.com/zed-industries/zed.git")
                .unwrap();

        // Test message without PR reference
        let message = "This does not contain a pull request";
        assert!(bitbucket.extract_pull_request(&remote, message).is_none());

        // Pull request number at end of first line
        let message = indoc! {r#"
            Merged in feature-branch (pull request #123)

            Some detailed description of the changes.
        "#};

        let pr = bitbucket.extract_pull_request(&remote, message).unwrap();
        assert_eq!(pr.number, 123);
        assert_eq!(
            pr.url.as_str(),
            "https://bitbucket.company.com/projects/zed-industries/repos/zed/pull-requests/123"
        );
    }

    #[test]
    fn test_bitbucket_pull_request_into_summary() {
        let json = r#"{
            "id": 42,
            "title": "Add device flow",
            "state": "OPEN",
            "draft": true,
            "author": { "nickname": "octocat", "display_name": "Octo Cat" },
            "source": { "branch": { "name": "feature" }, "commit": { "hash": "aaa111" } },
            "destination": { "branch": { "name": "main" }, "commit": { "hash": "bbb222" } },
            "links": { "html": { "href": "https://bitbucket.org/owner/repo/pull-requests/42" } },
            "updated_on": "2026-01-02T03:04:05Z",
            "summary": { "raw": "Body text" }
        }"#;
        let pr: BitbucketPullRequest = serde_json::from_str(json).unwrap();
        let state = pr.pull_request_state();
        let summary = pr.into_summary(state).unwrap();

        assert_eq!(summary.number, 42);
        assert_eq!(summary.title.to_string(), "Add device flow");
        // nickname is preferred over display_name.
        assert_eq!(summary.author_login.to_string(), "octocat");
        assert_eq!(summary.state, PullRequestState::Open);
        assert_eq!(summary.source_branch.to_string(), "feature");
        assert_eq!(summary.target_branch.to_string(), "main");
        assert_eq!(
            summary.url.as_str(),
            "https://bitbucket.org/owner/repo/pull-requests/42"
        );
        assert!(summary.is_draft);
    }

    #[test]
    fn test_bitbucket_pull_request_into_detail_defaults_unsupported_fields() {
        let json = r#"{
            "id": 7,
            "title": "Ship it",
            "state": "MERGED",
            "author": { "display_name": "Octo Cat" },
            "source": { "branch": { "name": "topic" }, "commit": { "hash": "headsha" } },
            "destination": { "branch": { "name": "main" }, "commit": { "hash": "basesha" } },
            "links": { "html": { "href": "https://bitbucket.org/owner/repo/pull-requests/7" } },
            "updated_on": "2026-01-03T00:00:00Z",
            "summary": { "raw": "Detailed body" }
        }"#;
        let pr: BitbucketPullRequest = serde_json::from_str(json).unwrap();
        let detail = pr.into_detail().unwrap();

        assert_eq!(detail.state, PullRequestState::Merged);
        assert_eq!(detail.body.to_string(), "Detailed body");
        assert_eq!(detail.head_sha.to_string(), "headsha");
        assert_eq!(detail.base_sha.to_string(), "basesha");
        // display_name is used when nickname is absent.
        assert_eq!(detail.author_login.to_string(), "Octo Cat");
        // Bitbucket Cloud does not report these on the PR object.
        assert_eq!(detail.is_mergeable, None);
        assert_eq!(detail.additions, 0);
        assert_eq!(detail.deletions, 0);
        assert_eq!(detail.changed_files, 0);
    }

    #[test]
    fn test_bitbucket_pull_request_state_mapping() {
        let cases = [
            ("OPEN", PullRequestState::Open),
            ("MERGED", PullRequestState::Merged),
            ("DECLINED", PullRequestState::Closed),
            ("SUPERSEDED", PullRequestState::Closed),
        ];
        for (raw_state, expected) in cases {
            let json = format!(
                r#"{{
                    "id": 1,
                    "title": "t",
                    "state": "{raw_state}",
                    "links": {{ "html": {{ "href": "https://bitbucket.org/o/r/pull-requests/1" }} }}
                }}"#
            );
            let pr: BitbucketPullRequest = serde_json::from_str(&json).unwrap();
            assert_eq!(pr.pull_request_state(), expected, "state {raw_state}");
        }
    }

    #[test]
    fn test_bitbucket_comment_into_comment_prefers_to_line() {
        let json = r#"{
            "id": 9001,
            "user": { "nickname": "reviewer" },
            "content": { "raw": "Nit: rename this" },
            "created_on": "2026-01-04T05:06:07Z",
            "inline": { "path": "src/main.rs", "to": 120, "from": 110 },
            "links": { "html": { "href": "https://bitbucket.org/o/r/pull-requests/7/_/diff#comment-9001" } }
        }"#;
        let comment: BitbucketComment = serde_json::from_str(json).unwrap();
        let mapped = comment.into_comment().unwrap();

        assert_eq!(mapped.id, 9001);
        assert_eq!(mapped.author_login.to_string(), "reviewer");
        assert_eq!(mapped.body.to_string(), "Nit: rename this");
        assert_eq!(mapped.path.to_string(), "src/main.rs");
        // `to` is preferred over `from`.
        assert_eq!(mapped.line, Some(120));
    }

    #[test]
    fn test_bitbucket_comment_without_html_link_falls_back() {
        // No links and no inline anchor: must not panic; URL falls back to the host root.
        let json = r#"{
            "id": 5,
            "user": { "display_name": "Someone" },
            "content": { "raw": "General feedback" },
            "created_on": "2026-01-04T05:06:07Z"
        }"#;
        let comment: BitbucketComment = serde_json::from_str(json).unwrap();
        let mapped = comment.into_comment().unwrap();

        assert_eq!(mapped.author_login.to_string(), "Someone");
        assert_eq!(mapped.path.to_string(), "");
        assert_eq!(mapped.line, None);
        assert_eq!(mapped.url.as_str(), "https://bitbucket.org/");
    }
}
