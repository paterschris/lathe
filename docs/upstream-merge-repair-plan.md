# Upstream merge repair plan

Companion to `upstream-merge-validation.md`. That document recorded what the
merge broke; this one lists what is still outstanding after the repair commit
and splits the work into two tracks that can run in parallel.

State at time of writing:

- Merge commit `d861e7a5c0`, repair commit `0fc2861703` on branch
  `upstream-merge-repair`.
- `cargo check --workspace --all-targets` is clean.
- `./script/clippy` exits 0.
- Passing: `workspace` 294/294, `git_ui` 101/101, `gpui` 320/320,
  `pr_ui` 19/19, `outline_panel` 12/12, `agent_settings` 41/41.

## How to split this

Track A and Track B touch disjoint crates, so two agents can work at once
without conflicting. Assign one track per agent and do not cross over.

| Track | Owns these paths |
| --- | --- |
| A | `crates/agent_servers/`, `crates/editor/`, `crates/scheduler/` |
| B | `crates/worktree/`, `crates/project_panel/`, `crates/git_ui/`, `crates/lathe/` |
| B (task B3 only) | `crates/acp_tools/`, `crates/auto_update/`, `crates/git/`, `crates/git_hosting_providers/`, `crates/project/`, `crates/terminal/`, `crates/theme/`, `crates/title_bar/` |

B3 needs that second row because most Lathe modules are declared private
(`#[path = "x_lathe.rs"] mod lathe;`), so guarding them from another crate
means making them `pub`. None of those crates contain a Lathe module that
Track A touches, so the two tracks still do not collide. Verified: no
`*lathe*` module exists under `crates/editor/` or `crates/agent_servers/`.

Neither track should edit `assets/settings/default.json`,
`crates/settings_content/`, or `crates/workspace/`. If a task seems to need
one of those, stop and flag it rather than editing.

If a failure in your track traces into the other track's crates, stop and hand
it over rather than reaching across. Both tracks may append findings to this
file, but each should add its own section rather than editing the other's.

---

## Track A

### A1. ACP transport runs on the wrong executor (crash risk)

**Severity: high. This crashes dev builds at runtime, and nothing in CI catches
it because it compiles and lints clean.**

`crates/agent_servers/src/acp.rs:1037` polls the ACP connection future with
`cx.background_spawn(...)`. The comment directly above it, at lines 1024-1033,
says that is exactly what must not happen:

> The future must be polled on a dedicated thread rather than via
> `background_spawn`: in unoptimized builds its dispatch chain needs ~0.5 MiB
> of stack per inbound message, which overflows the fixed 512 KiB stacks of the
> GCD workers that poll background tasks on macOS, crashing dev builds as soon
> as an agent sends its first message.

The merge kept the comment and replaced the call. The intended API still
exists: `BackgroundExecutor::spawn_dedicated` at
`crates/scheduler/src/executor.rs:304`.

- Fix: move the `connection_future` poll onto `spawn_dedicated`, preserving the
  existing error logging and the returned task handle.
- Verify: `cargo check -p agent_servers`, then run an AI agent in a dev build
  on macOS and confirm it survives the first inbound message. A compile check
  alone does not validate this task.

### A2. The `editor` test suite has never run

1016 tests in `cargo test -p editor --lib`. The build was killed three times by
the OS for memory, including once at `-j 2`, and hung once at 0% CPU. It is the
only major crate with no test signal, and it received real changes in the repair
commit.

- Run it on a machine with enough memory, or reduce parallelism further
  (`-j 1`, and `-- --test-threads=2`).
- Triage whatever fails. Changes made to `editor` in `0fc2861703` were: the
  `diff_hunk_delegate` to `diff_hunk_renderer` rename, `BlameEntry.boundary`,
  `window.blur(cx)`, and the three re-integrations in A3.
- If a failure predates the repair commit, say so rather than fixing it blind:
  check it against `d861e7a5c0`.

### A3. Runtime verification of three re-integrated editor features

These were dead code after the merge, re-integrated in `0fc2861703`. They
compile and lint clean, but none has been exercised at runtime. Cursor
animation is the highest risk because its ~60 lines were interleaved into a
render path that had diverged from upstream.

