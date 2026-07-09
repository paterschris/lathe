use git::status::GitSummary;
use gpui::{App, Hsla, transparent_black};
use theme::ActiveTheme;

pub(super) fn row_background(git_status: &GitSummary, cx: &App) -> Option<Hsla> {
    let tracked = git_status.index + git_status.worktree;
    let colors = cx.theme().colors();
    let background = if git_status.conflict > 0 {
        colors.lathe.panel_conflict_background
    } else if tracked.deleted > 0 {
        colors.lathe.panel_deleted_background
    } else if tracked.modified > 0 {
        colors.lathe.panel_modified_background
    } else if tracked.added > 0 || git_status.untracked > 0 {
        colors.lathe.panel_created_background
    } else {
        return None;
    };
    (background != transparent_black()).then_some(background)
}
