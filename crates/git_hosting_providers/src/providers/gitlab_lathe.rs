//! GitLab merge-request support for the pull-request panel.
//!
//! GitLab calls them merge requests and addresses them by `iid` (the per-project
//! number users see) rather than the global `id`; every endpoint here uses the
//! `iid`, which is what [`git::PullRequestSummary::number`] carries.

use super::*;

/// GitLab's REST API lives under `/api/v4` on the instance itself, for both
/// gitlab.com and self-managed deployments, so one shape covers every host.
pub(super) fn gitlab_api_base(base_url: &Url) -> Result<String> {
    let origin = crate::api_origin(base_url).context("GitLab base URL has no host")?;
    Ok(format!("{origin}/api/v4"))
}

/// GitLab addresses a repository by a single URL-encoded `owner/repo` path.
pub(super) fn gitlab_project_id(remote: &ParsedGitRemote) -> String {
    encode(&format!("{}/{}", remote.owner, remote.repo)).into_owned()
}

#[derive(Clone, Copy)]
pub(super) enum GitlabMethod {
    Get,
    Post,
    Put,
}

pub(super) fn gitlab_auth_header(auth: &Option<GitHostAuth>) -> Option<String> {
    match auth {
        Some(GitHostAuth::Bearer(token)) => Some(format!("Bearer {token}")),
        // GitLab has no Basic-auth API path; a Basic credential cannot be used.
        Some(GitHostAuth::Basic { .. }) => None,
        None => None,
    }
}