| Feature | Check |
| --- | --- |
| Cursor animation | Set `"cursor_animation": { "enabled": true }`, move the cursor, confirm it animates and does not flicker or ghost. Confirm `"enabled": false` and reduce-motion both fully disable it. |
| Emmet | Open an HTML or JSX buffer with the Emmet language server running, confirm the wrap-with-abbreviation action is registered. |
| `frozen_scroll_range` | Run a project search that holds results, confirm the scrollbar and minimap do not jitter while rewrapping settles. |

---

## Track B

### B1. `worktree`: two failing tests

`cargo test -p worktree`

- `test_root_repo_common_dir` (`worktree_tests.rs:4611`): after `.git` is
  removed, `root_repo_common_dir()` should return `None` but still returns
  `Some("/main_repo/.git")`.
- `test_open_gitignored_files` (`worktree_tests.rs:1840`): count assertion.

Both test files are unmodified and both tests exist upstream, so our
`worktree.rs` is missing behavior the tests expect rather than the tests being
wrong. Do not change the assertions without first establishing that upstream
behaves differently.

Note that `0fc2861703` ported `ROOT_PATH_CHECK_INTERVAL`, the periodic root
check, and extracted `report_root_moved_or_deleted` into this file. Check
whether that port is complete before looking elsewhere.

### B2. `project_panel`: file history opens nothing

`cargo test -p project_panel test_file_history_action_uses_focused_project_panel_selection`

With the project panel focused and `tracked1.txt` selected, dispatching
`git::FileHistory` should open one `FileHistoryView`. It opens zero.

The whole chain exists and reads correctly, which is why this needs runtime
tracing rather than more code reading:

1. `project_panel.rs:584` registers a workspace handler calling
   `project_panel_file_history::open`.
2. `project_panel_file_history.rs` resolves the panel selection to a
   `ProjectPath`, then calls `git_ui_core::open_file_history`.
3. `git_ui_core::open_file_history` (`git_ui_core.rs:103`) dispatches through a
   global opener installed by `git_ui::init` at `git_ui.rs:97`.

One thing worth checking first: `git_ui.rs:452` registers a second workspace
handler for the same `git::FileHistory` action that only looks at the active
editor. Two handlers for one action on one entity may be resolving in an order
that makes neither fire.

### B3. Extend the module tripwire

`crates/lathe/src/module_tripwire.rs` converts a silently dropped Lathe module
into a build failure. It currently guards three things, all in `workspace`:
`lathe.rs` (via its `Workspace` methods), `portable_workspace.rs`, and
`theme_customizer.rs`.

Separately there are sixteen `*lathe*`-named modules inside upstream-owned
crates. All sixteen are wired right now (audited), but only one of them
(`workspace/src/lathe.rs`) is guarded, leaving fifteen unguarded.

Unguarded, by crate: `acp_tools`, `auto_update`, `git`, `git_hosting_providers`
(three provider files), `git_ui`, `project`, `terminal`, `theme` (two files),
`title_bar` (four files).

Two cautions:

- Most of these use `#[path = "x_lathe.rs"] mod lathe;`, so the module is named
  `lathe`, not the filename. An audit grepping for `mod <filename>;` reports
  false positives. Guard the reachable public path, not the file.
- A guard is only useful if the guarding crate is built. `crates/lathe` is
  pulled in by `crates/zed`, which is why it works.

### B4. Smoke-check the restored features

`0fc2861703` restored these from a state where they compiled but never ran.
None has been confirmed working. Each is a manual check.

- Theme Customizer: command palette, `theme customizer: open theme customizer`.
- Save Workspace and Save Workspace As, including reopening a
  `.lathe-workspace` file.
- Workspace groups: confirm the group name persists across a restart. It was
  being serialized as a hardcoded `None`.
- Collab account binding: confirm opening a bound group switches accounts.
- Status bar: mobile device selector and AWS profile selector appear.

---

## Out of scope

Do not attempt these as part of this plan:

- `assets/images/lathe_logo.svg` is deleted in `0fc2861703`. That looks like a
  deliberate SVG to PNG swap, since `lathe_logo.png` is what the code
  references. Confirm with the repo owner rather than reverting.
- The three pre-existing failures are the only known test failures. Do not
  chase clippy or compile errors; both gates are green as of `0fc2861703`.
