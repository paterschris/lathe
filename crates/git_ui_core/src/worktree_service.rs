mod creation;
mod opening;
mod selection;

use std::path::PathBuf;

use anyhow::anyhow;
use gpui::{SharedString, Task, TaskExt};
use util::ResultExt as _;
use workspace::{MultiWorkspace, Workspace, dock::DockPosition};

pub use creation::await_and_rollback_on_failure;

use creation::{create_worktree_askpass_delegate, do_create_worktree, remote_branch_to_fetch};
use opening::do_switch_worktree;

use crate::notifications::show_error_toast;

pub use selection::{
    RemoteBranchName, WorktreeCreateTarget, classify_worktrees, resolve_worktree_branch_target,
    worktree_create_targets,
};

pub struct CreatedWorktreeWorkspace {
    pub workspace: gpui::Entity<Workspace>,
    pub consolidated_worktrees: bool,
}

pub fn create_worktree_workspace(
    workspace: &mut Workspace,
    action: &zed_actions::CreateWorktree,
    window: &mut gpui::Window,
    fallback_focused_dock: Option<DockPosition>,
    cx: &mut gpui::Context<Workspace>,
) -> Task<anyhow::Result<CreatedWorktreeWorkspace>> {
    let project = workspace.project().clone();
    if project.read(cx).repositories(cx).is_empty() {
        return Task::ready(Err(anyhow!(
            "create_worktree: no git repository in the project"
        )));
    }
    if project.read(cx).is_via_collab() {
        return Task::ready(Err(anyhow!(
            "create_worktree: not supported in collab projects"
        )));
    }
    if workspace.active_worktree_creation().label.is_some() {
        return Task::ready(Err(anyhow!("a worktree operation is already in progress")));
    }

    let previous_state =
        workspace.capture_state_for_worktree_switch(window, fallback_focused_dock, cx);
    let workspace_handle = workspace.weak_handle();
    let window_handle = window.window_handle().downcast::<MultiWorkspace>();
    let remote_connection_options = project.read(cx).remote_connection_options(cx);
    let (git_repos, non_git_paths) = classify_worktrees(project.read(cx), cx);
    if git_repos.is_empty() {
        return Task::ready(Err(anyhow!("No git repositories found in the project")));
    }
    if remote_connection_options.is_some()
        && project
            .read(cx)
            .remote_client()
            .is_some_and(|client| client.read(cx).is_disconnected())
    {
        return Task::ready(Err(anyhow!(
            "Cannot create worktree: remote connection is not active"
        )));
    }

    let worktree_name = action.worktree_name.clone();
    let branch_target = action.branch_target.clone();
    let display_name: SharedString = worktree_name
        .as_deref()
        .unwrap_or("worktree")
        .to_string()
        .into();
    workspace.set_active_worktree_creation(Some(display_name), false, cx);

    let fetch_askpass_delegates = if remote_branch_to_fetch(&branch_target).is_some() {
        let mut delegates = Vec::with_capacity(git_repos.len());
        for _ in &git_repos {
            delegates.push(create_worktree_askpass_delegate(
                workspace_handle.clone(),
                "git fetch",
                window,
                cx,
            ));
        }
        delegates
    } else {
        Vec::new()
    };

    cx.spawn_in(window, async move |_workspace_entity, mut cx| {
        let result = do_create_worktree(
            git_repos,
            non_git_paths,
            worktree_name,
            branch_target,
            previous_state,
            workspace_handle.clone(),
            window_handle,
            remote_connection_options,
            fetch_askpass_delegates,
            false,
            &mut cx,
        )
        .await;

        if result.is_err() {
            workspace_handle
                .update(cx, |workspace, cx| {
                    workspace.set_active_worktree_creation(None, false, cx);
                })
                .log_err();
        }
        result
    })
}

pub fn handle_create_worktree(
    workspace: &mut Workspace,
    action: &zed_actions::CreateWorktree,
    window: &mut gpui::Window,
    fallback_focused_dock: Option<DockPosition>,
    cx: &mut gpui::Context<Workspace>,
) {
    let project = workspace.project().clone();

    if project.read(cx).repositories(cx).is_empty() {
        log::error!("create_worktree: no git repository in the project");
        return;
    }
    if project.read(cx).is_via_collab() {
        log::error!("create_worktree: not supported in collab projects");
        return;
    }

    // Guard against concurrent creation
    if workspace.active_worktree_creation().label.is_some() {
        return;
    }

    let previous_state =
        workspace.capture_state_for_worktree_switch(window, fallback_focused_dock, cx);
    let workspace_handle = workspace.weak_handle();
    let window_handle = window.window_handle().downcast::<MultiWorkspace>();
    let remote_connection_options = project.read(cx).remote_connection_options(cx);

    let (git_repos, non_git_paths) = classify_worktrees(project.read(cx), cx);

    if git_repos.is_empty() {
        show_error_toast(
            cx.entity(),
            "worktree create",
            anyhow!("No git repositories found in the project"),
            cx,
        );
        return;
    }

    if remote_connection_options.is_some() {
        let is_disconnected = project
            .read(cx)
            .remote_client()
            .is_some_and(|client| client.read(cx).is_disconnected());
        if is_disconnected {
            show_error_toast(
                cx.entity(),
                "worktree create",
                anyhow!("Cannot create worktree: remote connection is not active"),
                cx,
            );
            return;
        }
    }

    let worktree_name = action.worktree_name.clone();
    let branch_target = action.branch_target.clone();
    let display_name: SharedString = worktree_name
        .as_deref()
        .unwrap_or("worktree")
        .to_string()
        .into();

    workspace.set_active_worktree_creation(Some(display_name), false, cx);

    // Build one askpass delegate per git repo when the worktree base is a remote
    // branch, so do_create_worktree (which runs in an AsyncWindowContext without a
    // Window) can fetch the remote first. Non-remote targets pass an empty vec and
    // never fetch.
    let fetch_askpass_delegates = if remote_branch_to_fetch(&branch_target).is_some() {
        let mut delegates = Vec::with_capacity(git_repos.len());
        for _ in &git_repos {
            delegates.push(create_worktree_askpass_delegate(
                workspace_handle.clone(),
                "git fetch",
                window,
                cx,
            ));
        }
        delegates
    } else {
        Vec::new()
    };

    cx.spawn_in(window, async move |_workspace_entity, mut cx| {
        let result = do_create_worktree(
            git_repos,
            non_git_paths,
            worktree_name,
            branch_target,
            previous_state,
            workspace_handle.clone(),
            window_handle,
            remote_connection_options,
            fetch_askpass_delegates,
            true,
            &mut cx,
        )
        .await;

        if let Err(err) = &result {
            log::error!("Failed to create worktree: {err}");
            workspace_handle
                .update(cx, |workspace, cx| {
                    workspace.set_active_worktree_creation(None, false, cx);
                    show_error_toast(cx.entity(), "worktree create", anyhow!("{err:#}"), cx);
                })
                .ok();
        }

        result
    })
    .detach_and_log_err(cx);
}

