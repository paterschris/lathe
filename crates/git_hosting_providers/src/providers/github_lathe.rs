use super::*;

impl Github {
    /// Fetch the login of the user the supplied credential authenticates as, so
    /// the PR list can be filtered down to review requests addressed to them.
    pub(super) async fn fetch_authenticated_login(
        &self,
        auth: &Option<GitHostAuth>,
        http_client: &Arc<dyn HttpClient>,
    ) -> Result<Option<SharedString>> {
        let host = self
            .base_url
            .host_str()
            .context("GitHub base URL has no host")?;
        let url = format!("https://api.{host}/user");
        let request = github_request(
            GithubMethod::Get,
            &url,
            "application/vnd.github+json",
            auth,
            None,
        )?;
        let bytes = github_send(http_client, request, "fetching authenticated GitHub user").await?;
        let user: AuthenticatedGithubUser =
            serde_json::from_slice(&bytes).context("parsing authenticated GitHub user")?;
        Ok(Some(user.login.into()))
    }

    /// Best-effort lookup of the authenticated user's current review verdict on a
    /// PR, so the detail view can reflect what they've already done. Returns
    /// `Ok(None)` when they have no approving or blocking review; the caller logs
    /// and ignores any error, leaving the buttons in their default state.
    /// Fetch all submitted reviews on a PR. Used to derive both the viewer's own
    /// verdict (list tint) and the full reviewer list (detail panel). Paginated to
    /// 100, which comfortably covers a PR's reviewers.
    pub(super) async fn fetch_reviews(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        auth: &Option<GitHostAuth>,
        http_client: &Arc<dyn HttpClient>,
    ) -> Result<Vec<GithubReview>> {
        let host = self
            .base_url
            .host_str()
            .context("GitHub base URL has no host")?;
        let url = format!(
            "https://api.{host}/repos/{owner}/{repo}/pulls/{number}/reviews?per_page=100",
            owner = remote.owner,
            repo = remote.repo,
        );
        let request = github_request(
            GithubMethod::Get,
            &url,
            "application/vnd.github+json",
            auth,
            None,
        )?;
        let bytes = github_send(http_client, request, "fetching GitHub reviews").await?;
        serde_json::from_slice(&bytes).context("parsing GitHub reviews")
    }

