//! Loads a repository's pull requests from its host, for the pull request
//! panel and for anything else that lists them the same way (the mobile
//! companion does).

use anyhow::{Context as _, Result};
use git::{
    GitHostAuth, GitHostingProvider, GitHostingProviderRegistry, ParsedGitRemote,
    PullRequestListFilter, PullRequestState, PullRequestSummary, parse_git_remote_url,
};
use gpui::http_client::HttpClient;
use gpui::{App, Entity, EntityId, SharedString};
use project::{Project, git_store::Repository, repo_identity_path};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use util::ResultExt as _;

/// How many pull requests one page of results holds. The panel loads a page at a
/// time and appends more on demand, so a busy repository is paged through rather
/// than silently truncated at a fixed cap.
pub const PAGE_SIZE: u32 = 50;

/// The two partitions the panel renders. `authored` holds PRs opened by the
/// connected account (rendered in a "Created by you" section at the bottom); `others`
/// is the rest of the list with the authored PRs removed so no PR appears
/// twice. `authored` is only populated when an account is connected and the
/// review-requested filter is off.
#[derive(Clone, Default)]
pub struct LoadedPullRequests {
    pub authored: Vec<PullRequestSummary>,
    pub others: Vec<PullRequestSummary>,
    /// True when the host returned a full page for `others`, so there may be
    /// more results behind it and a "Load more" row is worth offering.
    pub may_have_more: bool,
}

impl LoadedPullRequests {
    pub fn total(&self) -> usize {
        self.authored.len() + self.others.len()
    }

    pub fn is_empty(&self) -> bool {
        self.authored.is_empty() && self.others.is_empty()
    }

    /// Every PR number across both partitions, used to drive reviewer
    /// enrichment without fetching the same PR twice.
    pub fn numbers(&self) -> Vec<u32> {
        let mut seen = HashSet::new();
        self.authored
            .iter()
            .chain(self.others.iter())
            .map(|summary| summary.number)
            .filter(|number| seen.insert(*number))
            .collect()
    }
}

pub enum LoadOutcome {
    Loaded {
        provider: Arc<dyn GitHostingProvider + Send + Sync>,
        remote: ParsedGitRemote,
        loaded: LoadedPullRequests,
    },
    NoHost(SharedString),
}

/// Loads `pages` pages and concatenates them into one result, mirroring how
/// `load_more` extends a section. Used so a background refresh can rebuild a
/// list the user has paged through at its current length instead of truncating
/// it to the first page.
pub async fn load_pull_request_pages(
    candidates: Vec<String>,
    registry: Arc<GitHostingProviderRegistry>,
    states: Option<Vec<PullRequestState>>,
    reviewing: bool,
    pages: u32,
    http_client: Arc<dyn HttpClient>,
    workspace_id: EntityId,
    cx: &mut gpui::AsyncApp,
) -> Result<LoadOutcome> {
    let mut combined: Option<LoadOutcome> = None;
    for page in 1..=pages.max(1) {
        let outcome = load_pull_requests(
            candidates.clone(),
            registry.clone(),
            states.clone(),
            reviewing,
            page,
            http_client.clone(),
            workspace_id,
            cx,
        )
        .await?;

        match (&mut combined, outcome) {
            (None, outcome) => combined = Some(outcome),
            // A later page reporting no host is not a reason to discard the
            // pages already collected.
            (Some(_), LoadOutcome::NoHost(_)) => break,
            (
                Some(LoadOutcome::Loaded {
                    loaded: existing, ..
                }),
                LoadOutcome::Loaded { loaded: next, .. },
            ) => {
                // `authored` is re-queried whole on every page, so the first
                // page's copy is already complete. Only the paged partition
                // grows, deduplicated by number in case the host repeats a row
                // across a page boundary.
                let seen: HashSet<u32> = existing
                    .authored
                    .iter()
                    .chain(existing.others.iter())
                    .map(|summary| summary.number)
                    .collect();
                existing.others.extend(
                    next.others
                        .into_iter()
                        .filter(|s| !seen.contains(&s.number)),
                );
                existing.may_have_more = next.may_have_more;
                if !existing.may_have_more {
                    break;
                }
            }
            (Some(LoadOutcome::NoHost(_)), _) => break,
        }
    }
    combined.ok_or_else(|| anyhow::anyhow!("no pull request pages were requested"))
}