/// The open folders a worktree switch keeps as they are: everything outside the
/// repository being switched.
fn paths_kept_by_worktree_switch(
    visible_paths: &[PathBuf],
    switched_work_dir: &std::path::Path,
) -> Vec<PathBuf> {
    visible_paths
        .iter()
        .filter(|path| !path.starts_with(switched_work_dir))
        .cloned()
        .collect()
}

pub fn handle_switch_worktree(
    workspace: &mut Workspace,
    action: &zed_actions::SwitchWorktree,
    window: &mut gpui::Window,
    fallback_focused_dock: Option<DockPosition>,
    cx: &mut gpui::Context<Workspace>,
) {
    let project = workspace.project().clone();

    if project.read(cx).repositories(cx).is_empty() {
        log::error!("switch_to_worktree: no git repository in the project");
        return;
    }
    if project.read(cx).is_via_collab() {
        log::error!("switch_to_worktree: not supported in collab projects");
        return;
    }

    // Guard against concurrent creation
    if workspace.active_worktree_creation().label.is_some() {
        return;
    }

    let previous_state =
        workspace.capture_state_for_worktree_switch(window, fallback_focused_dock, cx);
    let workspace_handle = workspace.weak_handle();
    let window_handle = window.window_handle().downcast::<MultiWorkspace>();
    let remote_connection_options = project.read(cx).remote_connection_options(cx);

    // The picker lists worktrees of the active repository only, so that is the only
    // folder the switch replaces. Switching used to remap every repository in the
    // workspace onto the chosen worktree, which dropped the others. The path list then
    // no longer matched the original workspace on the way back, so a fresh workspace
    // was created and the original one's tabs and terminals were never restored.
    let Some(active_repository) = project.read(cx).active_repository(cx) else {
        log::error!("switch_to_worktree: no active repository");
        return;
    };
    let active_work_dir = active_repository
        .read(cx)
        .work_directory_abs_path
        .to_path_buf();
    let visible_paths: Vec<PathBuf> = project
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .collect();
    let kept_paths = paths_kept_by_worktree_switch(&visible_paths, &active_work_dir);

    let display_name: SharedString = action.display_name.clone().into();

    workspace.set_active_worktree_creation(Some(display_name), true, cx);

    let worktree_path = action.path.clone();

    cx.spawn_in(window, async move |_workspace_entity, mut cx| {
        let result = do_switch_worktree(
            worktree_path,
            vec![active_work_dir],
            kept_paths,
            previous_state,
            workspace_handle.clone(),
            window_handle,
            remote_connection_options,
            &mut cx,
        )
        .await;

        if let Err(err) = &result {
            log::error!("Failed to switch worktree: {err}");
            workspace_handle
                .update(cx, |workspace, cx| {
                    workspace.set_active_worktree_creation(None, false, cx);
                    show_error_toast(cx.entity(), "worktree switch", anyhow!("{err:#}"), cx);
                })
                .ok();
        }

        result
    })
    .detach_and_log_err(cx);
}

#[cfg(test)]
mod tests {
    use super::paths_kept_by_worktree_switch;
    use std::path::{Path, PathBuf};

    fn paths(items: &[&str]) -> Vec<PathBuf> {
        items.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn test_switching_one_repo_keeps_the_other_repos() {
        let visible = paths(&["/code/frontend", "/code/backend", "/code/notes"]);
        assert_eq!(
            paths_kept_by_worktree_switch(&visible, Path::new("/code/frontend")),
            paths(&["/code/backend", "/code/notes"]),
        );
    }

    #[test]
    fn test_folders_inside_the_switched_repo_are_replaced() {
        let visible = paths(&["/code/frontend/app", "/code/frontend/lib", "/code/backend"]);
        assert_eq!(
            paths_kept_by_worktree_switch(&visible, Path::new("/code/frontend")),
            paths(&["/code/backend"]),
        );
    }

    #[test]
    fn test_sibling_with_a_shared_name_prefix_is_kept() {
        // `starts_with` on a `Path` compares components, so `frontend-old` is not
        // mistaken for a folder inside `frontend`.
        let visible = paths(&["/code/frontend", "/code/frontend-old"]);
        assert_eq!(
            paths_kept_by_worktree_switch(&visible, Path::new("/code/frontend")),
            paths(&["/code/frontend-old"]),
        );
    }
}