    pub(super) async fn remove_review_with_verdict(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        verdict: PullRequestReviewVerdict,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<()> {
        let host = self
            .base_url
            .host_str()
            .context("GitHub base URL has no host")?;
        let login = self
            .fetch_authenticated_login(&auth, &http_client)
            .await?
            .context("could not determine the authenticated GitHub user")?;
        let reviews = self
            .fetch_reviews(remote, number, &auth, &http_client)
            .await?;
        let review_id = viewer_review_id_to_dismiss(&reviews, login.as_ref(), verdict)
            .context("no matching review to remove")?;
        // GitHub has no "un-review" endpoint; retracting a verdict is done by
        // dismissing the review, which requires a message.
        let url = format!(
            "https://api.{host}/repos/{owner}/{repo}/pulls/{number}/reviews/{review_id}/dismissals",
            owner = remote.owner,
            repo = remote.repo,
        );
        let body = serde_json::to_vec(
            &serde_json::json!({ "message": "Dismissed via Lathe", "event": "DISMISS" }),
        )?;
        let request = github_request(
            GithubMethod::Put,
            &url,
            "application/vnd.github+json",
            &auth,
            Some(body),
        )?;
        github_send(&http_client, request, "dismissing GitHub review").await?;
        Ok(())
    }
}

/// The viewer's current verdict from their chronological review history: the most
/// recent approving or blocking review wins; comment-only and pending entries
/// don't change a prior verdict; a later DISMISSED clears it.
pub(super) fn viewer_verdict_from_reviews(
    reviews: &[GithubReview],
    login: &str,
) -> Option<PullRequestReviewVerdict> {
    let mut verdict = None;
    for review in reviews {
        let is_viewer = review
            .user
            .as_ref()
            .is_some_and(|user| user.login.eq_ignore_ascii_case(login));
        if !is_viewer {
            continue;
        }
        match review.state.as_str() {
            "APPROVED" => verdict = Some(PullRequestReviewVerdict::Approve),
            "CHANGES_REQUESTED" => verdict = Some(PullRequestReviewVerdict::RequestChanges),
            "DISMISSED" => verdict = None,
            _ => {}
        }
    }
    verdict
}

/// The id of the viewer's own review that currently produces `verdict`, so it can
/// be dismissed to retract that verdict. Mirrors `viewer_verdict_from_reviews`:
/// the latest approving/blocking review wins and a later DISMISSED clears it, so
/// this returns an id only when the viewer's effective verdict still matches.
pub(super) fn viewer_review_id_to_dismiss(
    reviews: &[GithubReview],
    login: &str,
    verdict: PullRequestReviewVerdict,
) -> Option<u64> {
    let mut current: Option<(u64, PullRequestReviewVerdict)> = None;
    for review in reviews {
        let is_viewer = review
            .user
            .as_ref()
            .is_some_and(|user| user.login.eq_ignore_ascii_case(login));
        if !is_viewer {
            continue;
        }
        match review.state.as_str() {
            "APPROVED" => current = Some((review.id, PullRequestReviewVerdict::Approve)),
            "CHANGES_REQUESTED" => {
                current = Some((review.id, PullRequestReviewVerdict::RequestChanges))
            }
            "DISMISSED" => current = None,
            _ => {}
        }
    }
    current
        .filter(|(_, current_verdict)| *current_verdict == verdict)
        .map(|(id, _)| id)
}

/// The full reviewer list: everyone who submitted a review (with their latest
/// verdict, in first-seen order) plus still-requested reviewers as pending
/// (`verdict: None`). `viewer_login` marks the authenticated user's own entry.
pub(super) fn build_github_reviewers(
    reviews: &[GithubReview],
    requested: &[SharedString],
    viewer_login: Option<&str>,
) -> Vec<PullRequestReviewer> {
    let mut ordered: Vec<(String, Option<PullRequestReviewVerdict>)> = Vec::new();
    for review in reviews {
        let Some(user) = review.user.as_ref() else {
            continue;
        };
        if !ordered
            .iter()
            .any(|(login, _)| login.eq_ignore_ascii_case(&user.login))
        {
            ordered.push((user.login.clone(), None));
        }
        if let Some(entry) = ordered
            .iter_mut()
            .find(|(login, _)| login.eq_ignore_ascii_case(&user.login))
        {
            match review.state.as_str() {
                "APPROVED" => entry.1 = Some(PullRequestReviewVerdict::Approve),
                "CHANGES_REQUESTED" => entry.1 = Some(PullRequestReviewVerdict::RequestChanges),
                "DISMISSED" => entry.1 = None,
                _ => {}
            }
        }
    }
    for login in requested {
        if !ordered
            .iter()
            .any(|(existing, _)| existing.eq_ignore_ascii_case(login))
        {
            ordered.push((login.to_string(), None));
        }
    }
    ordered
        .into_iter()
        .map(|(login, verdict)| PullRequestReviewer {
            is_me: viewer_login.is_some_and(|viewer| viewer.eq_ignore_ascii_case(&login)),
            login: login.into(),
            verdict,
        })
        .collect()
}

pub(super) enum GithubMethod {
    Get,
    Post,
    Put,
}

/// Builds the `Authorization` header value for a GitHub API request. Falls back
/// to the `GITHUB_TOKEN` environment variable when no stored credential is
/// supplied, preserving the previous headless/CI behavior.
pub(super) fn github_auth_header(auth: &Option<GitHostAuth>) -> Option<String> {
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

pub(super) fn github_request(
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

pub(super) async fn github_send(
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
    if response.status().as_u16() == 401 {
        return Err(PullRequestAuthError {
            host: "github.com".into(),
        }
        .into());
    }
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
pub(super) fn github_list_state(filter: &PullRequestListFilter) -> &'static str {
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
pub(super) struct GithubUserRef {
    pub(super) login: String,
}

#[derive(Deserialize)]
struct AuthenticatedGithubUser {
    login: String,
}

#[derive(Deserialize)]
struct GithubBranchRef {
    #[serde(rename = "ref")]
    ref_name: String,
    sha: String,
}

#[derive(Deserialize)]
pub(super) struct GithubPullRequest {
    pub(super) number: u32,
    title: String,
    #[serde(default)]
    body: Option<String>,
    state: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    pub(super) user: Option<GithubUserRef>,
    /// Users whose review is still pending. GitHub removes a user from this
    /// list once they submit a review, so it represents outstanding requests.
    #[serde(default)]
    pub(super) requested_reviewers: Vec<GithubUserRef>,
    head: GithubBranchRef,
    base: GithubBranchRef,
    html_url: String,
    updated_at: String,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    commits: Option<u32>,
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
    pub(super) fn pull_request_state(&self) -> PullRequestState {
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

    pub(super) fn into_summary(self, state: PullRequestState) -> Result<PullRequestSummary> {
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

    pub(super) fn into_detail(self) -> Result<PullRequestDetail> {
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
            created_at: self.created_at.into(),
            updated_at: self.updated_at.into(),
            is_draft: self.draft,
            is_mergeable: self.mergeable,
            additions: self.additions.unwrap_or(0),
            deletions: self.deletions.unwrap_or(0),
            changed_files: self.changed_files.unwrap_or(0),
            commits: self.commits,
            // Filled in by `get_pull_request` from a separate compare request.
            behind_by: None,
            // Filled in by `get_pull_request` from a separate reviews request.
            viewer_review: None,
            reviewers: Vec::new(),
        })
    }
}

/// Subset of GitHub's compare response. `behind_by` is the number of commits on
/// the base branch that are not reachable from the head branch, i.e. how far
/// behind the base the pull request has fallen.
#[derive(Deserialize)]
pub(super) struct GithubComparison {
    #[serde(default)]
    pub(super) behind_by: u32,
}

/// A submitted review on a pull request. The author and state resolve the
/// authenticated user's current verdict; the id lets us dismiss (retract) it.
#[derive(Deserialize)]
pub(super) struct GithubReview {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    user: Option<GithubUserRef>,
    #[serde(default)]
    state: String,
}

#[derive(Deserialize)]
pub(super) struct GithubReviewComment {
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
    pub(super) fn into_comment(self) -> Result<PullRequestReviewComment> {
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
    use pretty_assertions::assert_eq;

    use super::*;
    use http_client::FakeHttpClient;

    #[test]
    fn test_github_send_maps_401_to_auth_error() {
        let client: Arc<dyn HttpClient> = FakeHttpClient::create(|_request| async move {
            Ok(http_client::Response::builder()
                .status(401)
                .body("Bad credentials".into())
                .unwrap())
        });
        let request = Request::get("https://api.github.com/user")
            .body(AsyncBody::empty())
            .unwrap();

        let error = futures::executor::block_on(github_send(&client, request, "load user"))
            .expect_err("a 401 response should be an error");
        let auth_error = error
            .downcast_ref::<PullRequestAuthError>()
            .expect("a 401 should map to PullRequestAuthError");
        assert_eq!(auth_error.host.as_ref(), "github.com");
    }

    #[test]
    fn test_github_send_non_401_failure_is_generic() {
        let client: Arc<dyn HttpClient> = FakeHttpClient::create(|_request| async move {
            Ok(http_client::Response::builder()
                .status(500)
                .body("boom".into())
                .unwrap())
        });
        let request = Request::get("https://api.github.com/user")
            .body(AsyncBody::empty())
            .unwrap();

        let error = futures::executor::block_on(github_send(&client, request, "load user"))
            .expect_err("a 500 response should be an error");
        assert!(
            error.downcast_ref::<PullRequestAuthError>().is_none(),
            "non-401 failures should not be treated as an auth error"
        );
    }

    #[test]
    fn test_pull_request_auth_error_survives_context() {
        // The UI downcasts to PullRequestAuthError after intermediate layers
        // wrap the error with `.context()`; anyhow must preserve the concrete
        // type through that wrapping for the reconnect prompt to appear.
        let error: anyhow::Error = PullRequestAuthError {
            host: "github.com".into(),
        }
        .into();
        let wrapped = error
            .context("listing pull requests")
            .context("refreshing pull request panel");
        let auth_error = wrapped
            .downcast_ref::<PullRequestAuthError>()
            .expect("auth error should survive context wrapping");
        assert_eq!(auth_error.host.as_ref(), "github.com");
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
        assert_eq!(
            summary.url.as_str(),
            "https://github.com/owner/repo/pull/42"
        );
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
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-03T00:00:00Z",
            "merged_at": "2026-01-03T00:00:00Z",
            "mergeable": true,
            "additions": 12,
            "deletions": 3,
            "changed_files": 2,
            "commits": 4
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
        assert_eq!(detail.created_at.to_string(), "2026-01-01T00:00:00Z");
        assert_eq!(detail.commits, Some(4));
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

    #[test]
    fn test_github_list_filters_to_requested_reviewer() {
        // With `reviewer_is_me`, the provider first resolves the caller's login
        // via `/user`, then keeps only PRs whose `requested_reviewers` include it.
        let client: Arc<dyn HttpClient> = FakeHttpClient::create(|request| async move {
            let path = request.uri().path();
            let body = if path == "/user" {
                r#"{"login":"octocat"}"#
            } else if path.ends_with("/pulls") {
                r#"[
                    {"number":1,"title":"Mine to review","state":"open",
                     "user":{"login":"alice"},
                     "requested_reviewers":[{"login":"octocat"}],
                     "head":{"ref":"a","sha":"a1"},"base":{"ref":"main","sha":"b1"},
                     "html_url":"https://github.com/owner/repo/pull/1","updated_at":"2026-01-02T00:00:00Z"},
                    {"number":2,"title":"Not mine","state":"open",
                     "user":{"login":"bob"},
                     "requested_reviewers":[{"login":"carol"}],
                     "head":{"ref":"c","sha":"c1"},"base":{"ref":"main","sha":"b2"},
                     "html_url":"https://github.com/owner/repo/pull/2","updated_at":"2026-01-01T00:00:00Z"}
                ]"#
            } else {
                "[]"
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
        let summaries = futures::executor::block_on(Github::public_instance().list_pull_requests(
            &remote,
            filter,
            Some(GitHostAuth::Bearer("token".into())),
            client,
        ))
        .unwrap();

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].number, 1);
        assert_eq!(summaries[0].title.to_string(), "Mine to review");
    }

    #[test]
    fn test_github_list_filters_to_authenticated_author() {
        let client: Arc<dyn HttpClient> = FakeHttpClient::create(|request| async move {
            let path = request.uri().path();
            let body = if path == "/user" {
                r#"{"login":"OctoCat"}"#
            } else if path.ends_with("/pulls") {
                r#"[
                    {"number":1,"title":"Mine","state":"open",
                     "user":{"login":"octocat"},
                     "requested_reviewers":[],
                     "head":{"ref":"a","sha":"a1"},"base":{"ref":"main","sha":"b1"},
                     "html_url":"https://github.com/owner/repo/pull/1","updated_at":"2026-01-02T00:00:00Z"},
                    {"number":2,"title":"Theirs","state":"open",
                     "user":{"login":"alice"},
                     "requested_reviewers":[],
                     "head":{"ref":"c","sha":"c1"},"base":{"ref":"main","sha":"b2"},
                     "html_url":"https://github.com/owner/repo/pull/2","updated_at":"2026-01-01T00:00:00Z"}
                ]"#
            } else {
                "[]"
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
        let summaries = futures::executor::block_on(Github::public_instance().list_pull_requests(
            &remote,
            filter,
            Some(GitHostAuth::Bearer("token".into())),
            client,
        ))
        .unwrap();

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].number, 1);
        assert_eq!(summaries[0].author_login.to_string(), "octocat");
    }

    #[test]
    fn test_github_author_filter_errors_when_login_cannot_be_resolved() {
        let client: Arc<dyn HttpClient> = FakeHttpClient::create(|request| async move {
            let path = request.uri().path();
            let body = if path == "/user" { r#"{}"# } else { "[]" };
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
        let error = futures::executor::block_on(Github::public_instance().list_pull_requests(
            &remote,
            filter,
            Some(GitHostAuth::Bearer("token".into())),
            client,
        ))
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("parsing authenticated GitHub user")
        );
    }

    #[test]
    fn test_github_pull_request_reviewers_marks_viewer_and_pending() {
        let client: Arc<dyn HttpClient> = FakeHttpClient::create(|request| async move {
            let path = request.uri().path();
            let body = if path == "/user" {
                r#"{"login":"octocat"}"#
            } else if path.ends_with("/pulls/7") {
                r#"{"number":7,"title":"Reviewers","state":"open",
                    "user":{"login":"alice"},
                    "requested_reviewers":[{"login":"pending"}],
                    "head":{"ref":"a","sha":"a1"},"base":{"ref":"main","sha":"b1"},
                    "html_url":"https://github.com/owner/repo/pull/7","updated_at":"2026-01-02T00:00:00Z"}"#
            } else if path.ends_with("/pulls/7/reviews") {
                r#"[
                    {"id":1,"user":{"login":"octocat"},"state":"APPROVED"},
                    {"id":2,"user":{"login":"carol"},"state":"CHANGES_REQUESTED"}
                ]"#
            } else {
                "[]"
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
            futures::executor::block_on(Github::public_instance().pull_request_reviewers(
                &remote,
                7,
                Some(GitHostAuth::Bearer("token".into())),
                client,
            ))
            .unwrap();

        assert_eq!(reviewers.len(), 3);
        assert_eq!(reviewers[0].login.to_string(), "octocat");
        assert_eq!(
            reviewers[0].verdict,
            Some(PullRequestReviewVerdict::Approve)
        );
        assert!(reviewers[0].is_me);
        assert_eq!(
            reviewers[1].verdict,
            Some(PullRequestReviewVerdict::RequestChanges)
        );
        assert_eq!(reviewers[2].login.to_string(), "pending");
        assert_eq!(reviewers[2].verdict, None);
        assert!(!reviewers[2].is_me);
    }

    #[test]
    fn test_github_pull_request_reviewers_without_auth_does_not_mark_me() {
        let client: Arc<dyn HttpClient> = FakeHttpClient::create(|request| async move {
            let path = request.uri().path();
            let (status, body) = if path == "/user" {
                (401, r#"{"message":"bad credentials"}"#)
            } else if path.ends_with("/pulls/7") {
                (
                    200,
                    r#"{"number":7,"title":"Reviewers","state":"open",
                    "user":{"login":"alice"},
                    "requested_reviewers":[],
                    "head":{"ref":"a","sha":"a1"},"base":{"ref":"main","sha":"b1"},
                    "html_url":"https://github.com/owner/repo/pull/7","updated_at":"2026-01-02T00:00:00Z"}"#,
                )
            } else if path.ends_with("/pulls/7/reviews") {
                (
                    200,
                    r#"[{"id":1,"user":{"login":"octocat"},"state":"APPROVED"}]"#,
                )
            } else {
                (200, "[]")
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
            Github::public_instance().pull_request_reviewers(&remote, 7, None, client),
        )
        .unwrap();

        assert_eq!(reviewers.len(), 1);
        assert_eq!(reviewers[0].login.to_string(), "octocat");
        assert!(!reviewers[0].is_me);
    }
}
