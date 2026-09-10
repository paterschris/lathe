# Lathe Feature Backlog

Audit date: 2026-09-10. Baseline: `23af659239` (docs: Document the peek view and correct stale README claims), working tree dirty with the unreleased v1.0.1-beta zoom work.

Six areas of the fork are carrying known gaps. Sources are the "Known issues" sections in `RELEASE_NOTES.md`, the hedges still standing in `README.md`, and the state of the fork-only crates themselves.

## Assignment

| Track | Owner | Items |
|---|---|---|
| A | Codex | 1. Rainbow parameters, 2. Pull request panel, 3. Peek view |
| B | Claude | 4. Cross-platform coverage, 5. `aws_dev` test coverage, 6. Detached HEAD on remote projects |

Track A is the user-visible surface: one dead feature, one under-tested feature that performs destructive operations, and the rough edges on the v1.0.0 headline feature. Track B is coverage and portability work that is lower risk but has been deferred across several releases.

The two tracks touch disjoint crates. Track A works in `crates/language`, `crates/grammars`, `crates/theme*`, `crates/pr_ui`, and `crates/lsp_locations`. Track B works in `crates/mobile_dev`, `crates/aws_dev`, `crates/project`, `crates/git`, and the packaging scripts. Overlap should be limited to `README.md` and `RELEASE_NOTES.md`, so coordinate edits to those two files.

---

# Track A (Codex)

## 1. Rainbow parameters is inert and latently broken

**Status: done (2026-09-10).** Restored custom predicates and their rope-aware dispatch, added bundled `chain_1` through `chain_8` theme keys, and added parameter-predicate coverage.

**Priority: high.** Self-contained, fully documented, and currently a live trap.

The feature assigns each function parameter a color by its ordinal position and propagates that color to every reference in the function body. The highlight query patterns ship, but nothing drives them.

### Current state

| Component | State |
|---|---|
| `chain_N` patterns in `crates/grammars/src/*/highlights.scm` | Present |
| Custom tree-sitter predicates in `crates/language/src/syntax_map.rs` | Missing |
| `variable.parameter.chain_N` keys in a bundled theme | Missing |

Query patterns are live in seven grammars: `typescript` (24 `chain_1` hits), `javascript` (24), `tsx` (24), `python` (18), `rust` (16), `cpp` (14), `go` (14).

The predicates were dropped by `f1b6ffa675` ("Release v0.236.14 stable", 2026-06-04), a 382-commit upstream merge that took upstream's `syntax_map.rs` wholesale. The `.scm` files kept their half of the contract.

### The trap

`satisfies_custom_predicates` at `crates/language/src/syntax_map.rs:1448` treats an unrecognized predicate as satisfied:

```rust
let satisfied = match predicate.operator.as_ref() {
    "has-parent?" => has_parent(&predicate.args, mat),
    "not-has-parent?" => !has_parent(&predicate.args, mat),
    _ => true,
};
```

So all eight `chain_N` patterns currently match every parameter unconditionally. This is invisible only because no theme defines the `chain_N` keys, and `HighlightMap`'s longest-prefix matching folds every `chain_N` capture back to plain `variable.parameter`. **The moment anyone adds those keys through the theme customizer, every parameter in the file takes the last-matching pattern's color.** That is a visible regression waiting on an unrelated action.

### Work

1. Reinstate the predicate functions in `crates/language/src/syntax_map.rs`. Recover them verbatim from `git show f1b6ffa675^:crates/language/src/syntax_map.rs`, lines 1435-1730 in that revision: `resolve_capture`, `sibling_index_parity`, `sibling_index_mod`, `ancestor_count_parity`, `is_parameter_reference`, `is_parameter_reference_mod`, `function_parameter_index`, `find_parameter_identifier`.
2. Restore the dispatch arms in `satisfies_custom_predicates` and restore its `text: &Rope` parameter. The reference-site predicates need the rope to read identifier text; upstream's current signature omits it, so the argument has to be threaded back through every call site (start at `syntax_map.rs:1408`).
3. Define `variable.parameter.chain_1` through `chain_8` in the bundled theme. Until these keys exist the feature is a no-op even with working predicates.
4. Restore `#ancestor-count-is-even?` / `#ancestor-count-is-odd?` as well. They drive closure-pipe depth coloring rather than parameters, but the same commit broke them.

`docs/rainbow-parameters-spec.md` holds the full specification, including the two coloring schemes (8-color cycle for TS/TSX/JS, 2-color parity for Rust and Python).

### Done looks like

Parameters in a TypeScript function render in distinct colors by position, references in the body match their declaration, and the Rust and Python 2-color alternation works. Add predicate-level tests in `syntax_map.rs`, since the `.scm` side cannot be unit tested directly. Alternatively, if the feature is not wanted, delete the `chain_N` patterns from all seven grammars and this spec, which closes the trap just as effectively. Either outcome is acceptable, but leaving it as-is is not.

