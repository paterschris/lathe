use super::ProjectPanel;
use editor::Editor;
use gpui::{Context, Window};
use project::ProjectPath;
use workspace::Workspace;

pub(super) fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if let Some(panel) = workspace.panel::<ProjectPanel>(cx) {
        let maybe_project_path = panel.read(cx).selection.and_then(|selection| {
            let project = workspace.project().read(cx);
            let worktree = project.worktree_for_id(selection.worktree_id, cx)?;
            let entry = worktree.read(cx).entry_for_id(selection.entry_id)?;
            if entry.is_file() {
                Some(ProjectPath {
                    worktree_id: selection.worktree_id,
                    path: entry.path.clone(),
                })
            } else {
                None
            }
        });

        if let Some(project_path) = maybe_project_path {
            open_for_project_path(workspace, project_path, window, cx);
            return;
        }
    }

    if let Some(active_item) = workspace.active_item(cx)
        && let Some(editor) = active_item.downcast::<Editor>()
        && let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton()
        && let Some(file) = buffer.read(cx).file()
    {
        let project_path = ProjectPath {
            worktree_id: file.worktree_id(cx),
            path: file.path().clone(),
        };
        open_for_project_path(workspace, project_path, window, cx);
    }
}

fn open_for_project_path(
    workspace: &mut Workspace,
    project_path: ProjectPath,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let git_store = workspace.project().read(cx).git_store().clone();
    if git_store
        .read(cx)
        .repository_and_path_for_project_path(&project_path, cx)
        .is_some()
    {
        git_ui_core::open_file_history(workspace, &project_path, window, cx);
    }
}
