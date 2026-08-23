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
    BuildCommitPermalinkParams, BuildPermalinkParams, DiffCommentSide, GitHostAuth,
    GitHostAuthKind, GitHostingProvider, ParsedGitRemote, PullRequest, PullRequestAuthError,
    NewPullRequest, PullRequestChecks, PullRequestDetail, PullRequestListFilter, PullRequestMergeMethod,
    PullRequestReviewComment, PullRequestReviewVerdict, PullRequestReviewer, PullRequestState,
    PullRequestSummary, RemoteUrl,
};

fn merge_request_number_regex() -> &'static Regex {
    static MERGE_REQUEST_NUMBER_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        // Matches GitLab MR references:
        // - "(!123)" at the end of line (squash merge pattern)
        // - "See merge request group/project!123" (standard merge commit)
        Regex::new(r"(?:\(!(\d+)\)$|See merge request [^\s]+!(\d+))").unwrap()
    });
    &MERGE_REQUEST_NUMBER_REGEX
}

use util::ResultExt as _;

use crate::get_host_from_git_remote_url;

#[path = "gitlab_lathe.rs"]
mod lathe;

use lathe::*;

#[derive(Debug, Deserialize)]
struct CommitDetails {
    author_email: String,
}

#[derive(Debug, Deserialize)]
struct AvatarInfo {
    avatar_url: String,
}

#[derive(Debug)]
pub struct Gitlab {
    name: String,
    base_url: Url,
}

impl Gitlab {
    pub fn new(name: impl Into<String>, base_url: Url) -> Self {
        Self {
            name: name.into(),
            base_url,
        }
    }

    pub fn public_instance() -> Self {
        Self::new("GitLab", Url::parse("https://gitlab.com").unwrap())
    }

    pub fn from_remote_url(remote_url: &str) -> Result<Self> {
        let host = get_host_from_git_remote_url(remote_url)?;
        if host == "gitlab.com" {
            bail!("the GitLab instance is not self-hosted");
        }

        // TODO: detecting self hosted instances by checking whether "gitlab" is in the url or not
        // is not very reliable. See https://github.com/zed-industries/zed/issues/26393 for more
        // information.
        if !host.contains("gitlab") {
            bail!("not a GitLab URL");
        }

        Ok(Self::new(
            "GitLab Self-Hosted",
            Url::parse(&format!("https://{}", host))?,
        ))
    }

    async fn fetch_gitlab_commit_author(
        &self,
        repo_owner: &str,
        repo: &str,
        commit: &str,
        client: &Arc<dyn HttpClient>,
    ) -> Result<Option<AvatarInfo>> {
        let Some(host) = self.base_url.host_str() else {
            bail!("failed to get host from gitlab base url");
        };
        let project_path = format!("{}/{}", repo_owner, repo);
        let project_path_encoded = urlencoding::encode(&project_path);
        let url = format!(
            "https://{host}/api/v4/projects/{project_path_encoded}/repository/commits/{commit}"
        );

        let request = Request::get(&url)
            .header("Content-Type", "application/json")
            .follow_redirects(http_client::RedirectPolicy::FollowAll);

        let mut response = client
            .send(request.body(AsyncBody::default())?)
            .await
            .with_context(|| format!("error fetching GitLab commit details at {:?}", url))?;

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

        let author_email = serde_json::from_str::<CommitDetails>(body_str)
            .map(|commit| commit.author_email)
            .context("failed to deserialize GitLab commit details")?;

        let avatar_info_url = format!("https://{host}/api/v4/avatar?email={author_email}");

        let request = Request::get(&avatar_info_url)
            .header("Content-Type", "application/json")
            .follow_redirects(http_client::RedirectPolicy::FollowAll);

        let mut response = client
            .send(request.body(AsyncBody::default())?)
            .await
            .with_context(|| format!("error fetching GitLab avatar info at {:?}", url))?;

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

        serde_json::from_str::<Option<AvatarInfo>>(body_str)
            .context("failed to deserialize GitLab avatar info")
    }
}

#[async_trait]
impl GitHostingProvider for Gitlab {
    fn auth_kind(&self) -> Option<GitHostAuthKind> {
        Some(GitHostAuthKind::GitLab)
    }