## 2. Pull request panel: large, thinly tested, write paths unproven

**Status: done (2026-09-10).** Recorded host-response fixtures cover every write path on GitHub, GitLab, and Bitbucket. New pull request source and target fields now select from local branches.

**Priority: high.** This is the area most likely to cause real damage.

6,731 lines across `crates/pr_ui/src`, with 14 tests. All 14 live in `pull_request_view.rs`. These files have zero:

- `pull_request_panel.rs`
- `create_pull_request_modal.rs`
- `connect_modal.rs`
- `pull_request_picker.rs`
- `reviewer_picker.rs`
- `pr_ui.rs`

The README still says "Still being polished."

### Standing known issues

Carried unchanged across four releases (v0.236.31-beta through v0.236.34-beta):

- Merge, squash and merge, draft transitions, reopen, close, and reviewer requests have **never been exercised against a live host** on GitHub or GitLab. Bitbucket decline and request-reviewers are the only live-tested write paths.
- The pull request overflow menu (added in v0.236.34-beta) has not been exercised against a live host at all.
- GitLab and GitHub Enterprise support has never been exercised against a live host.
- The New Pull Request dialog's branch fields are free text rather than pickers. Flagged in v0.236.32-beta and unchanged since.

These are irreversible operations against real repositories. A merge fired against a mis-parsed API response is not recoverable from the editor.

### Work

1. Test the write paths against live hosts, or build recorded-fixture tests over the host API responses if live testing is impractical. Cover at minimum: merge, squash and merge, draft to ready, ready to draft, close, reopen, request reviewers. Do this for GitHub, GitLab, and Bitbucket.
2. Replace the free-text branch fields in `create_pull_request_modal.rs` with branch pickers fed from the repository's branch list. This is a correctness fix as much as an ergonomic one: a typo in a base branch is currently accepted and sent to the host.
3. Add coverage to `pull_request_panel.rs` and the modals. The panel is the largest untested file in the crate.
4. Once the write paths are proven, decide whether the "Still being polished" note in `README.md` can come out.

### Done looks like

Every write path has either a live-host verification recorded in the release notes or a fixture test. The branch fields are pickers. The known-issues list in the next release shrinks rather than being copied forward again.

## 3. Peek view rough edges

**Status: done (2026-09-10).** Deferred drawing keeps hovers and context menus above the peek; multibuffer, split-pane, and Vim `j` coverage now exercise the previously untested cases.

**Priority: medium.** The v1.0.0 headline feature, shipped with three acknowledged gaps.

Code lives in `crates/lsp_locations/src/lsp_locations.rs` and `lsp_peek_view.rs` (2,502 lines, 20 tests).

### Known issues

1. **Z-order.** The peek paints above everything else in the editor. If it covers a hover popover or a context menu, that is why. Users hit this whenever a peek is open and they hover a symbol.
2. **Untested contexts.** Testing was on macOS in ordinary single-file editors. The peek inside a multibuffer (project search results, diffs) and inside a split is unexercised. Multibuffer is the more likely of the two to be broken, since the peek's line anchoring assumes a single buffer.
3. **Vim.** The `menu` key context fix in v1.0.0 is "reasoned from the keymap rather than confirmed by use." Nobody has actually pressed `j` in a peek under Vim mode.

### Work

1. Fix the z-order so hover popovers and context menus paint above the peek, or suppress them while a peek is open. Pick one and note the choice in the release notes.
2. Exercise the peek in a multibuffer and in a split. Add tests for whichever breaks.
3. Confirm the Vim behavior by hand, then either state it as tested in the release notes or fix what turns up.

### Done looks like

The three peek entries drop out of "Known issues" in the next release. Note that the Linux and Windows part of item 2 belongs to Track B item 4, not here.

---

# Track B (Claude)

## 4. Everything is verified on macOS only

**Status: partially done (2026-09-10).** The static portability audit is finished and four classes of Windows bug are fixed (see below). Actual Linux and Windows *runtime* verification remains blocked on hardware.

**Priority: medium.** Long-standing and broad rather than deep.

The README advertises macOS (Apple Silicon), Linux (x86_64 and arm64), and Windows (x86_64 and arm64, experimental). Verification does not match that claim:

- v1.0.1-beta (window zoom): "Tested on macOS. Linux and Windows are unexercised."
- v1.0.0 (peek view): "Tested on macOS, in ordinary single-file editors... on Linux and Windows is unexercised."
- Windows installers are signed, but SmartScreen still shows a reputation prompt. This has appeared in every release since v0.236.31-beta.

