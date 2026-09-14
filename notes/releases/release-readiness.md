# Release readiness review: proposed next beta

Written 2026-09-10 by the Track B agent (Claude), for Track A (Codex) to review before anything is committed, pushed, or tagged.

**Nothing has been committed, pushed, or released.** The request was to commit all in-flight work, push it, and cut the next beta. I stopped short of doing that for the reasons below, all of which are in Track A's area rather than mine. Track B's own work is finished and is not what is holding this up.

## What I verified

| Check | Result |
|---|---|
| `cargo check --workspace --all-targets` | Passes. The earlier `pr_ui` break is fixed. |
| `cargo test -p aws_dev` | 17 passed |
| `cargo test -p mobile_dev` | 62 passed |
| `cargo test -p lsp_locations` | 174 passed |
| `cargo test -p pr_ui`, `-p git_hosting_providers` | Passed |
| `cargo test -p language` | 157 passed |
| `cargo clippy` on every crate I touched | Clean |

47 files are currently modified across both tracks.

> **Update, 2026-09-10:** Resolved below. `BufferSnapshot::runnable_ranges` was still using the pre-grouped implementation duplicated in `buffer.rs`, bypassing the grouped resolver in `runnable.rs`. It now delegates to that implementation; `cargo test -p language --lib` passes all 157 tests.

## Blocker: 8 failing tests in `language::runnable`

```
runnable::tests::test_grouped_match_emits_one_runnable_per_run_item
runnable::tests::test_grouped_match_local_extras_are_per_group
runnable::tests::test_grouped_match_offset_range_filters_groups
runnable::tests::test_grouped_match_resolver_returning_none_skips_group
runnable::tests::test_grouped_match_shared_captures_propagate
runnable::tests::test_grouped_match_without_resolver_emits_nothing
runnable::tests::test_grouped_match_zero_width_offset_at_group_start
runnable::tests::test_local_extras_override_shared_extras_with_same_key
```

Representative failure, at `runnable.rs:640`:

```
assertion `left == right` failed
  left:  ["gamma"]
 right:  ["alpha", "beta", "gamma"]
```

A grouped tree-sitter match that should emit one runnable per `@run_item` is emitting only the last one.

### These are not yours, and not mine

Established rather than assumed:

- Stashing your `syntax_map.rs` and `syntax_map_tests.rs` back to HEAD and re-running leaves the same 8 failures. Nothing `language` depends on is in-flight.
- `runnable.rs` has since picked up debug `eprintln!`s in the working tree, which I take to be yours. They are the only working-tree changes to it and do not affect behavior.

So they are pre-existing on `main`, and `v1.0.0` was cut from a tree carrying them.

### Root cause, established by bisect

`runnable.rs` at HEAD is **byte-identical to upstream** apart from an added `#![allow(dead_code)]`. Running the same tests in throwaway worktrees:

| Tree | Result |
|---|---|
| `upstream/main` (72c53bf0aa) | 11 passed |
| merge base `b20c469479` | 11 passed |
| fork `HEAD` | 8 failed |

Passing at the merge base rules out upstream. This is a **fork-local regression**.

The culprit is `crates/language/src/buffer.rs`, which the fork changed by 217 lines since the merge base. At the merge base it delegated:

```rust
runnable::runnable_ranges(self, offset_range)
```

At HEAD that call is gone, replaced by an inline reimplementation in `buffer.rs` that builds `extra_captures` itself and emits one runnable per match, with no grouping. The tests exercise exactly this path: `collect_runnables` builds a real `Buffer` and calls `buffer.snapshot().runnable_ranges(range)` (`runnable.rs:432`).

So upstream's `group_runnable_matches` is still in the tree, still correct, and now **unreachable**. That is what the `#![allow(dead_code)]` at the top of `runnable.rs` is silencing: someone hit the dead-code warnings the drop produced and muted them rather than noticing a feature had gone missing.

### The fix

Restore `buffer.rs` to call `runnable::runnable_ranges`, delete the inline reimplementation, and drop the `#![allow(dead_code)]`. No upstream pull required, and the grouped-runnable code itself needs no changes.

### This is the third instance of one pattern

Every fork commit touching `crates/language` since the merge base is a merge commit, one of them labeled "fork-first resolution." Three features have now been silently dropped by conflict resolutions that kept the fork's side:

1. Rainbow parameters, predicates dropped by `f1b6ffa675` (Track A item 1).
2. Grouped runnables, dropped from `buffer.rs` (this one).
3. Worth auditing for others.

In each case the code compiled and nothing failed loudly; two were masked by a warning suppression or a missing theme key. A post-merge check that greps for newly added `allow(dead_code)`, and for upstream functions that lost their only caller, would have caught both.

### Why this is probably familiar

`git log -S` puts the tests' origin at upstream `6396a9b4d3` ("language: Support emitting multiple runnables from a single tree-sitter match", #57276). The last commit to touch `runnable.rs` is `f470d6f502` ("Merge upstream zed at 3b79b56201 (v1.13-pre, 273 commits)").

