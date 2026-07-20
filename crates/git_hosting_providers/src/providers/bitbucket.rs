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
use urlencoding::encode;
use util::ResultExt as _;

use git::{
    BuildCommitPermalinkParams, BuildPermalinkParams, DiffCommentSide, GitHostAuth,
    GitHostingProvider, ParsedGitRemote, PullRequest, PullRequestAuthError, PullRequestDetail,
    PullRequestListFilter, PullRequestMergeMethod, PullRequestReviewComment,
    PullRequestReviewVerdict, PullRequestReviewer, PullRequestState, PullRequestSummary, RemoteUrl,
};

use crate::get_host_from_git_remote_url;

#[path = "bitbucket_lathe.rs"]
mod lathe;

use lathe::*;

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
        let limit = filter.limit.unwrap_or(50);
        let viewer_uuid = if filter.reviewer_is_me || filter.author_is_me {
            Some(
                self.fetch_authenticated_uuid(&auth, &http_client)
                    .await?
                    .context(
                        "could not determine the authenticated Bitbucket user; \
                         the credential may lack the account scope",
                    )?,
            )
        } else {
            None
        };
        // "Awaiting my review" can't be filtered server-side: Bitbucket's query
        // language only indexes `reviewers`/`author`, not `participants`
        // (`participants.uuid` 400s), and the top-level `reviewers` array omits
        // still-awaiting default/group reviewers. So scan open PRs with their
        // participants pulled inline (`+values.participants`) and filter
        // client-side below. Scan well past `limit` since most rows get dropped.
        let scan_cap = if filter.reviewer_is_me || filter.author_is_me {
            (limit as usize).max(200)
        } else {
            limit as usize
        };
        let state_query = bitbucket_states(&filter)
            .iter()
            .map(|state| format!("state={state}"))
            .collect::<Vec<_>>()
            .join("&");
        let author_query = if filter.author_is_me {
            viewer_uuid
                .as_ref()
                .map(|uuid| format!("&q={}", encode(&format!("author.uuid=\"{uuid}\""))))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let first_url = if filter.reviewer_is_me {
            format!(
                "{api_base}/pullrequests?{state_query}{author_query}&pagelen=50&sort=-updated_on&fields=%2Bvalues.participants"
            )
        } else {
            format!(
                "{api_base}/pullrequests?{state_query}{author_query}&pagelen=50&sort=-updated_on"
            )
        };
        let mut raw: Vec<BitbucketPullRequest> = bitbucket_get_paginated(
            &http_client,
            first_url,
            &auth,
            "listing Bitbucket pull requests",
            scan_cap,
        )
        .await?;

        // Fallback: some Bitbucket responses ignore `+values.participants` on the
        // list endpoint (participants only ride along on the single-PR detail). If
        // no row carried any participants, hydrate them per-PR with bounded
        // concurrency so the reviewer filter still has data to work with.
        if filter.reviewer_is_me
            && !raw.is_empty()
            && raw.iter().all(|pr| pr.participants.is_empty())
        {
            let mut hydrated: HashMap<u32, Vec<BitbucketParticipant>> = HashMap::new();
            for chunk in raw.chunks(8) {
                let fetches = chunk.iter().map(|pr| {
                    let http_client = http_client.clone();
                    let auth = auth.clone();
                    let url = format!("{api_base}/pullrequests/{}", pr.id);
                    let id = pr.id;
                    async move {
                        let request = bitbucket_request(BitbucketMethod::Get, &url, &auth, None)?;
                        let bytes = bitbucket_send(
                            &http_client,
                            request,
                            "hydrating Bitbucket pull request participants",
                        )
                        .await?;
                        let detail: BitbucketPullRequest = serde_json::from_slice(&bytes)
                            .context("parsing Bitbucket pull request")?;
                        anyhow::Ok((id, detail.participants))
                    }
                });
                for (id, participants) in futures::future::join_all(fetches)
                    .await
                    .into_iter()
                    .flatten()
                {
                    hydrated.insert(id, participants);
                }
            }
            for pr in &mut raw {
                if let Some(participants) = hydrated.remove(&pr.id) {
                    pr.participants = participants;
                }
            }
        }

        let mut summaries = Vec::new();
        for pr in raw {
            let pr_state = pr.pull_request_state();
            // Bitbucket drops the top-level `state=` param once a `q` BBQL query
            // is present (e.g. the `author.uuid` clause for author_is_me), so the
            // server may return other states. Re-apply the state filter here,
            // mirroring the GitHub provider, so the requested states are honored.
            if let Some(states) = &filter.states
                && !states.contains(&pr_state)
            {
                continue;
            }
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
            // "My reviews": keep every PR where I'm a reviewer, whether or not I
            // have already voted. The panel distinguishes approved /
            // changes-requested / still-pending from each reviewer's verdict.
            if filter.reviewer_is_me
                && let Some(uuid) = &viewer_uuid
            {
                if !pr.is_reviewer(uuid) {
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
        let bytes =
            bitbucket_send(&http_client, request, "fetching Bitbucket pull request").await?;
        let pr: BitbucketPullRequest =
            serde_json::from_slice(&bytes).context("parsing Bitbucket pull request")?;
        // Resolve the viewer's own review from the participants before consuming
        // `pr`. Best-effort: failing to resolve the account uuid just leaves the
        // review state unknown rather than failing the load.
        let viewer_uuid = self
            .fetch_authenticated_uuid(&auth, &http_client)
            .await
            .log_err()
            .flatten();
        let viewer_review = viewer_uuid
            .as_deref()
            .and_then(|uuid| pr.viewer_review(uuid));
        let reviewers = pr.reviewers(viewer_uuid.as_deref());
        let mut detail = pr.into_detail()?;
        detail.viewer_review = viewer_review;
        detail.reviewers = reviewers;
        // The PR object carries no commit count, so fetch the commits and count
        // them. This is best-effort: a failure leaves `commits` unset rather than
        // failing the whole PR load. `max_items` bounds the walk for very large
        // PRs, in which case the count is a lower bound.
        let commits_url = format!("{api_base}/pullrequests/{number}/commits?pagelen=100");
        detail.commits = bitbucket_get_paginated::<BitbucketCommitId>(
            &http_client,
            commits_url,
            &auth,
            "fetching Bitbucket pull request commits",
            250,
        )
        .await
        .log_err()
        .map(|commits| commits.len() as u32);
        // Best-effort "behind by N commits": commits reachable from the target
        // branch but not from the source branch. Bitbucket exposes no divergence
        // field, so list them (bounded) and count. A failure leaves it unset.
        let behind_url = format!(
            "{api_base}/commits?include={target}&exclude={source}&pagelen=100",
            target = encode(&detail.target_branch),
            source = encode(&detail.source_branch),
        );
        detail.behind_by = bitbucket_get_paginated::<BitbucketCommitId>(
            &http_client,
            behind_url,
            &auth,
            "counting Bitbucket commits behind base",
            250,
        )
        .await
        .log_err()
        .map(|commits| commits.len() as u32);
        Ok(detail)
    }

    async fn pull_request_reviewers(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<Vec<PullRequestReviewer>> {
        if self.is_self_hosted() {
            return Ok(Vec::new());
        }
        let api_base = bitbucket_cloud_api_base(&self.base_url, remote)?;
        let url = format!("{api_base}/pullrequests/{number}");
        let request = bitbucket_request(BitbucketMethod::Get, &url, &auth, None)?;
        let bytes =
            bitbucket_send(&http_client, request, "fetching Bitbucket pull request").await?;
        let pr: BitbucketPullRequest =
            serde_json::from_slice(&bytes).context("parsing Bitbucket pull request")?;
        let viewer_uuid = self
            .fetch_authenticated_uuid(&auth, &http_client)
            .await
            .log_err()
            .flatten();
        Ok(pr.reviewers(viewer_uuid.as_deref()))
    }

    async fn pull_request_review_state(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<Option<PullRequestReviewVerdict>> {
        if self.is_self_hosted() {
            return Ok(None);
        }
        let Some(uuid) = self.fetch_authenticated_uuid(&auth, &http_client).await? else {
            return Ok(None);
        };
        let api_base = bitbucket_cloud_api_base(&self.base_url, remote)?;
        let url = format!("{api_base}/pullrequests/{number}");
        let request = bitbucket_request(BitbucketMethod::Get, &url, &auth, None)?;
        let bytes =
            bitbucket_send(&http_client, request, "fetching Bitbucket pull request").await?;
        let pr: BitbucketPullRequest =
            serde_json::from_slice(&bytes).context("parsing Bitbucket pull request")?;
        Ok(pr.viewer_review(uuid.as_ref()))
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
        let bytes = bitbucket_send(
            &http_client,
            request,
            "fetching Bitbucket pull request diff",
        )
        .await?;
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
        let bytes =
            bitbucket_send(&http_client, request, "fetching Bitbucket file content").await?;
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
                let payload =
                    serde_json::to_vec(&serde_json::json!({ "content": { "raw": raw } }))?;
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

    async fn create_review_comment(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        _commit_id: SharedString,
        path: SharedString,
        line: u32,
        side: DiffCommentSide,
        body: SharedString,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<()> {
        if self.is_self_hosted() {
            bail!("pull request operations are only supported on Bitbucket Cloud");
        }
        let api_base = bitbucket_cloud_api_base(&self.base_url, remote)?;
        let url = format!("{api_base}/pullrequests/{number}/comments");
        // Bitbucket anchors an inline comment by file path and a line number on
        // one side of the diff: `to` is the destination (post-image) line, `from`
        // is the source (pre-image) line.
        let inline = match side {
            DiffCommentSide::Right => serde_json::json!({ "path": path.to_string(), "to": line }),
            DiffCommentSide::Left => serde_json::json!({ "path": path.to_string(), "from": line }),
        };
        let payload = serde_json::json!({
            "content": { "raw": body.to_string() },
            "inline": inline,
        });
        let body = serde_json::to_vec(&payload)?;
        let request = bitbucket_request(BitbucketMethod::Post, &url, &auth, Some(body))?;
        bitbucket_send(&http_client, request, "creating Bitbucket review comment").await?;
        Ok(())
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
    fn test_bitbucket_list_filters_to_authenticated_author() {
        let client: Arc<dyn HttpClient> =
            http_client::FakeHttpClient::create(|request| async move {
                let path = request.uri().path();
                let body = if path == "/2.0/user" {
                    r#"{"uuid":"{viewer}"}"#
                } else if path.ends_with("/pullrequests") {
                    let query = request.uri().query().unwrap_or_default();
                    assert!(query.contains("q=author.uuid%3D%22%7Bviewer%7D%22"));
                    r#"{"values":[
                    {"id":1,"title":"Mine","state":"OPEN",
                     "author":{"display_name":"Me","uuid":"{viewer}"},
                     "source":{"branch":{"name":"feature"},"commit":{"hash":"a1"}},
                     "destination":{"branch":{"name":"main"},"commit":{"hash":"b1"}},
                     "links":{"html":{"href":"https://bitbucket.org/owner/repo/pull-requests/1"}},
                     "updated_on":"2026-01-02T00:00:00Z"}
                ]}"#
                } else {
                    r#"{"values":[]}"#
                };
                Ok(http_client::Response::builder()
                    .status(200)
                    .body(body.into())
                    .unwrap())
            });

        let remote = ParsedGitRemote {
            owner: "owner".into(),
            repo: "repo".into(),
        };
        let filter = PullRequestListFilter {
            states: Some(vec![PullRequestState::Open]),
            author: None,
            reviewer_is_me: false,
            author_is_me: true,
            limit: Some(50),
        };
        let summaries =
            futures::executor::block_on(Bitbucket::public_instance().list_pull_requests(
                &remote,
                filter,
                Some(GitHostAuth::Basic {
                    username: "user".into(),
                    secret: "secret".into(),
                }),
                client,
            ))
            .unwrap();

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].number, 1);
        assert_eq!(summaries[0].author_login.to_string(), "Me");
    }

    #[test]
    fn test_bitbucket_reviewer_is_me_includes_already_voted() {
        // A PR the viewer has already approved must still surface in "my open
        // reviews" mode (distinguished later by verdict); a PR the viewer does
        // not review is excluded.
        let client: Arc<dyn HttpClient> =
            http_client::FakeHttpClient::create(|request| async move {
                let path = request.uri().path();
                let body = if path == "/2.0/user" {
                    r#"{"uuid":"{viewer}"}"#
                } else if path.ends_with("/pullrequests") {
                    r#"{"values":[
                    {"id":1,"title":"Approved by me","state":"OPEN",
                     "author":{"display_name":"Author","uuid":"{author}"},
                     "source":{"branch":{"name":"feature"},"commit":{"hash":"a1"}},
                     "destination":{"branch":{"name":"main"},"commit":{"hash":"b1"}},
                     "links":{"html":{"href":"https://bitbucket.org/owner/repo/pull-requests/1"}},
                     "updated_on":"2026-01-02T00:00:00Z",
                     "participants":[
                         {"role":"REVIEWER","approved":true,"state":"approved",
                          "user":{"display_name":"Me","uuid":"{viewer}"}}
                     ]},
                    {"id":2,"title":"Not mine","state":"OPEN",
                     "author":{"display_name":"Author","uuid":"{author}"},
                     "source":{"branch":{"name":"feature2"},"commit":{"hash":"c1"}},
                     "destination":{"branch":{"name":"main"},"commit":{"hash":"b2"}},
                     "links":{"html":{"href":"https://bitbucket.org/owner/repo/pull-requests/2"}},
                     "updated_on":"2026-01-01T00:00:00Z",
                     "participants":[
                         {"role":"REVIEWER","approved":false,
                          "user":{"display_name":"Someone","uuid":"{other}"}}
                     ]}
                ]}"#
                } else {
                    r#"{"values":[]}"#
                };
                Ok(http_client::Response::builder()
                    .status(200)
                    .body(body.into())
                    .unwrap())
            });

        let remote = ParsedGitRemote {
            owner: "owner".into(),
            repo: "repo".into(),
        };
        let filter = PullRequestListFilter {
            states: Some(vec![PullRequestState::Open]),
            author: None,
            reviewer_is_me: true,
            author_is_me: false,
            limit: Some(50),
        };
        let summaries =
            futures::executor::block_on(Bitbucket::public_instance().list_pull_requests(
                &remote,
                filter,
                Some(GitHostAuth::Basic {
                    username: "user".into(),
                    secret: "secret".into(),
                }),
                client,
            ))
            .unwrap();

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].number, 1);
    }

    #[test]
    fn test_bitbucket_pull_request_reviewers_marks_viewer_and_pending() {
        let client: Arc<dyn HttpClient> =
            http_client::FakeHttpClient::create(|request| async move {
                let path = request.uri().path();
                let body = if path == "/2.0/user" {
                    r#"{"uuid":"{viewer}"}"#
                } else if path.ends_with("/pullrequests/7") {
                    r#"{"id":7,"title":"Reviewers","state":"OPEN",
                    "author":{"display_name":"Author","uuid":"{author}"},
                    "source":{"branch":{"name":"feature"},"commit":{"hash":"a1"}},
                    "destination":{"branch":{"name":"main"},"commit":{"hash":"b1"}},
                    "links":{"html":{"href":"https://bitbucket.org/owner/repo/pull-requests/7"}},
                    "participants":[
                        {"role":"REVIEWER","approved":true,"state":"approved",
                         "user":{"display_name":"Me","uuid":"{viewer}"}},
                        {"role":"REVIEWER","approved":false,"state":"changes_requested",
                         "user":{"display_name":"Blocker","uuid":"{blocker}"}},
                        {"role":"REVIEWER","approved":false,
                         "user":{"display_name":"Pending","uuid":"{pending}"}},
                        {"role":"PARTICIPANT","approved":true,
                         "user":{"display_name":"Participant","uuid":"{participant}"}}
                    ]}"#
                } else {
                    r#"{"values":[]}"#
                };
                Ok(http_client::Response::builder()
                    .status(200)
                    .body(body.into())
                    .unwrap())
            });

        let remote = ParsedGitRemote {
            owner: "owner".into(),
            repo: "repo".into(),
        };
        let reviewers =
            futures::executor::block_on(Bitbucket::public_instance().pull_request_reviewers(
                &remote,
                7,
                Some(GitHostAuth::Basic {
                    username: "user".into(),
                    secret: "secret".into(),
                }),
                client,
            ))
            .unwrap();

        assert_eq!(reviewers.len(), 3);
        assert_eq!(reviewers[0].login.to_string(), "Me");
        assert_eq!(
            reviewers[0].verdict,
            Some(PullRequestReviewVerdict::Approve)
        );
        assert!(reviewers[0].is_me);
        assert_eq!(
            reviewers[1].verdict,
            Some(PullRequestReviewVerdict::RequestChanges)
        );
        assert_eq!(reviewers[2].login.to_string(), "Pending");
        assert_eq!(reviewers[2].verdict, None);
        assert!(!reviewers[2].is_me);
    }

    #[test]
    fn test_bitbucket_pull_request_reviewers_without_auth_does_not_mark_me() {
        let client: Arc<dyn HttpClient> =
            http_client::FakeHttpClient::create(|request| async move {
                let path = request.uri().path();
                let (status, body) = if path == "/2.0/user" {
                    (401, r#"{"error":{"message":"bad credentials"}}"#)
                } else if path.ends_with("/pullrequests/7") {
                    (
                        200,
                        r#"{"id":7,"title":"Reviewers","state":"OPEN",
                    "author":{"display_name":"Author","uuid":"{author}"},
                    "source":{"branch":{"name":"feature"},"commit":{"hash":"a1"}},
                    "destination":{"branch":{"name":"main"},"commit":{"hash":"b1"}},
                    "links":{"html":{"href":"https://bitbucket.org/owner/repo/pull-requests/7"}},
                    "participants":[
                        {"role":"REVIEWER","approved":true,"state":"approved",
                         "user":{"display_name":"Me","uuid":"{viewer}"}}
                    ]}"#,
                    )
                } else {
                    (200, r#"{"values":[]}"#)
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
        let reviewers = futures::executor::block_on(
            Bitbucket::public_instance().pull_request_reviewers(&remote, 7, None, client),
        )
        .unwrap();

        assert_eq!(reviewers.len(), 1);
        assert_eq!(reviewers[0].login.to_string(), "Me");
        assert!(!reviewers[0].is_me);
    }
}
