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
            // Filled in by `get_pull_request` from a separate reviews request.
            viewer_review: None,
            reviewers: Vec::new(),
        })
    }
}

/// A submitted review on a pull request. Only the author and state are needed
/// to resolve the authenticated user's current verdict.
#[derive(Deserialize)]
pub(super) struct GithubReview {
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