That is the same shape as the rainbow parameters regression you are fixing: a large upstream merge took upstream's version of a file and left the fork holding one half of a contract. Worth checking whether `f470d6f502` dropped the grouped-runnable implementation while keeping its tests, in which case the fix may be recoverable from the merge's first parent the same way the predicates were.

**Raising it with you because it is in `language`, the crate you are already inside.** Not asking you to own it, but a second person editing that crate during a release cut would be worse.

## Your work, as I observe it from outside

I have not reviewed your diffs, only checked that the tree builds and what files moved. Please confirm each item is complete and releasable, or say what is still open.

**Item 1, rainbow parameters.** Both halves of the trap now look closed: `satisfies_custom_predicates` has the full dispatch set restored with `text: &Rope` threaded back through, and `one.json` defines 16 `variable.parameter.chain_*` keys. That is exactly the combination that was dangerous while only half-present, so this is the item I would most want confirmation on. Has it been looked at visually, in a real file, in both light and dark? A predicate that silently returns the wrong parity would now be visible rather than inert, which is the point, but it also means a mistake ships as a very obvious highlighting bug.

**Item 2, pull request panel.** `create_pull_request_modal.rs`, `pull_request_panel.rs`, and six files under `git_hosting_providers` are modified. The backlog's acceptance criterion was that write paths get live-host verification or fixture tests, and that the branch fields become pickers. Which of those landed? Anything not exercised against a live host should say so in the release notes rather than be implied as working, since that has been the standing known issue for four releases.

**Item 3, peek view.** `lsp_peek_view.rs`, `lsp_locations/Cargo.toml`, and `assets/keymaps/vim.json` are modified, and the crate's 174 tests pass. Is the z-order fix in, and did the multibuffer and split cases get exercised?

## Track B, finished

- **Item 5, `aws_dev`:** 1 test to 17. Extracted `project_config_path` so the project-local precedence rule is testable; `ensure_v2_wrapper` now refuses empty profile names (they produced a malformed `credential_process` line that parsed back as a target of `"--format"`). One divergence from the AWS CLI is pinned rather than fixed: a repeated `[profile x]` section is dropped rather than merged.
- **Item 6, detached HEAD on remote:** wired through `proto::GitChangeToCommit` (`git.proto`, `zed.proto` envelope 482, `proto.rs`, `collab/rpc.rs`, `git_store.rs`, `lathe.rs`). Ships without a test: the fork has no remote-git harness, and `change_branch` has none either. My original backlog entry was wrong to call this a one-off; **16 sibling Lathe git operations still bail on remote projects**, and whether they follow is an open decision. Separately, `GitFileHistory` has no collab routing at all, which looks like an oversight.
- **Item 4, cross-platform:** four classes of Windows bug fixed in `mobile_dev` (Gradle wrapper, 16 SDK path lookups that failed *silently* because they are `is_file()` probes, the login shell, README hint prefixes). Linux was clean on all four. **No Windows runtime behavior has been observed**; `crates/util` typechecks under `--target x86_64-pc-windows-msvc`, but `mobile_dev` cannot cross-compile from macOS because tree-sitter needs MSVC headers. The zoom and peek features remain unexercised on both Linux and Windows, and SmartScreen is untouched.

## Version question

`crates/zed/Cargo.toml` is at `1.0.1`, `RELEASE_CHANNEL` is `beta`, and `RELEASE_NOTES.md` already documents a `v1.0.1-beta` covering the window zoom work only. **That version was never tagged** (`v0.236.35-beta` was not either; `v1.0.0` was).

Since `v1.0.1-beta` never shipped, my suggestion is to fold everything into it and rewrite its notes to cover all of it, rather than bumping to `1.0.2` and leaving a version that never existed. Open to the other call.

## Two things that should not go in the commit

- **`.rules` has an unrelated 63-line "context-mode MANDATORY routing rules" block.** It describes MCP tool routing and has nothing to do with Lathe. `.rules` is read by every agent session in this repo, so it affects both of us. It should be dropped from the release commit unless it was deliberate.
- **`README.md` carries the mandatory review banner** (`> [!IMPORTANT]` / "Remove this line to confirm you've reviewed this PR before submitting"). Per the repo's own rule, removing it is a manual step for the human author, not something either agent does. It will ship in the release unless Chris removes it first.

## What I need before proceeding

1. Your confirmation that items 1 through 3 are complete, or a list of what is still open.
2. Who takes the `buffer.rs` runnable fix. It is small and identified, and it sits in `crates/language` where you are already working. Say the word if you would rather I did it.
3. The version call.

On the question of whether an upstream sync would resolve any of this: no. We are 229 commits behind and 244 ahead, merge base `b20c469479`, and the runnable regression is ours rather than upstream's. A 229-commit merge landing on top of your in-flight `syntax_map.rs` rewrite is also the exact scenario that produced two of the three dropped features above, so it should be a deliberate separate exercise after this release, not part of it.

Once those are settled I can do the commit, push, and release in one pass. Draft release notes are ready and will be filled in from your answers.
