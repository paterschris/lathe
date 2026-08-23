use super::*;

/// Matches a Bitbucket `@`-mention token in a comment body, e.g.
/// `@{62f4fec150bd9783f62da8af}`; capture group 1 is the account id.
fn bitbucket_mention_regex() -> &'static Regex {
    static MENTION_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"@\{([^}]+)\}").unwrap());
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
pub(super) fn resolve_bitbucket_mentions(
    body: &str,
    names: &HashMap<String, SharedString>,
) -> String {
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

impl Bitbucket {
    /// Fetch the UUID of the user the supplied credential authenticates as, so
    /// the PR list can be filtered to reviews assigned to them. Requires the
    /// credential to carry the `account` scope.
    /// The display name of the account a credential authenticates as, for
    /// labelling the connected account in menus. Separate from
    /// [`Self::fetch_authenticated_uuid`], which resolves the opaque uuid the
    /// PR-filtering endpoints match on.
    /// CI results for a commit, from Bitbucket's build-status API. Pipelines and
    /// third-party CI both publish here, so one call covers both.
    pub(super) async fn fetch_checks(
        &self,
        remote: &ParsedGitRemote,
        sha: &str,
        auth: &Option<GitHostAuth>,
        http_client: &Arc<dyn HttpClient>,
    ) -> Result<Option<PullRequestChecks>> {
        let api_base = bitbucket_cloud_api_base(&self.base_url, remote)?;
        let url = format!("{api_base}/commit/{sha}/statuses?pagelen=100");
        let statuses: Vec<BitbucketBuildStatus> = bitbucket_get_paginated(
            http_client,
            url,
            auth,
            "fetching Bitbucket build statuses",
            200,
        )
        .await?;
        let mut checks = PullRequestChecks {
            succeeded: 0,
            failed: 0,
            pending: 0,
            neutral: 0,
        };
        for status in statuses {
            match status.state.as_deref() {
                Some("SUCCESSFUL") => checks.succeeded += 1,
                Some("FAILED") => checks.failed += 1,
                Some("INPROGRESS") => checks.pending += 1,
                // `STOPPED` is a cancelled build: not a pass, not a failure.
                _ => checks.neutral += 1,
            }
        }
        Ok((!checks.is_empty()).then_some(checks))
    }

    pub(super) async fn fetch_authenticated_login(
        &self,
        auth: &Option<GitHostAuth>,
        http_client: &Arc<dyn HttpClient>,
    ) -> Result<Option<SharedString>> {
        let host = self
            .base_url
            .host_str()
            .context("Bitbucket base URL has no host")?;
        let url = format!("https://api.{host}/2.0/user");
        let request = bitbucket_request(BitbucketMethod::Get, &url, auth, None)?;
        let bytes = bitbucket_send(
            http_client,
            request,
            "fetching authenticated Bitbucket user",
        )
        .await?;
        let user: AuthenticatedBitbucketUser =
            serde_json::from_slice(&bytes).context("parsing authenticated Bitbucket user")?;
        Ok(user
            .nickname
            .or(user.display_name)
            .map(SharedString::from))
    }

    pub(super) async fn fetch_authenticated_uuid(
        &self,
        auth: &Option<GitHostAuth>,
        http_client: &Arc<dyn HttpClient>,
    ) -> Result<Option<SharedString>> {
        let host = self
            .base_url
            .host_str()
            .context("Bitbucket base URL has no host")?;
        let url = format!("https://api.{host}/2.0/user");
        let request = bitbucket_request(BitbucketMethod::Get, &url, auth, None)?;
        let bytes = bitbucket_send(
            http_client,
            request,
            "fetching authenticated Bitbucket user",
        )
        .await?;
        let user: AuthenticatedBitbucketUser =
            serde_json::from_slice(&bytes).context("parsing authenticated Bitbucket user")?;
        Ok(user.uuid.map(SharedString::from))
    }

    pub(super) async fn remove_review_with_verdict(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        verdict: PullRequestReviewVerdict,
        auth: Option<GitHostAuth>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<()> {
        if self.is_self_hosted() {
            bail!("pull request operations are only supported on Bitbucket Cloud");
        }
        let api_base = bitbucket_cloud_api_base(&self.base_url, remote)?;
        // Bitbucket Cloud redacts a verdict with a DELETE on the same endpoint
        // that adds it, mirroring `submit_review`'s POST.
        let url = match verdict {
            PullRequestReviewVerdict::Approve => {
                format!("{api_base}/pullrequests/{number}/approve")
            }
            PullRequestReviewVerdict::RequestChanges => {
                format!("{api_base}/pullrequests/{number}/request-changes")
            }
            PullRequestReviewVerdict::Comment => {
                bail!("Bitbucket has no comment-only review to remove");
            }
        };
        let request = bitbucket_request(BitbucketMethod::Delete, &url, &auth, None)?;
        bitbucket_send(&http_client, request, "removing Bitbucket review").await?;
        Ok(())
    }
}

pub(super) enum BitbucketMethod {
    Get,
    Post,
    Delete,
}

pub(super) fn bitbucket_cloud_api_base(base_url: &Url, remote: &ParsedGitRemote) -> Result<String> {
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
pub(super) fn bitbucket_auth_header(auth: &Option<GitHostAuth>) -> Option<String> {
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

pub(super) fn bitbucket_request(
    method: BitbucketMethod,
    url: &str,
    auth: &Option<GitHostAuth>,
    json_body: Option<Vec<u8>>,
) -> Result<Request<AsyncBody>> {
    let builder = match method {
        BitbucketMethod::Get => Request::get(url),
        BitbucketMethod::Post => Request::post(url),
        BitbucketMethod::Delete => Request::delete(url),
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

pub(super) async fn bitbucket_send(
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
            host: "bitbucket.org".into(),
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

/// Fetches a Bitbucket Cloud paginated collection, following `next` links until
/// `max_items` are collected or the pages run out.
pub(super) async fn bitbucket_get_paginated<T: DeserializeOwned>(
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
pub(super) fn bitbucket_states(filter: &PullRequestListFilter) -> Vec<&'static str> {
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

/// A pull request commit. Only the number of these is used (for the header's
/// commit count), so no fields are deserialized.
#[derive(Deserialize)]
pub(super) struct BitbucketCommitId {}

#[derive(Deserialize)]
struct AuthenticatedBitbucketUser {
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    nickname: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct BitbucketAccount {
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
    pub(super) fn login(&self) -> SharedString {
        self.nickname
            .clone()
            .or_else(|| self.display_name.clone())
            .map(SharedString::from)
            .unwrap_or_default()
    }

    /// The name Bitbucket shows for an `@`-mention (full display name preferred).
    pub(super) fn mention_name(&self) -> Option<SharedString> {
        self.display_name
            .clone()
            .or_else(|| self.nickname.clone())
            .map(SharedString::from)
    }

    /// Normalized ids this account can be mentioned by, for matching the
    /// `@{...}` token in comment bodies against the thread participants.
    pub(super) fn mention_keys(&self) -> Vec<String> {
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
pub(super) struct BitbucketPullRequest {
    pub(super) id: u32,
    title: String,
    state: String,
    #[serde(default)]
    pub(super) author: Option<BitbucketAccount>,
    #[serde(default)]
    source: Option<BitbucketEndpoint>,
    #[serde(default)]
    destination: Option<BitbucketEndpoint>,
    #[serde(default)]
    links: Option<BitbucketLinks>,
    #[serde(default)]
    updated_on: Option<String>,
    #[serde(default)]
    created_on: Option<String>,
    #[serde(default)]
    summary: Option<BitbucketContent>,
    #[serde(default)]
    draft: bool,
    /// Reviewers and other participants, each carrying their own approval state.
    /// Present on the single-PR detail response; absent from list summaries.
    #[serde(default)]
    pub(super) participants: Vec<BitbucketParticipant>,
}

/// A participant on a Bitbucket pull request. `approved` and `state` together
/// describe a reviewer's verdict: `state` is `"approved"`, `"changes_requested"`,
/// or absent when they have not weighed in.
#[derive(Deserialize)]
pub(super) struct BitbucketParticipant {
    #[serde(default)]
    user: Option<BitbucketAccount>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    approved: bool,
    #[serde(default)]
    state: Option<String>,
}

/// Maps a Bitbucket participant's approval flags to a review verdict: a blocking
/// review is `state == "changes_requested"`; an approval is `approved == true`
/// (Bitbucket also sets `state == "approved"`).
fn participant_verdict(participant: &BitbucketParticipant) -> Option<PullRequestReviewVerdict> {
    if participant.state.as_deref() == Some("changes_requested") {
        Some(PullRequestReviewVerdict::RequestChanges)
    } else if participant.approved || participant.state.as_deref() == Some("approved") {
        Some(PullRequestReviewVerdict::Approve)
    } else {
        None
    }
}

impl BitbucketPullRequest {
    /// The authenticated user's current review verdict, resolved by matching
    /// their account uuid against the PR participants. Bitbucket reports a
    /// blocking review as `state == "changes_requested"` and an approval as
    /// `approved == true` (alongside `state == "approved"`).
    pub(super) fn viewer_review(&self, viewer_uuid: &str) -> Option<PullRequestReviewVerdict> {
        let participant = self.participants.iter().find(|participant| {
            participant
                .user
                .as_ref()
                .and_then(|user| user.uuid.as_deref())
                .is_some_and(|uuid| uuid == viewer_uuid)
        })?;
        participant_verdict(participant)
    }

    /// Whether the authenticated user (matched by account uuid) is a designated
    /// reviewer on this PR, whether or not they have already approved or
    /// requested changes. Reviewers added via a default-reviewer group are
    /// materialized as participants with role REVIEWER, so this catches them even
    /// when the top-level `reviewers` array omits them. Callers distinguish an
    /// outstanding review from a completed one via the per-reviewer verdict.
    pub(super) fn is_reviewer(&self, viewer_uuid: &str) -> bool {
        self.participants
            .iter()
            .find(|participant| {
                participant
                    .user
                    .as_ref()
                    .and_then(|user| user.uuid.as_deref())
                    == Some(viewer_uuid)
            })
            .is_some_and(|participant| {
                participant
                    .role
                    .as_deref()
                    .is_some_and(|role| role.eq_ignore_ascii_case("REVIEWER"))
            })
    }

    /// All reviewers (participants with the REVIEWER role) and their verdicts, in
    /// the order Bitbucket returns them. `viewer_uuid` marks the authenticated
    /// user's own entry.
    pub(super) fn reviewers(&self, viewer_uuid: Option<&str>) -> Vec<PullRequestReviewer> {
        self.participants
            .iter()
            .filter(|participant| {
                participant
                    .role
                    .as_deref()
                    .is_some_and(|role| role.eq_ignore_ascii_case("REVIEWER"))
            })
            .map(|participant| {
                let user = participant.user.as_ref();
                let uuid = user.and_then(|user| user.uuid.as_deref());
                let login = user
                    .and_then(|user| user.display_name.as_deref().or(user.nickname.as_deref()))
                    .unwrap_or("Reviewer");
                PullRequestReviewer {
                    login: SharedString::from(login.to_string()),
                    verdict: participant_verdict(participant),
                    is_me: matches!((uuid, viewer_uuid), (Some(a), Some(b)) if a == b),
                }
            })
            .collect()
    }

    pub(super) fn pull_request_state(&self) -> PullRequestState {
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

    pub(super) fn into_summary(self, state: PullRequestState) -> Result<PullRequestSummary> {
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

    pub(super) fn into_detail(self) -> Result<PullRequestDetail> {
        let state = self.pull_request_state();
        let author_login = self.author.as_ref().map(|a| a.login()).unwrap_or_default();
        let source_branch = Self::branch_name(&self.source);
        let target_branch = Self::branch_name(&self.destination);
        let head_sha = Self::commit_hash(&self.source);
        let base_sha = Self::commit_hash(&self.destination);
        let url = self.html_url()?;
        let updated_at: SharedString = self.updated_on.clone().unwrap_or_default().into();
        let created_at: SharedString = self.created_on.clone().unwrap_or_default().into();
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
            created_at,
            updated_at,
            is_draft: self.draft,
            // Bitbucket does not expose mergeability or line-change stats on the
            // pull request object.
            is_mergeable: None,
            additions: 0,
            deletions: 0,
            changed_files: 0,
            // Filled in by `get_pull_request` via a separate commits request; the
            // PR object itself carries no commit count.
            commits: None,
            // Filled in by `get_pull_request` via a separate compare request.
            behind_by: None,
            // Resolved by `get_pull_request` from the PR's participants once the
            // authenticated account's uuid is known.
            viewer_review: None,
            reviewers: Vec::new(),
            checks: None,
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
pub(super) struct BitbucketComment {
    id: u64,
    #[serde(default)]
    pub(super) user: Option<BitbucketAccount>,
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
    pub(super) deleted: bool,
}

impl BitbucketComment {
    pub(super) fn into_comment(self) -> Result<PullRequestReviewComment> {
        let author_login = self
            .user
            .as_ref()
            .map(|user| user.login())
            .unwrap_or_default();
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
    use pretty_assertions::assert_eq;

    use super::*;
    use http_client::FakeHttpClient;

    #[test]
    fn test_bitbucket_send_maps_401_to_auth_error() {
        let client: Arc<dyn HttpClient> = FakeHttpClient::create(|_request| async move {
            Ok(http_client::Response::builder()
                .status(401)
                .body("Unauthorized".into())
                .unwrap())
        });
        let request = Request::get("https://api.bitbucket.org/2.0/user")
            .body(AsyncBody::empty())
            .unwrap();

        let error = futures::executor::block_on(bitbucket_send(&client, request, "load user"))
            .expect_err("a 401 response should be an error");
        let auth_error = error
            .downcast_ref::<PullRequestAuthError>()
            .expect("a 401 should map to PullRequestAuthError");
        assert_eq!(auth_error.host.as_ref(), "bitbucket.org");
    }

    #[test]
    fn test_bitbucket_send_non_401_failure_is_generic() {
        let client: Arc<dyn HttpClient> = FakeHttpClient::create(|_request| async move {
            Ok(http_client::Response::builder()
                .status(500)
                .body("boom".into())
                .unwrap())
        });
        let request = Request::get("https://api.bitbucket.org/2.0/user")
            .body(AsyncBody::empty())
            .unwrap();

        let error = futures::executor::block_on(bitbucket_send(&client, request, "load user"))
            .expect_err("a 500 response should be an error");
        assert!(
            error.downcast_ref::<PullRequestAuthError>().is_none(),
            "non-401 failures should not be treated as an auth error"
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
            "created_on": "2026-01-01T00:00:00Z",
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
        assert_eq!(detail.created_at.to_string(), "2026-01-01T00:00:00Z");
        // Bitbucket Cloud does not report these on the PR object.
        assert_eq!(detail.is_mergeable, None);
        assert_eq!(detail.additions, 0);
        assert_eq!(detail.deletions, 0);
        assert_eq!(detail.changed_files, 0);
        // The commit count comes from a separate request in `get_pull_request`.
        assert_eq!(detail.commits, None);
    }

    #[test]
    fn test_bitbucket_viewer_review_from_participants() {
        let json = r#"{
            "id": 9,
            "title": "Tinted buttons",
            "state": "OPEN",
            "links": { "html": { "href": "https://bitbucket.org/owner/repo/pull-requests/9" } },
            "participants": [
                { "user": { "uuid": "{approver}" }, "approved": true, "state": "approved" },
                { "user": { "uuid": "{blocker}" }, "approved": false, "state": "changes_requested" },
                { "user": { "uuid": "{lurker}" }, "approved": false, "state": null }
            ]
        }"#;
        let pr: BitbucketPullRequest = serde_json::from_str(json).unwrap();

        assert_eq!(
            pr.viewer_review("{approver}"),
            Some(PullRequestReviewVerdict::Approve)
        );
        assert_eq!(
            pr.viewer_review("{blocker}"),
            Some(PullRequestReviewVerdict::RequestChanges)
        );
        // A participant who has neither approved nor blocked has no verdict.
        assert_eq!(pr.viewer_review("{lurker}"), None);
        // An account that is not a participant has no verdict.
        assert_eq!(pr.viewer_review("{stranger}"), None);
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

#[derive(Deserialize)]
pub(super) struct BitbucketBuildStatus {
    #[serde(default)]
    state: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct BitbucketRepository {
    #[serde(default)]
    pub(super) mainbranch: Option<BitbucketBranchName>,
}

#[derive(Deserialize)]
pub(super) struct BitbucketBranchName {
    #[serde(default)]
    pub(super) name: Option<String>,
}