`crates/mobile_dev` has seven `cfg!(target_os = "macos")` branches (`mobile_dev.rs:494, 623, 765, 1498, 1918, 2176` and `device_picker.rs:130`). iOS being macOS-only is inherent, but the Android path off macOS has no verification at all, and `apple.rs` documents the split rather than testing it.

### Portability audit (done)

Auditing the Android path from macOS found it is not merely unverified on Windows, it cannot work there. Three classes of bug, all fixed:

1. **The Gradle wrapper.** `commands.rs` ran `./gradlew`, a Unix shell script. Windows ships `gradlew.bat`, and `cmd.exe` has no `./` convention. This is the whole one-click build and run flow. Now resolved per platform, by absolute path on Windows.
2. **The SDK command-line tools.** `sdkmanager` and `avdmanager` are `.bat` launchers on Windows, and `adb`, `java`, and `emulator` take `.exe`. Sixteen lookups across `toolchain.rs` and `emulator.rs` used the bare Unix names. Because every one is an `is_file()` probe, they failed *silently*: the panel would report the toolchain as not installed rather than erroring. Now routed through `tool_script` / `tool_binary` helpers in `toolchain.rs`.
3. **README scraping.** `mobile_project.rs` matched only the literal `./gradlew` prefix, so a Windows README documenting `gradlew.bat` or `.\gradlew.bat` yielded no run hints. Both spellings now match; `starts_with` is literal, so the Unix spelling did not cover them.

Linux came out clean on all four.

**How far these are verified.** Every fix uses `cfg!(...)`, which is a runtime boolean rather than conditional compilation, so the Windows branches are type-checked by the ordinary macOS build and are exercised by tests that assert both branches. `crates/util`, which supplies the Windows shell APIs, also typechecks under `--target x86_64-pc-windows-msvc`. `mobile_dev` itself cannot be cross-checked from macOS: tree-sitter's C build scripts need MSVC headers. So the code is known to compile and to be internally consistent; **no Windows runtime behavior has been observed**, and the `.bat` and `.exe` names rest on Android's published layout rather than on a machine.

4. **The login shell.** `spawn_terminal` read `$SHELL`, fell back to `/bin/zsh`, and passed `-lic`. On Windows there is no `$SHELL`, `/bin/zsh` does not exist, and `-lic` are POSIX flags, so every long-running action in the panel (Metro, builds, every scraped script) failed to spawn there regardless of the fixes above.

   Resolved by asking `util::shell::ShellKind` for the host shell's command flags, which is what the terminal panel already does, rather than hard-coding `cmd.exe` quoting in this crate. Unix keeps `-lic` deliberately: the toolchains the panel drives (node, adb, rbenv, the JDK) are usually only on `PATH` once the user's profile has been sourced, so the login shell is load-bearing. Windows resolves `PATH` from machine and user scope instead of a sourced profile, so it has nothing for a login shell to do.

   The unset-`$SHELL` fallback also moved from `/bin/zsh` to `/bin/sh`. `/bin/sh` exists on every macOS and Linux host; `/bin/zsh` is not guaranteed on Linux.

### Work

1. Exercise the window zoom on Linux and Windows. It touches `theme_settings` and `zed.rs`, both platform-independent in principle, so this is likely verification rather than fixing.
2. Exercise the peek view on Linux and Windows (the platform half of Track A item 3).
3. Exercise the Android path in `mobile_dev` on Linux and Windows: emulator creation, `adb` device listing, logcat, and `gradlew` install. The `cfg!` branches suggest these were written to work off macOS but never run there.
4. Windows SmartScreen reputation. Signed artifacts still prompt because the certificate has no accumulated reputation. Investigate whether the current signing setup can accrue it, or whether an EV certificate is the only path. If it is a wait-for-reputation situation, say so in the README and stop repeating it in every release's known issues.

### Done looks like

Each release's known-issues section names what was actually tested rather than defaulting to "macOS only." If a platform is genuinely unsupported for a feature, the README says so instead of implying parity.

## 5. `aws_dev` has one test

**Status: done (2026-09-10).** 1 test to 17. `project_config_path` was extracted from `profile_picker.rs` so the precedence rule is testable, and `ensure_v2_wrapper` now refuses empty profile names.

**Priority: medium.** Smallest amount of work in either track.

982 lines across `crates/aws_dev/src/aws_dev.rs` and `profile_picker.rs`, with exactly one `#[test]` (`aws_dev.rs:336`). This is the thinnest coverage of any fork-only crate, and the crate handles credentials and SSO session polling:

- Parsing global and project-local `.aws/config`, including the precedence rule where a project-local file takes over from the global one.
- SSO session status polling, which decides whether an expired login is surfaced before a command fails.
- Appending a `credential_process` wrapper profile to the user's config file. This writes to a file outside the project.
- Propagating `AWS_PROFILE` into every spawned terminal and task.

### Work (done)

16 tests added, covering config parsing (SSO markers, non-profile sections, sort order, duplicate sections, malformed input), `credential_process` target parsing, the project-local precedence rule, and the `ensure_v2_wrapper` append.

Two changes came out of writing them:

- `project_config_path` was extracted from the closure in `profile_picker.rs` into `aws_dev.rs`. The precedence rule was previously untestable because it lived inside a `background_spawn`.
- `ensure_v2_wrapper` now refuses an empty profile name. The character check passed `""` (vacuously true for `all`), which wrote `--profile  --format process`; the parser read that back as a target of `"--format"`. Not reachable from the current UI, but the guard is one line.

One behavior is pinned rather than fixed: `parse_profiles` drops a repeated `[profile x]` section including its keys, where the AWS CLI merges them. The test names this so a future change is deliberate.

### Not covered

`probe_session` and `run_login` shell out to the `aws` CLI and are still untested; that needs either a fake binary on `PATH` or an injection point for the command runner.

### Done looks like

Config parsing and precedence are covered by tests over fixture files. The `credential_process` append has a test proving it does not corrupt an existing config.

## 6. Detached HEAD checkout does not work on remote projects

**Status: done (2026-09-10), with a caveat.** Wired through `proto::GitChangeToCommit`. Ships untested: the fork has no remote-git test harness. See the correction below.

**Priority: low.** Small and well-scoped.

`crates/project/src/git_store/lathe.rs:327` in `change_to_commit`:

```rust
RepositoryState::Remote(_) => {
    bail!("detached checkout is not yet supported on remote projects")
}
```

The local path calls `backend.change_to_commit(revision)`, defined at `crates/git/src/repository.rs:848` and implemented at `:2557`. The only caller is the "Checkout this commit" menu entry in `crates/git_graph/src/git_graph.rs:3818`. The README documents the limitation: "Local repositories only for now; collab projects bail with a clear error."

### Correction to this item's premise

The original write-up said to "follow the neighboring operations." There are no such neighbors. **Every** Lathe-owned git operation bails on remote projects, not two: cherry-pick, revert, merge, rebase (plain, interactive, and actions), tag create/delete/list, reflog, branch force-update, conflict resolution, commit-range listing, stash by message, submodule update, and both LFS commands. That is 17 bail sites in `lathe.rs`. `change_branch` is upstream code, not a sibling.

So this was not "wire up the one that was missed." It was establishing the first proto message for a Lathe-owned git *write*, and 16 siblings are still local-only. Whether they should follow is a real decision, not a cleanup.

### Work (done)

Wired `change_to_commit` through `proto::GitChangeToCommit`, modeled on upstream's `GitChangeBranch` and on `GitFileHistory` (the fork's only prior proto, and a read). Touched: `git.proto`, `zed.proto` (envelope 482, at the fork's tail with a note about renumbering), `proto.rs` (three macro tables), `collab/src/rpc.rs` (`forward_mutating_project_request`, matching `GitChangeBranch`), `git_store.rs` (handler registration), and `lathe.rs` (handler plus the remote arm).

Note that `GitFileHistory` has **no** collab routing, so file history works over ssh remoting but not in a collab-hosted project. That looks like an oversight rather than a decision, and is worth a separate look.

### Remaining

- No test. `crates/collab/src/tests` does not exist in this fork and `change_branch` has no remote test either, so there is no harness to follow. The change is verified by `cargo check` and by symmetry with the upstream handler only.
- The other 16 bail sites are untouched.

### Done looks like

Checking out a commit works in a collab project, or the limitation is confirmed as inherent and the README explains why rather than saying "for now."

---

# Unassigned housekeeping

Not features, but they are sitting in the tree and someone should decide on them.

- **v1.0.1-beta is unreleased and uncommitted.** `RELEASE_NOTES.md` documents it, `crates/zed/Cargo.toml` is bumped to `1.0.1`, and `crates/zed/RELEASE_CHANNEL` is flipped to `beta`, all as uncommitted working-tree changes alongside the zoom implementation in `theme_settings` and `zed.rs`.
- **Two releases have no tags.** `v1.0.0` is tagged. `v0.236.35-beta` and `v1.0.1-beta` are not, despite both having release notes and release commits.
- **`.rules` has an unrelated 63-line addition.** An uncommitted "context-mode MANDATORY routing rules" block was appended to `.rules`. It describes MCP tool routing and has nothing to do with Lathe. `.rules` is read by every agent session in this repo, so this affects both tracks. Confirm it was intentional before it gets committed.