pub(super) fn gitlab_request(
    method: GitlabMethod,
    url: &str,
    auth: &Option<GitHostAuth>,
    json_body: Option<Vec<u8>>,
) -> Result<Request<AsyncBody>> {
    let builder = match method {
        GitlabMethod::Get => Request::get(url),
        GitlabMethod::Post => Request::post(url),
        GitlabMethod::Put => Request::put(url),
    };
    let mut builder = builder
        .header("Accept", "application/json")
        .header("User-Agent", "Lathe")
        .follow_redirects(http_client::RedirectPolicy::FollowAll);
    if let Some(value) = gitlab_auth_header(auth) {
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

pub(super) async fn gitlab_send(
    client: &Arc<dyn HttpClient>,
    request: Request<AsyncBody>,
    host: &str,
    context: &str,
) -> Result<Vec<u8>> {
    let mut response = client
        .send(request)
        .await
        .with_context(|| format!("error while {context}"))?;
    let mut bytes = Vec::new();
    response.body_mut().read_to_end(&mut bytes).await?;
    // A revoked or expired token is reported as 401 both directly and, for some
    // endpoints, as 403 with an explicit message; surface the recoverable case
    // so the UI can offer a targeted reconnect.
    let status = response.status().as_u16();
    if status == 401 {
        return Err(PullRequestAuthError {
            host: host.to_string().into(),
        }
        .into());
    }
    if !response.status().is_success() {
        let text = String::from_utf8_lossy(&bytes);
        bail!("{context} failed ({status}): {text:?}");
    }
    Ok(bytes)
}

/// Fetches a paginated GitLab collection, following the `X-Next-Page` header
/// until `max_items` are collected or the pages run out.
pub(super) async fn gitlab_get_paginated<T: serde::de::DeserializeOwned>(
    client: &Arc<dyn HttpClient>,
    base_url: &str,
    auth: &Option<GitHostAuth>,
    host: &str,
    context: &str,
    max_items: usize,
) -> Result<Vec<T>> {
    let mut collected: Vec<T> = Vec::new();
    let mut page = 1u32;
    loop {
        let separator = if base_url.contains('?') { '&' } else { '?' };
        let url = format!("{base_url}{separator}per_page=100&page={page}");
        let request = gitlab_request(GitlabMethod::Get, &url, auth, None)?;
        let bytes = gitlab_send(client, request, host, context).await?;
        let batch: Vec<T> = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing response while {context}"))?;
        let batch_len = batch.len();
        collected.extend(batch);
        if batch_len < 100 || collected.len() >= max_items {
            break;
        }
        page += 1;
    }
    collected.truncate(max_items);
    Ok(collected)
}

impl Gitlab {
    pub(super) fn api_base(&self) -> Result<String> {
        gitlab_api_base(&self.base_url)
    }

    pub(super) fn api_host(&self) -> String {
        self.base_url
            .host_str()
            .unwrap_or("gitlab.com")
            .to_string()
    }

    /// The username the supplied credential authenticates as, used both to
    /// resolve the "mine" filters and to label the connected account.
    pub(super) async fn fetch_authenticated_username(
        &self,
        auth: &Option<GitHostAuth>,
        http_client: &Arc<dyn HttpClient>,
    ) -> Result<Option<SharedString>> {
        if gitlab_auth_header(auth).is_none() {
            return Ok(None);
        }
        let api = self.api_base()?;
        let request = gitlab_request(GitlabMethod::Get, &format!("{api}/user"), auth, None)?;
        let bytes = gitlab_send(
            http_client,
            request,
            &self.api_host(),
            "fetching authenticated GitLab user",
        )
        .await?;
        let user: GitlabUser =
            serde_json::from_slice(&bytes).context("parsing authenticated GitLab user")?;
        Ok(Some(user.username.into()))
    }

    /// The approval state of a merge request, which GitLab models separately
    /// from its notes rather than as a review object.
    pub(super) async fn fetch_approvals(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        auth: &Option<GitHostAuth>,
        http_client: &Arc<dyn HttpClient>,
    ) -> Result<GitlabApprovals> {
        let api = self.api_base()?;
        let project = gitlab_project_id(remote);
        let url = format!("{api}/projects/{project}/merge_requests/{number}/approvals");
        let request = gitlab_request(GitlabMethod::Get, &url, auth, None)?;
        let bytes = gitlab_send(
            http_client,
            request,
            &self.api_host(),
            "fetching GitLab merge request approvals",
        )
        .await?;
        serde_json::from_slice(&bytes).context("parsing GitLab merge request approvals")
    }

    /// Builds the reviewer roll-up from GitLab's two separate sources: the
    /// assigned `reviewers` on the merge request, and the `approved_by` list on
    /// the approvals endpoint. A reviewer who has approved gets an `Approve`
    /// verdict; one who is merely assigned is pending.
    pub(super) async fn fetch_reviewers(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        auth: &Option<GitHostAuth>,
        http_client: &Arc<dyn HttpClient>,
    ) -> Result<Vec<PullRequestReviewer>> {
        let api = self.api_base()?;
        let project = gitlab_project_id(remote);
        let url = format!("{api}/projects/{project}/merge_requests/{number}");
        let request = gitlab_request(GitlabMethod::Get, &url, auth, None)?;
        let bytes = gitlab_send(
            http_client,
            request,
            &self.api_host(),
            "fetching GitLab merge request",
        )
        .await?;
        let merge_request: GitlabMergeRequest =
            serde_json::from_slice(&bytes).context("parsing GitLab merge request")?;
        let approvals = self
            .fetch_approvals(remote, number, auth, http_client)
            .await
            .unwrap_or_default();
        let viewer = self
            .fetch_authenticated_username(auth, http_client)
            .await
            .unwrap_or_default();

        let approved: Vec<String> = approvals
            .approved_by
            .iter()
            .filter_map(|entry| entry.user.as_ref().map(|user| user.username.clone()))
            .collect();

        let mut reviewers: Vec<PullRequestReviewer> = Vec::new();
        let mut push = |username: &str, verdict: Option<PullRequestReviewVerdict>| {
            if reviewers
                .iter()
                .any(|existing| existing.login.as_ref() == username)
            {
                return;
            }
            reviewers.push(PullRequestReviewer {
                login: username.to_string().into(),
                verdict,
                is_me: viewer
                    .as_ref()
                    .is_some_and(|me| me.as_ref().eq_ignore_ascii_case(username)),
            });
        };
        for username in &approved {
            push(username, Some(PullRequestReviewVerdict::Approve));
        }
        for reviewer in &merge_request.reviewers {
            push(&reviewer.username, None);
        }
        Ok(reviewers)
    }

    /// Sets a merge request's `state_event`, which is how GitLab models both
    /// closing and reopening.
    pub(super) async fn set_merge_request_state_event(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        state_event: &str,
        auth: &Option<GitHostAuth>,
        http_client: &Arc<dyn HttpClient>,
    ) -> Result<()> {
        let api = self.api_base()?;
        let project = gitlab_project_id(remote);
        let url = format!("{api}/projects/{project}/merge_requests/{number}");
        let payload = serde_json::json!({ "state_event": state_event });
        let request = gitlab_request(
            GitlabMethod::Put,
            &url,
            auth,
            Some(serde_json::to_vec(&payload)?),
        )?;
        gitlab_send(
            http_client,
            request,
            &self.api_host(),
            "updating GitLab merge request state",
        )
        .await?;
        Ok(())
    }

    /// Resolves reviewer usernames to GitLab account ids.
    pub(super) async fn resolve_reviewer_ids(
        &self,
        reviewers: &[SharedString],
        auth: &Option<GitHostAuth>,
        http_client: &Arc<dyn HttpClient>,
    ) -> Result<Vec<u64>> {
        let api = self.api_base()?;
        let mut ids = Vec::new();
        for reviewer in reviewers {
            let wanted = reviewer.trim();
            if wanted.is_empty() {
                continue;
            }
            let url = format!("{api}/users?username={}", encode(wanted));
            let request = gitlab_request(GitlabMethod::Get, &url, auth, None)?;
            let bytes = gitlab_send(
                http_client,
                request,
                &self.api_host(),
                "looking up GitLab user",
            )
            .await?;
            let users: Vec<GitlabUser> =
                serde_json::from_slice(&bytes).context("parsing GitLab user lookup")?;
            match users.into_iter().find_map(|user| user.id) {
                Some(id) => ids.push(id),
                None => bail!("no GitLab user named '{wanted}'"),
            }
        }
        Ok(ids)
    }

    /// The id of the discussion containing `note_id`. GitLab replies target a
    /// discussion rather than a note, and the review-comment type only carries
    /// note ids, so the mapping is resolved on demand.
    pub(super) async fn find_discussion_for_note(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        note_id: u64,
        auth: &Option<GitHostAuth>,
        http_client: &Arc<dyn HttpClient>,
    ) -> Result<String> {
        let api = self.api_base()?;
        let project = gitlab_project_id(remote);
        let url = format!("{api}/projects/{project}/merge_requests/{number}/discussions");
        let discussions: Vec<GitlabDiscussion> = gitlab_get_paginated(
            http_client,
            &url,
            auth,
            &self.api_host(),
            "fetching GitLab merge request discussions",
            500,
        )
        .await?;
        discussions
            .into_iter()
            .find(|discussion| discussion.notes.iter().any(|note| note.id == note_id))
            .map(|discussion| discussion.id)
            .context("could not find the GitLab discussion this comment belongs to")
    }

    /// CI results for a merge request's head commit, from GitLab's pipeline API.
    pub(super) async fn fetch_checks(
        &self,
        remote: &ParsedGitRemote,
        number: u32,
        auth: &Option<GitHostAuth>,
        http_client: &Arc<dyn HttpClient>,
    ) -> Result<Option<PullRequestChecks>> {
        let api = self.api_base()?;
        let project = gitlab_project_id(remote);
        let url = format!("{api}/projects/{project}/merge_requests/{number}/pipelines");
        let pipelines: Vec<GitlabPipeline> = gitlab_get_paginated(
            http_client,
            &url,
            auth,
            &self.api_host(),
            "fetching GitLab merge request pipelines",
            100,
        )
        .await?;
        // Pipelines come back newest first, and only the latest one describes the
        // current head; older entries are previous pushes.
        let Some(latest) = pipelines.first() else {
            return Ok(None);
        };
        let mut checks = PullRequestChecks {
            succeeded: 0,
            failed: 0,
            pending: 0,
            neutral: 0,
        };
        match latest.status.as_str() {
            "success" => checks.succeeded += 1,
            "failed" => checks.failed += 1,
            "running" | "pending" | "created" | "waiting_for_resource" | "preparing"
            | "scheduled" => checks.pending += 1,
            _ => checks.neutral += 1,
        }
        Ok(Some(checks))
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct GitlabUser {
    pub(super) username: String,
    /// Numeric account id. GitLab's write endpoints address reviewers by id,
    /// never by username.
    #[serde(default)]
    pub(super) id: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct GitlabApprovals {
    #[serde(default)]
    pub(super) approved_by: Vec<GitlabApprovedBy>,
}

#[derive(Debug, Deserialize)]
pub(super) struct GitlabApprovedBy {
    #[serde(default)]
    pub(super) user: Option<GitlabUser>,
}

#[derive(Debug, Deserialize)]
pub(super) struct GitlabPipeline {
    #[serde(default)]
    pub(super) status: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct GitlabMergeRequest {
    pub(super) iid: u32,
    #[serde(default)]
    pub(super) title: String,
    #[serde(default)]
    pub(super) description: Option<String>,
    #[serde(default)]
    pub(super) state: String,
    #[serde(default)]
    pub(super) author: Option<GitlabUser>,
    #[serde(default)]
    pub(super) reviewers: Vec<GitlabUser>,
    #[serde(default)]
    pub(super) source_branch: String,
    #[serde(default)]
    pub(super) target_branch: String,
    #[serde(default)]
    pub(super) web_url: String,
    #[serde(default)]
    pub(super) created_at: String,
    #[serde(default)]
    pub(super) updated_at: String,
    #[serde(default)]
    pub(super) draft: bool,
    /// `can_be_merged`, `cannot_be_merged`, or `checking` while GitLab works it
    /// out. Absent on the list endpoint.
    #[serde(default)]
    pub(super) merge_status: Option<String>,
    #[serde(default)]
    pub(super) sha: Option<String>,
    #[serde(default)]
    pub(super) diff_refs: Option<GitlabDiffRefs>,
    #[serde(default)]
    pub(super) changes_count: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct GitlabDiffRefs {
    #[serde(default)]
    pub(super) base_sha: Option<String>,
    #[serde(default)]
    pub(super) head_sha: Option<String>,
    #[serde(default)]
    pub(super) start_sha: Option<String>,
}

/// Strips GitLab's `Draft:` / `WIP:` title prefix, which is how it encodes
/// draft state, so a title can be round-tripped without stacking prefixes.
pub(super) fn strip_draft_prefix(title: &str) -> &str {
    let trimmed = title.trim_start();
    for prefix in ["Draft:", "draft:", "WIP:", "wip:"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return rest.trim_start();
        }
    }
    trimmed
}

impl GitlabMergeRequest {
    /// Ids of the accounts currently assigned as reviewers.
    pub(super) fn reviewer_ids(&self) -> Vec<u64> {
        self.reviewers.iter().filter_map(|user| user.id).collect()
    }

    pub(super) fn pull_request_state(&self) -> PullRequestState {
        match self.state.as_str() {
            "merged" => PullRequestState::Merged,
            "closed" | "locked" => PullRequestState::Closed,
            _ => PullRequestState::Open,
        }
    }

    pub(super) fn author_login(&self) -> SharedString {
        self.author
            .as_ref()
            .map(|user| SharedString::from(user.username.clone()))
            .unwrap_or_default()
    }

    pub(super) fn into_summary(self, state: PullRequestState) -> Result<PullRequestSummary> {
        let url = Url::parse(&self.web_url).context("parsing merge request URL")?;
        let author_login = self.author_login();
        Ok(PullRequestSummary {
            number: self.iid,
            title: self.title.into(),
            author_login,
            state,
            source_branch: self.source_branch.into(),
            target_branch: self.target_branch.into(),
            url,
            updated_at: self.updated_at.into(),
            is_draft: self.draft,
        })
    }

    pub(super) fn into_detail(self) -> Result<PullRequestDetail> {
        let state = self.pull_request_state();
        let url = Url::parse(&self.web_url).context("parsing merge request URL")?;
        let author_login = self.author_login();
        let is_mergeable = match self.merge_status.as_deref() {
            Some("can_be_merged") => Some(true),
            Some("cannot_be_merged") => Some(false),
            // "checking" and "unchecked" mean GitLab has not decided yet.
            _ => None,
        };
        let diff_refs = self.diff_refs.clone().unwrap_or(GitlabDiffRefs {
            base_sha: None,
            head_sha: None,
            start_sha: None,
        });
        let head_sha = diff_refs
            .head_sha
            .clone()
            .or_else(|| self.sha.clone())
            .unwrap_or_default();
        let base_sha = diff_refs
            .base_sha
            .clone()
            .or_else(|| diff_refs.start_sha.clone())
            .unwrap_or_default();
        // GitLab reports the changed-file count as a string that may be
        // approximate ("1000+"), so take the leading digits and treat anything
        // else as unknown.
        let changed_files = self
            .changes_count
            .as_deref()
            .map(|count| {
                count
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
            })
            .and_then(|digits| digits.parse::<u32>().ok())
            .unwrap_or(0);
        Ok(PullRequestDetail {
            number: self.iid,
            title: self.title.into(),
            body: self.description.unwrap_or_default().into(),
            state,
            author_login,
            source_branch: self.source_branch.into(),
            target_branch: self.target_branch.into(),
            head_sha: head_sha.into(),
            base_sha: base_sha.into(),
            url,
            created_at: self.created_at.into(),
            updated_at: self.updated_at.into(),
            is_draft: self.draft,
            is_mergeable,
            // GitLab reports line counts only on the diff endpoint, which the
            // detail view fetches separately; leaving them zero keeps the header
            // honest rather than inventing numbers.
            additions: 0,
            deletions: 0,
            changed_files,
            commits: None,
            behind_by: None,
            viewer_review: None,
            reviewers: Vec::new(),
            checks: None,
            // Filled in by `get_pull_request` once the viewer is resolved.
            viewer_is_author: None,
        })
    }
}

/// One file's diff as GitLab returns it. GitLab has no "download the whole patch"
/// endpoint, so a unified diff is reassembled from these.
#[derive(Debug, Deserialize)]
pub(super) struct GitlabDiffFile {
    #[serde(default)]
    pub(super) old_path: String,
    #[serde(default)]
    pub(super) new_path: String,
    #[serde(default)]
    pub(super) diff: String,
    #[serde(default)]
    pub(super) new_file: bool,
    #[serde(default)]
    pub(super) deleted_file: bool,
}

/// Reassembles GitLab's per-file diff objects into the single unified-diff text
/// the pull-request view parses, matching what `git diff` and GitHub's `.diff`
/// endpoint produce.
pub(super) fn gitlab_unified_diff(files: &[GitlabDiffFile]) -> String {
    let mut out = String::new();
    for file in files {
        // GitLab omits the headers and gives only the hunks, so synthesize the
        // `diff --git` / `---` / `+++` preamble the parser keys off.
        out.push_str(&format!(
            "diff --git a/{} b/{}\n",
            file.old_path, file.new_path
        ));
        if file.new_file {
            out.push_str("--- /dev/null\n");
        } else {
            out.push_str(&format!("--- a/{}\n", file.old_path));
        }
        if file.deleted_file {
            out.push_str("+++ /dev/null\n");
        } else {
            out.push_str(&format!("+++ b/{}\n", file.new_path));
        }
        out.push_str(&file.diff);
        if !file.diff.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// A discussion note. GitLab groups inline review comments into discussions;
/// the first note carries the diff position and the rest are replies.
#[derive(Debug, Deserialize)]
pub(super) struct GitlabDiscussion {
    pub(super) id: String,
    #[serde(default)]
    pub(super) notes: Vec<GitlabNote>,
}

#[derive(Debug, Deserialize)]
pub(super) struct GitlabNote {
    pub(super) id: u64,
    #[serde(default)]
    pub(super) body: String,
    #[serde(default)]
    pub(super) author: Option<GitlabUser>,
    #[serde(default)]
    pub(super) created_at: String,
    #[serde(default)]
    pub(super) system: bool,
    #[serde(default)]
    pub(super) resolved: bool,
    #[serde(default)]
    pub(super) position: Option<GitlabNotePosition>,
}

#[derive(Debug, Deserialize)]
pub(super) struct GitlabNotePosition {
    #[serde(default)]
    pub(super) new_path: Option<String>,
    #[serde(default)]
    pub(super) old_path: Option<String>,
    #[serde(default)]
    pub(super) new_line: Option<u32>,
    #[serde(default)]
    pub(super) old_line: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub(super) struct GitlabProject {
    #[serde(default)]
    pub(super) default_branch: Option<String>,
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_api_base_uses_api_v4_on_gitlab_dot_com() {
        let gitlab = Gitlab::public_instance();

        assert_eq!(gitlab.api_base().unwrap(), "https://gitlab.com/api/v4");
    }

    // Unlike GitHub, GitLab serves its API from the instance itself on every
    // deployment, so self-managed hosts take the same shape as gitlab.com with
    // no special case.
    #[test]
    fn test_api_base_uses_the_same_shape_for_a_self_managed_host() {
        let gitlab = Gitlab::new(
            "GitLab Self-Managed",
            Url::parse("https://gitlab.acme.com").unwrap(),
        );

        assert_eq!(gitlab.api_base().unwrap(), "https://gitlab.acme.com/api/v4");
    }

    // A self-managed instance is only reachable at the scheme and port it is
    // actually served on, so both have to survive into the API base.
    #[test]
    fn test_api_base_keeps_a_non_default_port() {
        let gitlab = Gitlab::new(
            "GitLab Self-Managed",
            Url::parse("https://gitlab.acme.com:8443").unwrap(),
        );

        assert_eq!(
            gitlab.api_base().unwrap(),
            "https://gitlab.acme.com:8443/api/v4"
        );
    }

    #[test]
    fn test_api_base_keeps_a_plain_http_scheme() {
        let gitlab = Gitlab::new(
            "GitLab Self-Managed",
            Url::parse("http://gitlab.internal").unwrap(),
        );

        assert_eq!(gitlab.api_base().unwrap(), "http://gitlab.internal/api/v4");
    }

    #[test]
    fn test_api_base_errors_when_the_base_url_has_no_host() {
        let message = gitlab_api_base(&Url::parse("mailto:nobody@example.com").unwrap())
            .expect_err("a URL without a host has no API base")
            .to_string();

        assert_eq!(message, "GitLab base URL has no host");
    }

    #[test]
    fn test_api_host_reports_the_configured_host() {
        let gitlab = Gitlab::new(
            "GitLab Self-Managed",
            Url::parse("https://gitlab.acme.com").unwrap(),
        );

        assert_eq!(gitlab.api_host(), "gitlab.acme.com");
    }

    // `api_host` only labels errors and telemetry, so it degrades to the public
    // host instead of failing the way `api_base` does.
    #[test]
    fn test_api_host_falls_back_to_the_public_host_when_the_url_has_no_host() {
        let gitlab = Gitlab::new("Hostless", Url::parse("mailto:nobody@example.com").unwrap());

        assert_eq!(gitlab.api_host(), "gitlab.com");
    }
}

#[cfg(test)]
mod draft_prefix_tests {
    use super::strip_draft_prefix;

    #[test]
    fn strips_every_prefix_gitlab_recognises() {
        assert_eq!(strip_draft_prefix("Draft: Add metrics"), "Add metrics");
        assert_eq!(strip_draft_prefix("draft: Add metrics"), "Add metrics");
        assert_eq!(strip_draft_prefix("WIP: Add metrics"), "Add metrics");
        assert_eq!(strip_draft_prefix("wip: Add metrics"), "Add metrics");
    }

    #[test]
    fn leaves_an_ordinary_title_alone() {
        assert_eq!(strip_draft_prefix("Add metrics"), "Add metrics");
        // A title merely mentioning drafts is not itself a draft marker.
        assert_eq!(
            strip_draft_prefix("Rework the draft: handling"),
            "Rework the draft: handling"
        );
    }

    #[test]
    fn stripping_is_idempotent_so_prefixes_cannot_stack() {
        let once = strip_draft_prefix("Draft: Add metrics");
        assert_eq!(strip_draft_prefix(once), once);
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct GitlabMember {
    pub(super) username: String,
    #[serde(default)]
    pub(super) name: Option<String>,
}
