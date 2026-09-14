# r/ZedEditor feature-request scan

Scanned 2026-09-14. Counts are the number of distinct public r/ZedEditor
submission threads in the reviewed sample that request the capability. They are
lower bounds, not Reddit-wide vote totals or comment counts. A request mentioned
only in comments is called out separately and is not added to the count.

## Ranked requests

| Requests | Feature | Lathe opportunity |
| ---: | --- | --- |
| 4 | Extension API that can add editor UI, commands, and integrations | Large platform project; do not start before defining a narrow first extension surface. |
| 3 | Editable Jupyter and `.ipynb` notebooks with useful output rendering | High-value, large project. A WebView-backed notebook renderer is a plausible first increment. |
| 3 | Markdown authoring and preview: readable rendered output, frontmatter, Mermaid, and inline editing | Strong fit for a fork. Start with frontmatter/Mermaid rendering and side-by-side scroll synchronization. |
| 3 | Broader Git workflow | Split the work into the concrete requests below rather than a generic Git rewrite. |
| 2 | Staged and unstaged file trees and diffs | Valuable for AI-assisted review workflows. Check the current Git panel before planning: upstream may have since shipped part of it. |
| 2 | Terminal-session safety and restoration | Confirm before closing a running terminal; restore terminal-thread agent sessions after restart. |
| 2 | Navigation for large codebases | A compact, fuzzy-filterable picker for definitions/references/implementations and a call-hierarchy view. |
| 2 | Cross-platform project/workspace tabs | Tabbed project windows and saved multi-root workspaces. |
| 1 | WebView support | Enables notebooks, documentation preview, database tools, and richer extensions. It has unusually strong community response. |
| 1 | Built-in resource monitor | Show CPU, memory, PID, uptime, and controls for Zed-owned LSP, terminal, agent, and Git processes. |
| 1 | Code bookmarks | Already substantially present in Lathe; treat the Reddit request as a discoverability and workflow-verification item. |
| 1 | Clear Git synchronization state | Persistent indication that a push failed or a pull/rebase is needed. |
| 1 | Automatic commit-and-push on save | Keep a Markdown workspace continuously versioned; likely safest as an opt-in task/setting with a debounce. |
| 1 | Disable new AI popups and suggestions by default | A conservative “no unsolicited AI UI” preference. |

## Evidence

### Extension API — 4 threads