/// Queries `candidates` (origin first, then upstream when set) and returns the
/// pull requests of the first remote that resolves to a known host and has a
/// credential. Stopping at the first usable remote is deliberate: a fork shows
/// its own `origin` pull requests (often none) rather than falling through to
/// the `upstream` project and surfacing the canonical repository's PRs.
pub async fn load_pull_requests(
    candidates: Vec<String>,
    registry: Arc<GitHostingProviderRegistry>,
    states: Option<Vec<PullRequestState>>,
    reviewing: bool,
    page: u32,
    http_client: Arc<dyn HttpClient>,
    workspace_id: EntityId,
    cx: &mut gpui::AsyncApp,
) -> Result<LoadOutcome> {
    let mut chosen: Option<(Arc<dyn GitHostingProvider + Send + Sync>, ParsedGitRemote)> = None;
    let mut last_summaries: Vec<PullRequestSummary> = Vec::new();
    // Auth for the chosen candidate, reused for the second "authored by me" call
    // so we do not re-read the keychain.
    let mut chosen_auth: Option<GitHostAuth> = None;
    let mut connect_hint: Option<SharedString> = None;

    for remote_url in candidates {
        let Some((provider, parsed)) = parse_git_remote_url(registry.clone(), &remote_url) else {
            continue;
        };
        // Resolve the credential for THIS repo's host only. We never fall back to
        // another host's token, so a Bitbucket repository without a Bitbucket
        // credential surfaces a connect prompt instead of an empty GitHub result.
        let auth = git::git_host_credentials::auth_for_repository(
            cx,
            Some(workspace_id),
            provider.clone(),
            &parsed,
            http_client.clone(),
        )
        .await
        .ok()
        .flatten();
        if auth.is_none() {
            if connect_hint.is_none() {
                connect_hint = Some(
                    format!(
                        "Connect {} from the account menu to see this repository's pull requests.",
                        provider.name()
                    )
                    .into(),
                );
            }
            continue;
        }
        let list_filter = PullRequestListFilter {
            states: states.clone(),
            author: None,
            reviewer_is_me: reviewing,
            author_is_me: false,
            limit: Some(PAGE_SIZE),
            page: Some(page),
        };
        let remote_for_call = ParsedGitRemote {
            owner: parsed.owner.clone(),
            repo: parsed.repo.clone(),
        };
        let summaries = provider
            .list_pull_requests(
                &remote_for_call,
                list_filter,
                auth.clone(),
                http_client.clone(),
            )
            .await
            .with_context(|| {
                format!("listing pull requests for {}/{}", parsed.owner, parsed.repo)
            })?;
        chosen = Some((provider, parsed));
        last_summaries = summaries;
        chosen_auth = auth;
        break;
    }

    let Some((provider, remote)) = chosen else {
        if let Some(hint) = connect_hint {
            return Ok(LoadOutcome::NoHost(hint));
        }
        return Ok(LoadOutcome::NoHost(
            "This repository's remote does not match any known hosting provider.".into(),
        ));
    };

    // A full page back means the host may have more behind it.
    let may_have_more = last_summaries.len() as u32 >= PAGE_SIZE;

    // With an account connected, also fetch the viewer's own PRs so the panel
    // can surface them in a "Created by you" section. Skipped in review mode,
    // where the list is already scoped to review requests (which exclude your
    // own PRs), and on later pages, where the section is already populated.
    // A failure here (for example a host that cannot resolve the authenticated
    // user) leaves the section empty rather than failing the load.
    let authored = if reviewing || page > 1 {
        Vec::new()
    } else if let Some(auth) = chosen_auth {
        let authored_filter = PullRequestListFilter {
            states: states.clone(),
            author: None,
            reviewer_is_me: false,
            author_is_me: true,
            limit: Some(PAGE_SIZE),
            page: None,
        };
        let remote_for_authored = ParsedGitRemote {
            owner: remote.owner.clone(),
            repo: remote.repo.clone(),
        };
        provider
            .list_pull_requests(
                &remote_for_authored,
                authored_filter,
                Some(auth),
                http_client,
            )
            .await
            .with_context(|| {
                format!(
                    "listing authored pull requests for {}/{}",
                    remote.owner, remote.repo
                )
            })
            .log_err()
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let authored_numbers: HashSet<u32> = authored.iter().map(|summary| summary.number).collect();
    let others: Vec<PullRequestSummary> = last_summaries
        .into_iter()
        .filter(|summary| !authored_numbers.contains(&summary.number))
        .collect();

    Ok(LoadOutcome::Loaded {
        provider,
        remote,
        loaded: LoadedPullRequests {
            authored,
            others,
            may_have_more,
        },
    })
}

/// The remotes worth querying for a repository, origin first and then upstream
/// when it is set to something different.
pub fn remote_candidates(repository: &Repository) -> Vec<String> {
    let snapshot = repository.snapshot();
    let mut candidates = Vec::new();
    if let Some(origin) = snapshot.remote_origin_url.clone() {
        candidates.push(origin);
    }
    if let Some(upstream) = snapshot.remote_upstream_url
        && !candidates.contains(&upstream)
    {
        candidates.push(upstream);
    }
    candidates
}

/// A one-line summary of a load failure the user can act on. The raw error
/// chain is useful for a bug report and unhelpful as headline copy.
pub fn failure_summary(error: &anyhow::Error) -> SharedString {
    let lowercase = format!("{error:#}").to_lowercase();
    // Map the failures users actually hit onto copy that says what to do.
    // Everything else keeps a neutral summary.
    if lowercase.contains("rate limit") {
        "The host's API rate limit was reached. Try again shortly.".into()
    } else if lowercase.contains("403") || lowercase.contains("forbidden") {
        "Your account cannot read pull requests in this repository.".into()
    } else if lowercase.contains("404") || lowercase.contains("not found") {
        "The repository was not found on the host. Check the remote URL.".into()
    } else if lowercase.contains("dns")
        || lowercase.contains("connect")
        || lowercase.contains("timed out")
        || lowercase.contains("timeout")
    {
        "Could not reach the host. Check your network connection.".into()
    } else {
        "Could not load pull requests from the host.".into()
    }
}

/// A repository whose pull requests are listed, with the label to list them under.
pub struct RepositoryEntry {
    /// Where the repository's git data lives, shared by a checkout and its
    /// linked worktrees. Stable across restarts. `None` when it cannot be
    /// resolved locally.
    pub identity: Option<PathBuf>,
    pub repository: Entity<Repository>,
    pub label: SharedString,
}

/// The project's repositories whose pull requests to list, ordered by label.
pub fn pull_request_repositories(project: &Project, cx: &App) -> Vec<RepositoryEntry> {
    let all_repositories: Vec<Entity<Repository>> = project
        .git_store()
        .read(cx)
        .repositories()
        .values()
        .cloned()
        .collect();

    // One section per repository *identity*, not per `Repository`. A linked
    // worktree is its own repository in the git store but shares a remote
    // with its main checkout, so rendering one section each would list the
    // same pull requests twice, the second time under the worktree's
    // directory name (`.pr12-review` rather than `offline-mode`).
    //
    // The main checkout is preferred as the representative so the label and
    // the remote come from it; when only a worktree is open, the first
    // repository seen for that identity stands in.
    let mut grouped: Vec<(Option<PathBuf>, Entity<Repository>, SharedString)> = Vec::new();
    for repository in all_repositories {
        let (identity, is_main, label) = {
            let repo = repository.read(cx);
            let snapshot = repo.snapshot();
            let identity = Some(
                repo_identity_path(&snapshot.common_dir_abs_path, snapshot.path_style)
                    .to_path_buf(),
            );
            let label = identity
                .as_deref()
                .and_then(|identity| {
                    if identity.extension() == Some(std::ffi::OsStr::new("git")) {
                        identity.file_stem()
                    } else {
                        identity.file_name()
                    }
                })
                .and_then(|name| name.to_str())
                .map(SharedString::from)
                .unwrap_or_else(|| repo.display_name());
            (identity, snapshot.is_main_worktree(), label)
        };

        // A repository whose identity cannot be resolved locally never
        // groups; it keeps a section of its own, as before.
        let existing = identity.as_ref().and_then(|identity| {
            grouped
                .iter_mut()
                .find(|(other, _, _)| other.as_ref() == Some(identity))
        });
        match existing {
            Some(entry) => {
                if is_main {
                    entry.1 = repository;
                    entry.2 = label;
                }
            }
            None => grouped.push((identity, repository, label)),
        }
    }
    grouped.sort_by_key(|(_, _, label)| label.to_lowercase());
    grouped
        .into_iter()
        .map(|(identity, repository, label)| RepositoryEntry {
            identity,
            repository,
            label,
        })
        .collect()
}
