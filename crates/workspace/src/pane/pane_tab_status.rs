use crate::item::ItemSettings;
use gpui::{App, Hsla, WeakEntity, transparent_black};
use language::DiagnosticSeverity;
use project::{Project, ProjectPath};
use settings::Settings;
use ui::prelude::*;

enum GitTabStatus {
    Conflict,
    Deleted,
    Modified,
    Created,
}

pub(crate) fn text_color_override(
    project: &WeakEntity<Project>,
    project_path: Option<&ProjectPath>,
    cx: &App,
) -> Option<Color> {
    if !ItemSettings::get_global(cx).git_status {
        return None;
    }

    let colors = cx.theme().colors();
    let color = match git_tab_status(project, project_path?, cx)? {
        GitTabStatus::Conflict => colors.lathe.tab_conflict_foreground,
        GitTabStatus::Deleted => colors.lathe.tab_deleted_foreground,
        GitTabStatus::Modified => colors.lathe.tab_modified_foreground,
        GitTabStatus::Created => colors.lathe.tab_created_foreground,
    };
    Some(Color::Custom(color))
}

pub(crate) fn background_override(
    project: &WeakEntity<Project>,
    project_path: Option<&ProjectPath>,
    item_diagnostic: Option<&DiagnosticSeverity>,
    item_dirty: bool,
    cx: &App,
) -> Option<Hsla> {
    let colors = cx.theme().colors();

    let git_status_background = || {
        if !ItemSettings::get_global(cx).git_status {
            return None;
        }

        let color = if let Some(&DiagnosticSeverity::ERROR) = item_diagnostic {
            colors.lathe.tab_error_background
        } else if let Some(&DiagnosticSeverity::WARNING) = item_diagnostic {
            colors.lathe.tab_warning_background
        } else {
            match git_tab_status(project, project_path?, cx)? {
                GitTabStatus::Conflict => colors.lathe.tab_conflict_background,
                GitTabStatus::Deleted => colors.lathe.tab_deleted_background,
                GitTabStatus::Modified => colors.lathe.tab_modified_background,
                GitTabStatus::Created => colors.lathe.tab_created_background,
            }
        };

        non_transparent(color)
    };

    git_status_background().or_else(|| {
        if item_dirty {
            non_transparent(colors.lathe.tab_dirty_background)
        } else {
            None
        }
    })
}

fn git_tab_status(
    project: &WeakEntity<Project>,
    project_path: &ProjectPath,
    cx: &App,
) -> Option<GitTabStatus> {
    let project = project.upgrade()?;
    let project = project.read(cx);
    let (repo, repo_path) = project
        .git_store()
        .read(cx)
        .repository_and_path_for_project_path(project_path, cx)?;
    let summary = repo.read(cx).status_for_path(&repo_path)?.status.summary();
    let tracked = summary.index + summary.worktree;

    if summary.conflict > 0 {
        Some(GitTabStatus::Conflict)
    } else if tracked.deleted > 0 {
        Some(GitTabStatus::Deleted)
    } else if tracked.modified > 0 {
        Some(GitTabStatus::Modified)
    } else if tracked.added > 0 || summary.untracked > 0 {
        Some(GitTabStatus::Created)
    } else {
        None
    }
}

fn non_transparent(color: Hsla) -> Option<Hsla> {
    (color != transparent_black()).then_some(color)
}