- [What are your most anticipated Zed features?](https://www.reddit.com/r/ZedEditor/comments/1ft5x37/what_are_your_most_anticipated_zed_features/) requests extension APIs alongside tasks, debugging, and Git.
- [Zed should be more extensible](https://www.reddit.com/r/ZedEditor/comments/1l5fu6h/zed_should_be_more_extensible/) asks that the editor expose more functionality to extensions.
- [We need extensions](https://www.reddit.com/r/ZedEditor/comments/1raoikx/we_need_extensions/) asks for UI-capable extensions and names TabOut, one-click code running, and bookmarks.
- [Zed Roadmap and Extension API](https://www.reddit.com/r/ZedEditor/comments/1s2bzpc/zed_roadmap_and_extension_api/) argues that a richer API would let the community supply missing editor functionality.

### Notebooks — 3 threads

- [When is Zed bringing notebooks support?](https://www.reddit.com/r/ZedEditor/comments/1tlnfzc/when_is_zed_bringing_notebooks_support/)
- [Jupyter?](https://www.reddit.com/r/ZedEditor/comments/1u85cz7/jupyter/)
- [Guys when are we gonna get ipynb support](https://www.reddit.com/r/ZedEditor/comments/1urzxej/guys_when_are_we_gonna_get_ipynb_support/)

The threads consistently ask for editable notebooks and usable rendered output, not JSON viewing or a read-only preview.

### Markdown authoring and preview — 3 threads

- [Preview Markdown](https://www.reddit.com/r/ZedEditor/comments/1d4rj9d/preview_markdown/)
- [Markdown preview is kinda terrible, no?](https://www.reddit.com/r/ZedEditor/comments/1tzc3ji/markdown_preview_is_kinda_terrible_no/)
- [I improved the Markdown Previewer in Zed](https://www.reddit.com/r/ZedEditor/comments/1up9ksl/i_improved_the_markdown_previewer_in_zed/)

The repeated details are frontmatter, Mermaid, readable theming, synchronized preview scrolling, and inline/Typora-like editing. A further request for an in-window Typora-like view appears in the comments of [Just switched all my IDEs and editors to Zed](https://www.reddit.com/r/ZedEditor/comments/1vx6qe6/just_switched_all_my_ides_and_editors_to_zed/).

### Git workflow — 3 threads

- [Feature Request: Proper Staged/Unstaged File Tracking in Zed](https://www.reddit.com/r/ZedEditor/comments/1rtavff/feature_request_proper_stagedunstaged_file/)
- [Is there a way to stage changes and see a staged vs. unstaged diff in Zed?](https://www.reddit.com/r/ZedEditor/comments/1tsvrqj/is_there_a_way_to_stage_changes_and_see_a_staged/)
- [Better view of the Git state](https://www.reddit.com/r/ZedEditor/comments/1vxuopi/better_view_of_the_git_state/)

The first two are the same concrete request and account for the count of two above. The third asks for durable push/pull failure and synchronization state, so it is tracked separately in the ranked list.

### Terminal safety and persistence — 2 threads

- [TIL I can just fork Zed and do whatever I want](https://www.reddit.com/r/ZedEditor/comments/1s0l6if/til_i_can_just_fork_zed_and_do_whatever_i_want/) requests confirmation before a terminal tab is closed while work is running.
- [Is it possible to auto resume agent tui when restart in terminal thread](https://www.reddit.com/r/ZedEditor/comments/1vyterv/is_it_possible_to_auto_resume_agent_tui_when/) requests restoring the prior agent command/session after reopening a project.

### Navigation and workspaces — 4 threads across two grouped requests

- [Feature request: compact picker view for LSP navigation](https://www.reddit.com/r/ZedEditor/comments/1t82ku7/feature_request_compact_picker_view_for_lsp/) proposes fuzzy-filtered one-line definition, reference, and implementation results.
- [Focusing too much on AI](https://www.reddit.com/r/ZedEditor/comments/1tvwz0h/focusing_too_much_on_ai/) calls out missing call hierarchy as a blocking editor feature.
- [Feature Request: Cross-platform window tabs for Windows & Linux](https://www.reddit.com/r/ZedEditor/comments/1q4ou9t/feature_request_crossplatform_window_tabs_for/)
- [New to Zed, why so hard to find basic features?](https://www.reddit.com/r/ZedEditor/comments/1s28maj/new_to_zed_why_so_hard_to_find_basic_features/) asks for saved multi-root workspaces.

### One-thread requests worth considering

- [Zed With Webview](https://www.reddit.com/r/ZedEditor/comments/1vtf4bq/zed_with_webview/) demonstrates a fork and drew a large response. It explicitly connects WebViews to notebooks and other rich integrations.
- [Feature request: a built-in resource monitor for Zed](https://www.reddit.com/r/ZedEditor/comments/1txi1ef/feature_request_a_builtin_resource_monitor_for/) describes a Zed-aware process monitor.
- [Quick review and a suggestion from a PyCharm user](https://www.reddit.com/r/ZedEditor/comments/1q827c3/quick_review_and_a_suggestion_from_a_pycharm_user/) requests code bookmarks.
- [automatic git commit on save](https://www.reddit.com/r/ZedEditor/comments/1vz6yku/automatic_git_commit_on_save/) asks for opt-in real-time commit/push for a Markdown folder.
- [Is zed doomed to seek dark patterns?](https://www.reddit.com/r/ZedEditor/comments/1reezmu/is_zed_doomed_to_seek_dark_patterns/) asks for a preference that pre-emptively disables new AI suggestions and popups.

## Suggested first Lathe backlog

1. Improve Markdown preview and authoring. It is requested repeatedly, has a clear scope, and fits Lathe's existing editor surface.
2. Add terminal close confirmation and session restoration. These are discrete, low-risk workflow improvements.
3. Add a Git synchronization status indicator. It addresses a concrete failure mode without duplicating the whole Git client.
4. Add a compact LSP navigation picker. It is a bounded editor feature with clear UI behavior.
5. Investigate a WebView foundation before taking on notebooks. The same foundation can support notebooks, richer Markdown, and future integrations.

Do not use the counts as a priority score by themselves. They measure repeated explicit requests in a constrained public sample, while implementation cost, existing upstream support, and Lathe's mobile-development focus should determine the final order.

## Local capability check

Lathe already implements the core bookmark workflow in `crates/editor`: labeled
bookmarks, gutter markers, next/previous navigation, and a project-wide
bookmark view. It should therefore be verified and made discoverable before
being considered new feature work.

Lathe also has Jupyter settings and REPL support. The notebook requests should
be scoped to editable `.ipynb` documents and robust rendered outputs, rather
than treated as a request to add Python cell execution from scratch.
