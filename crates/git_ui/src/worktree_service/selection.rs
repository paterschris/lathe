use std::path::PathBuf;

use collections::HashSet;
use gpui::Entity;
use project::{Project, git_store::Repository};
use zed_actions::NewWorktreeBranchTarget;

/// A remote-tracking branch reference parsed into its remote and branch parts,
/// e.g. `origin/main` -> remote `origin`, branch `main`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteBranchName {
    pub remote_name: String,
    pub branch_name: String,
}

impl RemoteBranchName {
    pub fn parse(name: &str) -> Option<Self> {
        let name = name.strip_prefix("refs/remotes/").unwrap_or(name);
        let (remote_name, branch_name) = name.split_once('/')?;
        if remote_name.is_empty() || branch_name.is_empty() {
            return None;
        }
        Some(Self {
            remote_name: remote_name.to_string(),
            branch_name: branch_name.to_string(),
        })
    }

    pub fn display_name(&self) -> String {
        format!("{}/{}", self.remote_name, self.branch_name)
    }
}

/// A "create new worktree" option offered to the user. The set of targets is
/// derived from repository state by [`worktree_create_targets`] so that the
/// worktree picker and the sidebar's new-thread menu stay in sync.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorktreeCreateTarget {
    CurrentBranch,
    DefaultBranch(RemoteBranchName),
}

impl WorktreeCreateTarget {
    pub fn branch_target(&self) -> NewWorktreeBranchTarget {
        match self {
            WorktreeCreateTarget::CurrentBranch => NewWorktreeBranchTarget::CurrentBranch,
            WorktreeCreateTarget::DefaultBranch(default_branch) => {
                NewWorktreeBranchTarget::RemoteBranch {
                    remote_name: default_branch.remote_name.clone(),
                    branch_name: default_branch.branch_name.clone(),
                }
            }
        }
    }

    pub fn branch_label(
        &self,
        has_multiple_repositories: bool,
        current_branch_name: Option<&str>,
    ) -> String {
        match self {
            WorktreeCreateTarget::DefaultBranch(default_branch) => default_branch.display_name(),
            WorktreeCreateTarget::CurrentBranch => {
                if has_multiple_repositories {
                    "current branches".to_string()
                } else {
                    current_branch_name.unwrap_or("HEAD").to_string()
                }
            }
        }
    }
}

/// Determines which "create new worktree" options to surface for the given
/// repository state: prefer the remote default branch when it differs from the
/// current branch, and otherwise offer the current branch.
pub fn worktree_create_targets(
    has_multiple_repositories: bool,
    default_branch: Option<RemoteBranchName>,
    current_branch_name: Option<&str>,
) -> Vec<WorktreeCreateTarget> {
    if has_multiple_repositories {
        return vec![WorktreeCreateTarget::CurrentBranch];
    }
    let Some(default_branch) = default_branch else {
        return vec![WorktreeCreateTarget::CurrentBranch];
    };
    let is_different =
        current_branch_name.is_none_or(|current| current != default_branch.branch_name);
    let mut targets = vec![WorktreeCreateTarget::DefaultBranch(default_branch)];
    if is_different {
        targets.push(WorktreeCreateTarget::CurrentBranch);
    }
    targets
}

/// Classifies the project's visible worktrees into git-managed repositories
/// and non-git paths. Each unique repository is returned only once.
pub fn classify_worktrees(
    project: &Project,
    cx: &gpui::App,
) -> (Vec<Entity<Repository>>, Vec<PathBuf>) {
    let repositories = project.repositories(cx).clone();
    let mut git_repos: Vec<Entity<Repository>> = Vec::new();
    let mut non_git_paths: Vec<PathBuf> = Vec::new();
    let mut seen_repo_ids = HashSet::default();

    for worktree in project.visible_worktrees(cx) {
        let worktree_path = worktree.read(cx).abs_path();

        let matching_repo = repositories
            .iter()
            .filter_map(|(id, repo)| {
                let work_dir = repo.read(cx).work_directory_abs_path.clone();
                if worktree_path.starts_with(work_dir.as_ref()) {
                    Some((*id, repo.clone(), work_dir.as_ref().components().count()))
                } else {
                    None
                }
            })
            .max_by(
                |(left_id, _left_repo, left_depth), (right_id, _right_repo, right_depth)| {
                    left_depth
                        .cmp(right_depth)
                        .then_with(|| left_id.cmp(right_id))
                },
            );

        if let Some((id, repo, _)) = matching_repo {
            if seen_repo_ids.insert(id) {
                git_repos.push(repo);
            }
        } else {
            non_git_paths.push(worktree_path.to_path_buf());
        }
    }

    (git_repos, non_git_paths)
}

/// Resolves a branch target into the ref the new worktree should be based on.
/// Returns `None` for `CurrentBranch`, meaning "use the current HEAD".
pub fn resolve_worktree_branch_target(branch_target: &NewWorktreeBranchTarget) -> Option<String> {
    match branch_target {
        NewWorktreeBranchTarget::CurrentBranch => None,
        NewWorktreeBranchTarget::ExistingBranch { name } => Some(name.clone()),
        NewWorktreeBranchTarget::RemoteBranch {
            remote_name,
            branch_name,
        } => Some(format!("{remote_name}/{branch_name}")),
    }
}