    async fn fetch_authenticated_user(
        &self,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<Option<SharedString>> {
        self.fetch_authenticated_username(&auth, &http_client).await
    }

    async fn list_pull_requests(
        &self,
        remote: &ParsedGitRemote,
        filter: PullRequestListFilter,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<Vec<PullRequestSummary>> {
        let api = self.api_base()?;
        let project = gitlab_project_id(remote);
        let limit = filter.limit.unwrap_or(50);

        // GitLab filters "mine" server-side by username, unlike the other
        // providers, so no wide client-side scan is needed.
        let username = if filter.reviewer_is_me || filter.author_is_me {
            Some(
                self.fetch_authenticated_username(&auth, &http_client)
                    .await?
                    .context("could not determine the authenticated GitLab user")?,
            )
        } else {
            None
        };

        let mut query = vec![
            format!("per_page={}", limit.clamp(1, 100)),
            format!("page={}", filter.page.unwrap_or(1).max(1)),
            "order_by=updated_at".to_string(),
            "sort=desc".to_string(),
        ];
        // GitLab's `state` takes a single value, so a multi-state request has to
        // fetch everything and filter below.
        if let Some(states) = &filter.states
            && states.len() == 1
        {
            let state = match states[0] {
                PullRequestState::Open => "opened",
                PullRequestState::Closed => "closed",
                PullRequestState::Merged => "merged",
            };
            query.push(format!("state={state}"));
        }
        if let Some(username) = &username {
            if filter.reviewer_is_me {
                query.push(format!("reviewer_username={}", encode(username)));
            }
            if filter.author_is_me {
                query.push(format!("author_username={}", encode(username)));
            }
        }
        let url = format!(
            "{api}/projects/{project}/merge_requests?{}",
            query.join("&")
        );
        let request = gitlab_request(GitlabMethod::Get, &url, &auth, None)?;
        let bytes = gitlab_send(
            &http_client,
            request,
            &self.api_host(),
            "listing GitLab merge requests",
        )
        .await?;
        let raw: Vec<GitlabMergeRequest> =
            serde_json::from_slice(&bytes).context("parsing GitLab merge request list")?;

        let mut summaries = Vec::new();
        for merge_request in raw {
            let state = merge_request.pull_request_state();
            if let Some(states) = &filter.states
                && !states.contains(&state)
            {
                continue;
            }
            if let Some(author) = &filter.author
                && !merge_request
                    .author_login()
                    .to_lowercase()
                    .contains(&author.to_lowercase())
            {
                continue;
            }
            summaries.push(merge_request.into_summary(state)?);
            if summaries.len() as u32 >= limit {
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
        let project = gitlab_project_id(remote);
        // GitLab marks a draft by title prefix rather than a flag.
        let title = if request.is_draft && !request.title.starts_with("Draft:") {
            format!("Draft: {}", request.title)
        } else {
            request.title.to_string()
        };
        let body = serde_json::json!({
            "title": title,
            "description": request.body.to_string(),
            "source_branch": request.source_branch.to_string(),
            "target_branch": request.target_branch.to_string(),
        });
        let url = format!("{api}/projects/{project}/merge_requests");
        let http_request = gitlab_request(
            GitlabMethod::Post,
            &url,
            &auth,
            Some(serde_json::to_vec(&body)?),
        )?;
        let bytes = gitlab_send(
            &http_client,
            http_request,
            &self.api_host(),
            "creating GitLab merge request",
        )
        .await?;
        let created: GitlabMergeRequest =
            serde_json::from_slice(&bytes).context("parsing created GitLab merge request")?;
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
        let project = gitlab_project_id(remote);
        let url = format!("{api}/projects/{project}");
        let request = gitlab_request(GitlabMethod::Get, &url, &auth, None)?;
        let bytes = gitlab_send(
            &http_client,
            request,
            &self.api_host(),
            "fetching GitLab project",
        )
        .await?;
        let project: GitlabProject =
            serde_json::from_slice(&bytes).context("parsing GitLab project")?;
        Ok(project.default_branch.map(SharedString::from))
    }

    async fn get_pull_request(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<PullRequestDetail> {
        let api = self.api_base()?;
        let project = gitlab_project_id(remote);
        let url = format!("{api}/projects/{project}/merge_requests/{number}");
        let request = gitlab_request(GitlabMethod::Get, &url, &auth, None)?;
        let bytes = gitlab_send(
            &http_client,
            request,
            &self.api_host(),
            "fetching GitLab merge request",
        )
        .await?;
        let merge_request: GitlabMergeRequest =
            serde_json::from_slice(&bytes).context("parsing GitLab merge request")?;
        let mut detail = merge_request.into_detail()?;

        // Best-effort enrichment, each independently allowed to fail so a
        // restricted token still renders the header.
        if let Some(reviewers) = self
            .fetch_reviewers(remote, number, &auth, &http_client)
            .await
            .log_err()
        {
            detail.viewer_review = reviewers
                .iter()
                .find(|reviewer| reviewer.is_me)
                .and_then(|reviewer| reviewer.verdict);
            detail.reviewers = reviewers;
        }
        detail.checks = self
            .fetch_checks(remote, number, &auth, &http_client)
            .await
            .log_err()
            .flatten();
        Ok(detail)
    }

    async fn get_pull_request_diff(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<String> {
        let api = self.api_base()?;
        let project = gitlab_project_id(remote);
        // GitLab exposes no whole-patch download, so collect the per-file diffs
        // and reassemble the unified diff the view expects.
        let url = format!("{api}/projects/{project}/merge_requests/{number}/diffs");
        let files: Vec<GitlabDiffFile> = gitlab_get_paginated(
            &http_client,
            &url,
            &auth,
            &self.api_host(),
            "fetching GitLab merge request diff",
            1000,
        )
        .await?;
        Ok(gitlab_unified_diff(&files))
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
        let project = gitlab_project_id(remote);
        let encoded_path = encode(path);
        let url = format!(
            "{api}/projects/{project}/repository/files/{encoded_path}/raw?ref={}",
            encode(revision)
        );
        let request = gitlab_request(GitlabMethod::Get, &url, &auth, None)?;
        let bytes = gitlab_send(
            &http_client,
            request,
            &self.api_host(),
            "fetching GitLab file content",
        )
        .await?;
        String::from_utf8(bytes).context("GitLab file content was not valid UTF-8")
    }

    async fn get_pull_request_comments(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<Vec<PullRequestReviewComment>> {
        let api = self.api_base()?;
        let project = gitlab_project_id(remote);
        let url = format!("{api}/projects/{project}/merge_requests/{number}/discussions");
        let discussions: Vec<GitlabDiscussion> = gitlab_get_paginated(
            &http_client,
            &url,
            &auth,
            &self.api_host(),
            "fetching GitLab merge request discussions",
            500,
        )
        .await?;

        let mut comments = Vec::new();
        for discussion in discussions {
            // The first non-system note roots the thread and carries the diff
            // position; later notes are replies to it.
            let mut root_id: Option<u64> = None;
            for note in discussion.notes {
                // System notes are GitLab's own activity entries ("changed the
                // description"), not review feedback.
                if note.system {
                    continue;
                }
                let position = note.position.as_ref();
                let path = position
                    .and_then(|position| {
                        position.new_path.clone().or_else(|| position.old_path.clone())
                    })
                    .unwrap_or_default();
                // Only positioned notes anchor to the diff; an unpositioned one
                // is a general MR comment, which the view renders at file top.
                let line = position.and_then(|position| position.new_line.or(position.old_line));
                comments.push(PullRequestReviewComment {
                    id: note.id,
                    author_login: note
                        .author
                        .as_ref()
                        .map(|user| SharedString::from(user.username.clone()))
                        .unwrap_or_default(),
                    body: note.body.into(),
                    path: path.into(),
                    line,
                    parent_id: root_id,
                    is_resolved: root_id.is_none() && note.resolved,
                    created_at: note.created_at.into(),
                    url: self
                        .base_url
                        .join(&format!(
                            "{}/{}/-/merge_requests/{number}#note_{}",
                            remote.owner, remote.repo, note.id
                        ))
                        .unwrap_or_else(|_| self.base_url.clone()),
                });
                root_id.get_or_insert(note.id);
            }
        }
        Ok(comments)
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
        let project = gitlab_project_id(remote);
        // GitLab expresses merge strategy through project settings and a squash
        // flag rather than a per-request method. Rebase is a distinct endpoint
        // that only rebases, so it is rejected rather than silently merging.
        let body = match method {
            PullRequestMergeMethod::Merge => serde_json::json!({ "squash": false }),
            PullRequestMergeMethod::Squash => serde_json::json!({ "squash": true }),
            PullRequestMergeMethod::Rebase => {
                bail!(
                    "GitLab does not support rebase-merging from the API; \
                     set the project's merge method to fast-forward instead"
                )
            }
        };
        let url = format!("{api}/projects/{project}/merge_requests/{number}/merge");
        let request = gitlab_request(
            GitlabMethod::Put,
            &url,
            &auth,
            Some(serde_json::to_vec(&body)?),
        )?;
        gitlab_send(
            &http_client,
            request,
            &self.api_host(),
            "merging GitLab merge request",
        )
        .await?;
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
        let project = gitlab_project_id(remote);
        // Leave the summary comment first so it lands whichever verdict follows.
        if let Some(body) = body.filter(|body| !body.is_empty()) {
            let url = format!("{api}/projects/{project}/merge_requests/{number}/notes");
            let payload = serde_json::json!({ "body": body.to_string() });
            let request = gitlab_request(
                GitlabMethod::Post,
                &url,
                &auth,
                Some(serde_json::to_vec(&payload)?),
            )?;
            gitlab_send(
                &http_client,
                request,
                &self.api_host(),
                "posting GitLab merge request note",
            )
            .await?;
        }

        match verdict {
            PullRequestReviewVerdict::Approve => {
                let url = format!("{api}/projects/{project}/merge_requests/{number}/approve");
                let request = gitlab_request(GitlabMethod::Post, &url, &auth, None)?;
                gitlab_send(
                    &http_client,
                    request,
                    &self.api_host(),
                    "approving GitLab merge request",
                )
                .await?;
            }
            // GitLab models blocking review as unresolved threads plus a removed
            // approval, not as a review verdict. Withdrawing any approval is the
            // closest faithful action; the note above carries the reasoning.
            PullRequestReviewVerdict::RequestChanges => {
                let url = format!("{api}/projects/{project}/merge_requests/{number}/unapprove");
                let request = gitlab_request(GitlabMethod::Post, &url, &auth, None)?;
                gitlab_send(
                    &http_client,
                    request,
                    &self.api_host(),
                    "withdrawing approval on GitLab merge request",
                )
                .await?;
            }
            // A comment-only review is exactly the note posted above.
            PullRequestReviewVerdict::Comment => {}
        }
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
        // Only an approval can be retracted; the other verdicts leave no state
        // on GitLab to remove.
        if verdict != PullRequestReviewVerdict::Approve {
            return Ok(());
        }
        let api = self.api_base()?;
        let project = gitlab_project_id(remote);
        let url = format!("{api}/projects/{project}/merge_requests/{number}/unapprove");
        let request = gitlab_request(GitlabMethod::Post, &url, &auth, None)?;
        gitlab_send(
            &http_client,
            request,
            &self.api_host(),
            "removing approval on GitLab merge request",
        )
        .await?;
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
        let api = self.api_base()?;
        let project = gitlab_project_id(remote);
        // Replies address the discussion, not the note, and a comment only
        // carries its note id. Re-read the discussions and find the one holding
        // that note: one extra request, but stateless and always correct, where
        // caching the mapping would go stale the moment the thread changes.
        let discussion_id = self
            .find_discussion_for_note(remote, number, in_reply_to, &auth, &http_client)
            .await?;
        let url = format!(
            "{api}/projects/{project}/merge_requests/{number}/discussions/{discussion_id}/notes"
        );
        let payload = serde_json::json!({ "body": body.to_string() });
        let request = gitlab_request(
            GitlabMethod::Post,
            &url,
            &auth,
            Some(serde_json::to_vec(&payload)?),
        )?;
        gitlab_send(
            &http_client,
            request,
            &self.api_host(),
            "replying to GitLab discussion",
        )
        .await?;
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
        let api = self.api_base()?;
        let project = gitlab_project_id(remote);
        // A positioned note needs the three shas that bound the diff, which only
        // the merge request itself reports.
        let url = format!("{api}/projects/{project}/merge_requests/{number}");
        let request = gitlab_request(GitlabMethod::Get, &url, &auth, None)?;
        let bytes = gitlab_send(
            &http_client,
            request,
            &self.api_host(),
            "fetching GitLab merge request",
        )
        .await?;
        let merge_request: GitlabMergeRequest =
            serde_json::from_slice(&bytes).context("parsing GitLab merge request")?;
        let diff_refs = merge_request
            .diff_refs
            .clone()
            .context("GitLab merge request did not report diff refs")?;

        let mut position = serde_json::json!({
            "position_type": "text",
            "base_sha": diff_refs.base_sha.clone().unwrap_or_default(),
            "head_sha": diff_refs.head_sha.clone().unwrap_or_default(),
            "start_sha": diff_refs.start_sha.clone().unwrap_or_default(),
            "new_path": path.to_string(),
            "old_path": path.to_string(),
        });
        match side {
            DiffCommentSide::Right => position["new_line"] = serde_json::json!(line),
            DiffCommentSide::Left => position["old_line"] = serde_json::json!(line),
        }
        let payload = serde_json::json!({
            "body": body.to_string(),
            "position": position,
        });
        let url = format!("{api}/projects/{project}/merge_requests/{number}/discussions");
        let request = gitlab_request(
            GitlabMethod::Post,
            &url,
            &auth,
            Some(serde_json::to_vec(&payload)?),
        )?;
        gitlab_send(
            &http_client,
            request,
            &self.api_host(),
            "creating GitLab review comment",
        )
        .await?;
        Ok(())
    }

    async fn pull_request_review_state(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<Option<PullRequestReviewVerdict>> {
        Ok(self
            .fetch_reviewers(remote, number, &auth, &http_client)
            .await?
            .into_iter()
            .find(|reviewer| reviewer.is_me)
            .and_then(|reviewer| reviewer.verdict))
    }

    async fn pull_request_reviewers(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<Vec<PullRequestReviewer>> {
        self.fetch_reviewers(remote, number, &auth, &http_client)
            .await
    }

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
        format!("L{line}")
    }

    fn format_line_numbers(&self, start_line: u32, end_line: u32) -> String {
        format!("L{start_line}-{end_line}")
    }

    fn parse_remote_url(&self, url: &str) -> Option<ParsedGitRemote> {
        let url = RemoteUrl::from_str(url).ok()?;

        let host = url.host_str()?;
        if host != self.base_url.host_str()? {
            return None;
        }

        let mut path_segments = url.path_segments()?.collect::<Vec<_>>();
        let repo = path_segments.pop()?.trim_end_matches(".git");
        let owner = path_segments.join("/");

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
            .join(&format!("{owner}/{repo}/-/commit/{sha}"))
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
            .join(&format!("{owner}/{repo}/-/blob/{sha}/{path}"))
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
        let mut url = self
            .base_url()
            .join(&format!(
                "{}/{}/-/merge_requests/new",
                remote.owner, remote.repo
            ))
            .ok()?;

        let query = format!("merge_request%5Bsource_branch%5D={}", encode(source_branch));

        url.set_query(Some(&query));
        Some(url)
    }

    fn extract_pull_request(&self, remote: &ParsedGitRemote, message: &str) -> Option<PullRequest> {
        // Check commit message for GitLab MR references
        let capture = merge_request_number_regex().captures(message)?;
        // The regex has two capture groups - one for "(!123)" pattern, one for "See merge request" pattern
        let number = capture
            .get(1)
            .or_else(|| capture.get(2))?
            .as_str()
            .parse::<u32>()
            .ok()?;

        let mut url = self.base_url();
        let path = format!(
            "{}/{}/-/merge_requests/{}",
            remote.owner, remote.repo, number
        );
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
            .fetch_gitlab_commit_author(repo_owner, repo, &commit, &http_client)
            .await?
            .map(|author| -> Result<Url, url::ParseError> {
                let mut url = Url::parse(&author.avatar_url)?;
                if let Some(host) = url.host_str() {
                    let size_query = if host.contains("gravatar") || host.contains("libravatar") {
                        Some("s=128")
                    } else if self
                        .base_url
                        .host_str()
                        .is_some_and(|base_host| host.contains(base_host))
                    {
                        Some("width=128")
                    } else {
                        None
                    };
                    url.set_query(size_query);
                }
                Ok(url)
            })
            .transpose()?;
        Ok(avatar_url)
    }
}

#[cfg(test)]
mod tests {
    use git::repository::repo_path;
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_invalid_self_hosted_remote_url() {
        let remote_url = "https://gitlab.com/zed-industries/zed.git";
        let gitlab = Gitlab::from_remote_url(remote_url);
        assert!(gitlab.is_err());
    }

    #[test]
    fn test_parse_remote_url_given_ssh_url() {
        let parsed_remote = Gitlab::public_instance()
            .parse_remote_url("git@gitlab.com:zed-industries/zed.git")
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
        let parsed_remote = Gitlab::public_instance()
            .parse_remote_url("https://gitlab.com/zed-industries/zed.git")
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
        let remote_url = "git@gitlab.my-enterprise.com:zed-industries/zed.git";

        let parsed_remote = Gitlab::from_remote_url(remote_url)
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
        let remote_url = "https://gitlab.my-enterprise.com/group/subgroup/zed.git";
        let parsed_remote = Gitlab::from_remote_url(remote_url)
            .unwrap()
            .parse_remote_url(remote_url)
            .unwrap();

        assert_eq!(
            parsed_remote,
            ParsedGitRemote {
                owner: "group/subgroup".into(),
                repo: "zed".into(),
            }
        );
    }

    #[test]
    fn test_build_gitlab_permalink() {
        let permalink = Gitlab::public_instance().build_permalink(
            ParsedGitRemote {
                owner: "zed-industries".into(),
                repo: "zed".into(),
            },
            BuildPermalinkParams::new(
                "e6ebe7974deb6bb6cc0e2595c8ec31f0c71084b7",
                &repo_path("crates/editor/src/git/permalink.rs"),
                None,
            ),
        );

        let expected_url = "https://gitlab.com/zed-industries/zed/-/blob/e6ebe7974deb6bb6cc0e2595c8ec31f0c71084b7/crates/editor/src/git/permalink.rs";
        assert_eq!(permalink.to_string(), expected_url.to_string())
    }

    #[test]
    fn test_build_gitlab_permalink_with_single_line_selection() {
        let permalink = Gitlab::public_instance().build_permalink(
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

        let expected_url = "https://gitlab.com/zed-industries/zed/-/blob/e6ebe7974deb6bb6cc0e2595c8ec31f0c71084b7/crates/editor/src/git/permalink.rs#L7";
        assert_eq!(permalink.to_string(), expected_url.to_string())
    }

    #[test]
    fn test_build_gitlab_permalink_with_multi_line_selection() {
        let permalink = Gitlab::public_instance().build_permalink(
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

        let expected_url = "https://gitlab.com/zed-industries/zed/-/blob/e6ebe7974deb6bb6cc0e2595c8ec31f0c71084b7/crates/editor/src/git/permalink.rs#L24-48";
        assert_eq!(permalink.to_string(), expected_url.to_string())
    }

    #[test]
    fn test_build_gitlab_create_pr_url() {
        let remote = ParsedGitRemote {
            owner: "zed-industries".into(),
            repo: "zed".into(),
        };

        let provider = Gitlab::public_instance();

        let url = provider
            .build_create_pull_request_url(&remote, "feature/cool stuff")
            .expect("create PR url should be constructed");

        assert_eq!(
            url.as_str(),
            "https://gitlab.com/zed-industries/zed/-/merge_requests/new?merge_request%5Bsource_branch%5D=feature%2Fcool%20stuff"
        );
    }

    #[test]
    fn test_build_gitlab_self_hosted_permalink_from_ssh_url() {
        let gitlab =
            Gitlab::from_remote_url("git@gitlab.some-enterprise.com:zed-industries/zed.git")
                .unwrap();
        let permalink = gitlab.build_permalink(
            ParsedGitRemote {
                owner: "zed-industries".into(),
                repo: "zed".into(),
            },
            BuildPermalinkParams::new(
                "e6ebe7974deb6bb6cc0e2595c8ec31f0c71084b7",
                &repo_path("crates/editor/src/git/permalink.rs"),
                None,
            ),
        );

        let expected_url = "https://gitlab.some-enterprise.com/zed-industries/zed/-/blob/e6ebe7974deb6bb6cc0e2595c8ec31f0c71084b7/crates/editor/src/git/permalink.rs";
        assert_eq!(permalink.to_string(), expected_url.to_string())
    }

    #[test]
    fn test_build_gitlab_self_hosted_permalink_from_https_url() {
        let gitlab =
            Gitlab::from_remote_url("https://gitlab-instance.big-co.com/zed-industries/zed.git")
                .unwrap();
        let permalink = gitlab.build_permalink(
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

        let expected_url = "https://gitlab-instance.big-co.com/zed-industries/zed/-/blob/b2efec9824c45fcc90c9a7eb107a50d1772a60aa/crates/zed/src/main.rs";
        assert_eq!(permalink.to_string(), expected_url.to_string())
    }

    #[test]
    fn test_build_create_pull_request_url() {
        let remote = ParsedGitRemote {
            owner: "zed-industries".into(),
            repo: "zed".into(),
        };

        let github = Gitlab::public_instance();
        let url = github
            .build_create_pull_request_url(&remote, "feature/new-feature")
            .unwrap();

        assert_eq!(
            url.as_str(),
            "https://gitlab.com/zed-industries/zed/-/merge_requests/new?merge_request%5Bsource_branch%5D=feature%2Fnew-feature"
        );

        let base_url = Url::parse("https://gitlab.zed.com").unwrap();
        let github = Gitlab::new("GitLab Self-Hosted", base_url);
        let url = github
            .build_create_pull_request_url(&remote, "feature/new-feature")
            .expect("should be able to build pull request url");

        assert_eq!(
            url.as_str(),
            "https://gitlab.zed.com/zed-industries/zed/-/merge_requests/new?merge_request%5Bsource_branch%5D=feature%2Fnew-feature"
        );
    }

    #[test]
    fn test_extract_merge_request_from_squash_commit() {
        let remote = ParsedGitRemote {
            owner: "zed-industries".into(),
            repo: "zed".into(),
        };

        let provider = Gitlab::public_instance();

        // Test squash merge pattern: "commit message (!123)"
        let message = "Add new feature (!456)";
        let pull_request = provider.extract_pull_request(&remote, message).unwrap();

        assert_eq!(pull_request.number, 456);
        assert_eq!(
            pull_request.url.as_str(),
            "https://gitlab.com/zed-industries/zed/-/merge_requests/456"
        );
    }

    #[test]
    fn test_extract_merge_request_from_merge_commit() {
        let remote = ParsedGitRemote {
            owner: "zed-industries".into(),
            repo: "zed".into(),
        };

        let provider = Gitlab::public_instance();

        // Test standard merge commit pattern: "See merge request group/project!123"
        let message =
            "Merge branch 'feature' into 'main'\n\nSee merge request zed-industries/zed!789";
        let pull_request = provider.extract_pull_request(&remote, message).unwrap();

        assert_eq!(pull_request.number, 789);
        assert_eq!(
            pull_request.url.as_str(),
            "https://gitlab.com/zed-industries/zed/-/merge_requests/789"
        );
    }

    #[test]
    fn test_extract_merge_request_self_hosted() {
        let base_url = Url::parse("https://gitlab.my-company.com").unwrap();
        let provider = Gitlab::new("GitLab Self-Hosted", base_url);

        let remote = ParsedGitRemote {
            owner: "team".into(),
            repo: "project".into(),
        };

        let message = "Fix bug (!42)";
        let pull_request = provider.extract_pull_request(&remote, message).unwrap();

        assert_eq!(pull_request.number, 42);
        assert_eq!(
            pull_request.url.as_str(),
            "https://gitlab.my-company.com/team/project/-/merge_requests/42"
        );
    }

    #[test]
    fn test_extract_merge_request_no_match() {
        let remote = ParsedGitRemote {
            owner: "zed-industries".into(),
            repo: "zed".into(),
        };

        let provider = Gitlab::public_instance();

        // No MR reference in message
        let message = "Just a regular commit message";
        let pull_request = provider.extract_pull_request(&remote, message);

        assert!(pull_request.is_none());
    }
}
