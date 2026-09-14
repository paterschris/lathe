# Upstream merge validation

Validated against `upstream/main` at `25b5569dd2` after merge commit `d861e7a5c0`.

## Passing checks

- `cargo check -p project`
- `cargo check -p workspace`

The following Lathe features remain connected to implementation and registration paths:

| Feature | Evidence |
| --- | --- |
| Mobile development | `mobile_dev` is registered from `zed`; the mobile panel and Pixel Tablet AVD profile remain present. |
| Pull request reviews | `pr_ui::PullRequestPanel` is loaded from `zed`; per-repository sections and settings remain present. |
| Code navigation | `lsp_locations::lsp_peek_view` remains registered, with picker and multi-buffer settings retained. |
| AWS profiles | The status selector, project environment overlay, and debug-adapter overlay remain present. |
| Awaiting-input indicator | Terminal settings, workspace item hooks, and title-bar rendering remain present. |
| Markdown Preview | Automatic Markdown preview settings and view handling are present. |
| Git history protocol | `GitFileHistory` and `GitChangeToCommit` protocol variants and `project::git_store::lathe` handlers remain present. |
| Git Flow and interactive rebase | The Git UI feature modules remain present. |

## Failures requiring repair

### Workspace groups, portable workspaces, theme customizer, and account binding

The merge selected upstream `workspace.rs`, which removed the module wiring for Lathe's `lathe`, `portable_workspace`, and `theme_customizer` modules. Their source files remain on disk but are not compiled. `MultiWorkspace` also lost `workspace_group_name` and its setter while `recent_projects::workspace_groups` still invokes both APIs.

This disconnects workspace-group operations, their collab-account binding, portable `.lathe-workspace` files, the customizer command, and workspace-level awaiting-input helpers. Reintegrate the Lathe workspace extension points into the current upstream workspace API before release.

### Editor integration

`cargo check -p recent_projects -p git_ui -p pr_ui -p mobile_dev -p aws_dev -p lsp_locations -p theme` reaches `editor` and fails because merged `editor/src/element.rs` calls `Editor::diff_hunk_delegate`, but the retained `editor/src/editor.rs` has neither the field nor the method.

This blocks all downstream UI crates and specifically endangers inline hunk staging.

### AI agent transport

The merged ACP transport does not expose the dedicated executor or the `AuthMethod::EnvVar` protocol variant assumed by Lathe's account integration. The compatibility edits make the crate compile, but the dedicated-stack guarantee and EnvVar authentication metadata require a deliberate upstream-compatible implementation and a runtime test.

## Release decision

Do not treat the merge as feature-complete or release-ready until the workspace and editor failures above are repaired and the full `cargo check -p zed` succeeds. The merge itself is committed; these compatibility and validation changes are currently in the working tree and have not been committed.
