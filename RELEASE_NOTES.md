# Lathe Release Notes

Most recent releases first. Beta releases (`-beta` suffix) ship as GitHub pre-releases and typically batch new features ahead of a stable cut.

## v1.2.0 - 2026-09-15

A large upstream merge, plus per-workspace panel placement and a quieter pull request panel.

### Added

- **The agent panel's dock is remembered per workspace.** Moving the panel used to write the global `agent.dock` setting, which every window shares, and which every release channel shares too because the config directory ignores the channel. Dragging the panel in one project moved it in all of them, stable and beta alike. A workspace that has been customized now records its own placement and stops following the setting. **Use Default Position** in the panel menu, offered only once a workspace has a placement of its own, hands it back to the global setting.
- **Mark a pull request as read without opening it.** Right-click a flagged row in the pull request panel and choose **Mark as Read** to clear its updated indicator. The entry appears only on rows that are actually flagged, and a pull request that changes again afterwards is flagged again, exactly as if it had been opened.

### Fixed

- **The welcome page scrolls again.** Its padding, maximum width and vertical centering sat on the scrolling container itself, so content taller than the window was clipped rather than reachable. Those now sit on an inner column that centers horizontally, and the scroll position is tracked.
- **Cycling a panel to its next dock no longer risks a panic.** Moving the focused panel resolved its destination while the dock was already being updated, which is the shape GPUI rejects with a double lease. The destination is now read first, through a new `next_position`, and applied outside the update.
- Relocating a panel between docks runs through one shared path, so the global-settings route and the new per-workspace route cannot drift apart in how they preserve a panel's visibility and size.

### Changed

- **Merged upstream Zed**, 262 commits. Among them: an LSP call hierarchy modal and its `call_hierarchy.modal_max_width` setting, agent client protocol 2.1, and Rust 1.98.1.

### Known issues

- This release was verified by a full `cargo check --workspace --all-targets` on macOS. The new per-workspace placement test compiles, but the test suite was not run before the cut, and Windows and Linux are unexercised.
- Windows installers are signed, but SmartScreen still shows a reputation prompt. Choose **More info**, then **Run anyway**.

---

## v1.1.0 - 2026-09-11

The first stable release since 1.0.0. It carries everything from the 1.0.1 beta plus the work below.

### Added

- **The composer walks your prompt history.** Up and down in the agent panel's composer recall prompts you have already sent, the way a shell walks its command history. The keys only take over at the edges of the text, so moving around inside a multi-line prompt is unchanged, and the editors for past and queued messages keep ordinary cursor movement.
- **Create a Pixel Tablet Android virtual device from the Mobile panel.** The Android device menu offers separate phone and tablet AVDs, both on API 35 and the host-matched Google APIs image.
- **Window zoom scales the whole window.** `Cmd +`, `Cmd -`, and `Cmd 0` resize the editor, terminal, project panel, git panel, git graph, pull requests, agent panel, git commit editor, and markdown preview together, scoped to the active window.
- **Checking out a commit works in remote and collab projects.** Detached HEAD checkout from the history view and commit graph previously refused anything but a local repository.

### Fixed

- **The pull request panel shows every repository again.** It built its section list and then failed to store it, so a workspace with two repositories reported none at all and the panel read "No repositories open". Each repository now gets its own collapsible section with its open count.
- **Clicking an agent link to a file outside the project opens it.** The link was resolved against the project's worktrees, which return nothing for an outside path, so the click silently did nothing.
- **Creating a worktree from a remote branch starts from the remote tip.** The branch target resolved to `origin/main` rather than the fully qualified `refs/remotes/origin/main`.
- **Rainbow parameter highlighting works again.** Bundled themes define the eight parameter colors, and declarations and references take the color for their ordinal position.
- **Grouped runnable queries emit every runnable again**, restoring table-test discovery and per-item metadata.
- **The Mobile panel runs on Windows.** It shelled out to `./gradlew`, a Unix shell script, and opened terminals through `$SHELL -lic`, neither of which exists there; sixteen further lookups asked for `sdkmanager`, `avdmanager`, `adb`, `java`, and `emulator` by their Unix names and so reported the Android toolchain as missing rather than erroring.
- **New pull requests take their branches from pickers** rather than free text, so a mistyped branch cannot be submitted. Recorded API fixtures now cover close, reopen, reviewer requests, draft transitions, merge, and squash merge across GitHub, GitLab, and Bitbucket.
- **The peek view leaves hover popovers and context menus above it**, and is covered in multibuffers and splits; Vim `j` and `k` move its location list.
- Logging out of an agent that supports it now actually clears the authenticated state.

### Changed

- The feature catalog moved out of the README to [docs/features.md](docs/features.md), which doubles as the checklist of what this fork changes on top of upstream.

### Known issues

- **Two tests covering restoration of a promoted draft thread across a reload fail**, and have failed since v1.0.0. Reloading the agent panel may not restore a promoted thread's messages. Not a new regression, but not yet fixed.
- One `project` test covering git state refresh for a bare `.git` file fails, also pre-existing.
- **The Windows fixes above are verified by compilation and tests only.** No Windows machine has run them. Linux is likewise unexercised, as are the window zoom and peek view on both.
- Detached HEAD checkout over collab ships without a test; the fork has no remote-git test harness. The other sixteen Lathe git operations still refuse to run on remote projects.
- Windows installers are signed, but SmartScreen still shows a reputation prompt. Choose **More info**, then **Run anyway**.

---

## v1.0.1-beta - 2026-09-10

Zoom now scales the whole window, not just the editor.

### Changed

- **`Cmd +`, `Cmd -`, and `Cmd 0` zoom every piece of text in the window together:** the editor, terminal, project panel, git panel, git graph, pull requests, agent panel, git commit editor, and markdown preview. The View menu zoom items and `Cmd + scroll-wheel` (when mouse-wheel zoom is enabled) do the same. Before this only the editor and terminal changed size, and the agent panel and markdown preview zoomed on their own when focused.
- The zoom is still scoped to the active window, so two windows side-by-side can be zoomed independently.
- The persisted zoom items write both `buffer_font_size` and `ui_font_size` to `settings.json`. Agent, git commit, and markdown preview sizes are only written when you had already set them. Reset clears all of them.

### Added

- **Create a Pixel Tablet Android virtual device from the Mobile panel.** The Android device menu now offers separate phone and tablet AVDs, both using API 35 and the host-matched Google APIs image.

### Fixed

- **Rainbow parameter highlighting works again.** Bundled themes define the eight parameter colors, and declarations and references now receive the color for their ordinal parameter position.
- **Grouped runnable queries emit every runnable again.** The buffer now uses the grouped-runnable resolver, restoring table-test discovery and per-item metadata.
- **New pull requests use local branch pickers.** The source and target branches must come from the repository's local branch list, so a mistyped branch cannot be submitted. Recorded API fixtures now cover close, reopen, reviewer requests, draft transitions, merge, and squash merge for GitHub, GitLab, and Bitbucket.
- **Peek leaves hover popovers and context menus above it.** The peek remains above the minimap at deferred-draw priority 0, while the editor draws those overlays at higher priorities. Peek is also covered in multibuffer and split editors, and Vim `j` and `k` now move its location list.
- **The Mobile panel can run on Windows.** It could not before: the panel shelled out to `./gradlew`, which is a Unix shell script rather than Windows' `gradlew.bat`, and it opened every terminal through `$SHELL -lic`, which on Windows means an unset variable, a `/bin/zsh` that does not exist, and POSIX-only flags. Sixteen further lookups asked for `sdkmanager`, `avdmanager`, `adb`, `java`, and `emulator` by their Unix names, so the Android toolchain reported itself as not installed rather than erroring. Run hints are also scraped from READMEs that spell the wrapper `gradlew.bat`.
- **Checking out a commit works in remote and collab projects.** Detached HEAD checkout from the history view and commit graph previously failed on anything but a local repository.
- An AWS profile whose name was empty wrote a malformed `credential_process` line into the AWS config. Empty names are now refused.

### Known issues

- The Windows fixes above are reasoned from Android's published tool layout and are verified by compilation and tests, but **no Windows runtime behavior has been observed**. Linux is likewise unexercised, as are the window zoom and peek view on both.
- Detached HEAD checkout over collab ships without a test; the fork has no remote-git test harness. The other 16 Lathe git operations still refuse to run on remote projects.
- Windows installers are signed, but SmartScreen still shows a reputation prompt. Choose **More info**, then **Run anyway**.

---

## v1.0.0 - 2026-09-01

Lathe moves to 1.0 and off the version number it inherited from upstream. This release contains the peek view that shipped as v0.236.35-beta, plus the two fixes below.

### Added

- **Peek view for LSP results.** Find All References, Go to Definition, Declaration, Implementation and Type Definition open a panel below the cursor line instead of a new tab: a preview of the selected location beside the list of locations, grouped by file. Arrow keys move through the list and the preview follows; Enter or a double click goes to a location and closes the peek; Escape or clicking back into the code dismisses it. Cmd-click opens it too, since going to a definition is the case where being sent to another tab is least wanted. Alt-cmd-click still splits the pane.
- **The divider between the two panes can be dragged**, and where you leave it is remembered by later peeks and across restarts.

### Changed

- `lsp_results_location` defaults to `peek` rather than `multi_buffer`. Set it back to `multi_buffer` for the previous behaviour, or `picker` for the filterable list.
- In `peek`, a single result is shown in the peek rather than opened directly, which is the point of the mode. `multi_buffer` and `picker` still go straight to it.
- **Version numbers restart at 1.0.0.** Lathe had been carrying upstream Zed's 0.236.x line, which said nothing about the fork. Updating from 0.236.x is offered as normal.

### Fixed

- The peek list renders only the rows on screen. Before this, a symbol with thousands of references laid out every row on every frame.
- Vim mode no longer drives the editor behind the peek. The peek now declares the `menu` key context, which is what Vim's normal-mode bindings key off, so `j` and `k` move through the list rather than the buffer underneath.

### Known issues

- Tested on macOS, in ordinary single-file editors. The peek inside a multibuffer (project search results, diffs), in a split, and on Linux and Windows is unexercised. The Vim fix is reasoned from the keymap rather than confirmed by use.
- The peek paints above everything else in the editor. If it covers a hover popover or a context menu, that is why.
- Windows installers are signed, but SmartScreen still shows a reputation prompt. Choose **More info**, then **Run anyway**.

---

## v0.236.35-beta - 2026-09-01

Finding references no longer takes you out of the file you are reading.

### Added

- **Peek view for LSP results.** Find All References, Go to Definition, Declaration, Implementation and Type Definition now open a panel below the cursor line instead of a new tab: a preview of the selected location beside the list of locations, grouped by file. Arrow keys move through the list and the preview follows; Enter or a double click goes to a location and closes the peek; Escape or clicking back into the code dismisses it. Cmd-click opens it too, since going to a definition is the case where being sent to another tab is least wanted. Alt-cmd-click still splits the pane.
- **The divider between the two panes can be dragged**, and where you leave it is remembered by later peeks and across restarts.

### Changed

- `lsp_results_location` defaults to `peek` rather than `multi_buffer`. Set it back to `multi_buffer` for the previous behaviour, or `picker` for the filterable list.
- In `peek`, a single result is shown in the peek rather than opened directly, which is the point of the mode. `multi_buffer` and `picker` still go straight to it.

### Known issues

- The list renders every result rather than only the visible ones. Find All References on a symbol with thousands of hits is expected to be slow.
- Testing was on macOS, in ordinary single-file editors, without Vim mode. The peek in a multibuffer (project search results, diffs), under Vim mode, in a split, and on Linux and Windows is unexercised.
- The peek paints above everything else in the editor. If it covers a hover popover or a context menu, that is why.
- Windows installers are signed, but SmartScreen still shows a reputation prompt. Choose **More info**, then **Run anyway**.

---

## v0.236.34-beta — 2026-08-27

Fixes a data-loss class of bug that only shows up when two Lathe channels run at the same time, and finishes the pull request panel's separation between reviewing someone's work and acting on your own.

### Fixed

- **Running two channels side by side no longer breaks the running one.** Every channel shared one Node.js installation and npm cache under `node/`, and every launch reset it. Starting Beta therefore deleted the packages that an already-running Stable was executing from, and vice versa. Language servers and agent servers spawned before the wipe kept running against paths that no longer existed. Codex showed this as a total loss of tools: it could still talk, but every tool call failed to spawn its `codex-code-mode-host` sidecar, and it reported the workspace as unavailable. Each channel now gets its own `node/<channel>` directory.
- Creating a Bitbucket pull request as a draft ignored the draft option and produced an ordinary pull request.

### Improved

- **Author-only actions move out of the button row on pull requests you did not open.** Merge, Squash & merge, the draft toggle, and Decline / Close are collected under a **More** menu, so a destructive action is no longer one stray click away from **Approve**. They remain fully available, and the host still decides whether you are permitted to use them. Lathe resolves authorship from the account you connected; when the token cannot report an identity, the buttons stay in the row rather than being demoted for someone who may well be the author.

### Upgrade note

- The first launch after updating re-downloads the npm packages for your language servers and agent servers, because they now live under a per-channel directory. The old `node/` directory is left in place on purpose rather than cleaned up, since removing it would be the same cross-channel wipe one last time against a build that has not updated yet. Once every channel on the machine is on 0.236.34 or later, it is safe to delete by hand: `~/Library/Application Support/Zed/node/cache` and `~/Library/Application Support/Zed/node/node-v*` on macOS.

### Known issues

- The channel fix is verified by build and static analysis only. It has not been exercised by running two updated channels concurrently.
- The pull request overflow menu has not been exercised against a live host, and neither had the reviewer, draft and close work shipped in v0.236.33-beta. Declining and requesting reviewers on Bitbucket remain the only live-tested write paths.
- The New Pull Request dialog's branch fields are still free text rather than pickers.
- Windows installers are signed, but SmartScreen still shows a reputation prompt. Choose **More info**, then **Run anyway**.

---

## v0.236.33-beta — 2026-08-26

Completes the write side of the pull request panel. Until now it could open, review, comment on and merge a pull request, but every path that did not end in a merge was missing: it listed closed pull requests while offering no way to close one, and showed reviewers with no way to request one.

### Added

- **Request reviewers** from the pull request view, through a searchable multi-select list of the accounts the host will accept. People are listed by name where the host reports one, with their handle underneath. Bitbucket and GitLab report real names; GitHub's collaborator listing does not, so it shows logins.
- **Close, decline and reopen** pull requests, using each host's own wording. Bitbucket calls it declining.
- **Convert to draft** and **Mark ready for review**, on hosts that model drafts.

### Improved

- Draft pull requests are now clearly marked. Draft previously rendered in the faintest grey available, in both the list and the detail header, and greyed out the row icon as well, which made a draft the least noticeable entry in a list of pull requests. It is now an amber badge in both places, shown alongside the state rather than replacing it, since a pull request is both open and draft.

### Fixed

- Creating a Bitbucket pull request as a draft ignored the draft option and always produced an ordinary pull request.

### Notes for reviewers on Bitbucket

Requesting a review reads the pull request first and merges into its existing reviewer list, because Bitbucket's update replaces that list wholesale. Adding someone will not displace reviewers already assigned.

### Known issues

- Draft transitions, reopening, and everything on GitHub and GitLab have not been exercised against a live host. Declining and requesting reviewers have been, on Bitbucket.
- The New Pull Request dialog's branch fields are still free text rather than pickers.
- Windows installers are signed, but SmartScreen still shows a reputation prompt. Choose **More info**, then **Run anyway**.

---

## v0.236.32-beta — 2026-08-26

A fix release for the pull request panel shipped in v0.236.30-beta. Two of these were dead on arrival and only surfaced once the panel was used against a real repository.

### Fixed

- The panel's **New pull request** and **Reconnect** buttons did nothing when clicked. Both dispatched through the app rather than the window, which re-enters the active window's update from inside that same update; the failure was swallowed and left only a log line. Reconnect had been broken since the panel was rewritten and is reachable only with an expired credential, which is why it went unnoticed.
- API base URLs for self-hosted hosts discarded the scheme and port of the configured base URL. A GitHub Enterprise instance on a non-default port, or a self-managed GitLab reachable only over plain HTTP internally, addressed an endpoint nothing was listening on. Both now preserve scheme, host and port.

### Improved

- Inline review comments show a calendar date instead of the host's raw ISO-8601 timestamp.
- The pull request header reads `3 files, +25` rather than `3 file(s), +25 -0`.
- A reviewer who commented without a verdict gets a distinct icon, instead of a dash that read as stray punctuation and was indistinguishable from a reviewer who had not looked yet.

### Known issues

- Creating a pull request from the panel has not been exercised against a live host.
- The New Pull Request dialog's branch fields are free text rather than pickers.
- Windows installers are signed, but SmartScreen still shows a reputation prompt. Choose **More info**, then **Run anyway**.

---

## v0.236.31-beta — 2026-08-25

An upstream sync, bringing Lathe up to date with 78 commits from Zed. The pull request panel and Windows code signing shipped in v0.236.30-beta and are unchanged here.

### Security

- Updated Wasmtime's WASI implementation to fix a filesystem sandbox escape. This affects extensions, which run in that sandbox.
- Disabled one-time code autofill in Lathe's text inputs.

### Editor

- Fixed `fold_at_level` folding a function's arguments instead of its body.
- Oversized LSP hover contents are truncated before display rather than overflowing.
- The gutter repaints immediately when bookmarks change.
- Format-on-save no longer runs against read-only files.
- Overlapping range-formatting results are deduplicated properly.
- `lsp_results_location` is now respected for go-to-declaration and go-to-type-definition.
- Added a configurable debounce timeout for inline completions.

### Git

- Fixed a crash when selecting collapsed sections in the git panel.
- Added an action to toggle the diff base.
- Added Tangled as a git hosting provider.
- Recursive blaming no longer emits useless toasts.
- Fixed global gitignore matching outside the worktree root.

### Markdown

- Improved preview typography and inline code rendering.
- Restored fallback language highlighting for untagged code blocks.

### Debugger

- Continue Program and Continue Thread are now separate actions.
- Python's locator passes through `env` variables from the task template.

### AI

- Added Gemini 3.5 Flash-Lite and removed deprecated Gemini models.
- Added the Baseten provider.
- Copilot Chat supports data-resident GitHub Enterprise.
- Provider rejection details are preserved instead of discarded, and transport errors now include the host.
- Failed agent connections can be reloaded from the panel.
- The most recently selected agent persists correctly.

### Platform

- **Linux**: demand-driven Wayland render loop, prewarmed font match caches, several X11 fixes, and GLib is no longer bundled in release archives.
- **Windows**: support for Restart Manager shutdown.
- **macOS**: fixed session restore when a window is closed with the X button.

### Workspace

- Recent navigation history persists across sessions.
- The terminal panel registers before serialized terminals are restored.
- Terminals no longer steal focus from an open modal.
- Undoing a rename removes directories the rename created.
- Added a Clear button to the settings search field.

### Known issues

- The pull request panel's GitLab and GitHub Enterprise support has not been exercised against a live host.
- Windows installers are signed, but SmartScreen still shows a reputation prompt. Choose **More info**, then **Run anyway**. The dialog names Christopher Paterson as the publisher rather than reporting an unknown one.

---

## v0.236.17-beta — 2026-06-16

GitKraken-parity push for the Git panel, plus a worktree-aware debug scenario fix.

### Git Explorer panel

- Branches in the Local and Remote sections render as a collapsible folder tree, splitting names on `/`. So `feature/auth/login` and `feature/auth/signup` collapse under a single `feature/auth/` folder that you can fold or expand. Folders show counts of contained branches and remember their open/closed state per section.
- When the filter input is active the tree flattens to a plain list so filter results stay legible.
- Explorer context menus thread a weak handle back to the panel, so successful branch operations (checkout, merge, rebase, delete) refresh the explorer data immediately instead of leaving stale rows.

### Branch from commit

- New BranchFromCommitModal: from any commit in the Git history view, create a new branch off that revision without first checking out.

### Detached HEAD checkout

- New `change_to_commit(revision)` on the repository backend, project GitStore, and FakeGitRepository. The UI can now check out an arbitrary SHA into detached HEAD from the history view. Local repositories only in this release; collab projects bail with a clear error.

### Interactive rebase modal

- Layout reworked. Drag-and-drop reordering of commits, and per-row action affordances (pick, squash, edit, drop) inline.

### Git graph

- Expanded edge routing and lane assignment so wider histories lay out without crossings.

### Debug scenarios

- When a task source is a worktree, the resulting debug scenario context is taken from that worktree's task context (and worktree id) rather than the globally-active one. Scenarios from every worktree context are now surfaced, not just the first.

### Release tooling

- `script/release-fork` now tags `-beta`, `-pre`, and `-rc` versions as GitHub pre-releases automatically.

---

## Lathe Beta, AI Account Switcher

A feature for managing multiple subscription-authenticated identities (accounts) per AI agent, with per-workspace binding, brand-accented UI, and per-account conversation history.

### Highlights

- **Multi-account support** for the three Tier A ACP-mode CLI agents: Claude Code, Gemini CLI, Codex CLI.
- **Workspace-bound by default**. Each workspace's `.zed/settings.json` can pin a different account per agent. Falls back to a global default, which itself falls back to the implicit single-account default when only one exists.
- **In-panel chip** in the Agent Panel header showing the active account for the active agent, brand-tinted with the agent's accent color (Claude Code burnt-orange, Gemini blue, Codex green). Click to switch, add, or manage.
- **Manage AI Accounts modal** (command palette: `agent: manage ai accounts`). List per agent, add / delete / set-default / verify-connection, expandable conversation history per account, brand-accented section dividers, empty-state hero on first run.
- **Add AI Account modal**. Agent picker, optional Sign-up link (opens provider's pricing page in browser), display-name input with case-insensitive uniqueness validation, brand-tinted Connect CTA.
- **Auto-trigger of agent login flow** after Connect:
  - Claude Code and Gemini: opens a fresh ACP thread for the agent (workspace already bound, env var injected at spawn). User types `/login` or `/auth`.
  - Codex: opens a dock terminal with `CODEX_HOME` env set and `codex login` running, since Codex uses browser-OAuth that needs a real terminal.
- **Per-account conversation history**. Click an account row to expand it; shows the 20 most recent conversations parsed from disk. Click any row to copy the agent's resume command (`claude --resume <id>`, `codex exec resume <id>`, `gemini --resume <id>`) to the clipboard.
- **Migration import from `claude-account-switcher`**. When `~/.claude-profiles/` exists, the Claude Code section header gains an "Import from claude-account-switcher" button. Imported profiles are registered by reference (no copying); the shell helper continues to work alongside.

### Implementation details

- New crate `crates/ai_accounts/` provides the descriptor / registry / parser / lifecycle layer. Storage: `paths::config_dir().join("ai_accounts.json")` for the index, `paths::data_dir().join("ai_accounts/<agent>/<id>/")` for new account directories.
- ACP server spawn (`crates/agent_servers/src/acp.rs`) reads `AiAccountsSettings` and the on-disk index at thread spawn time, resolves the bound account per agent, and injects the agent's config-dir env var (`CLAUDE_CONFIG_DIR`, `GEMINI_CONFIG_DIR`, `CODEX_HOME`) into the spawned subprocess.
- `last_used_at` is touched at ACP spawn and on chip switch so the Manage modal sorts most-used-first within each agent.
- Codex's keyring credential bypass is mitigated at create time: the per-account `config.toml` gets `cli_auth_credentials_store = "file"` written so OAuth tokens land inside the account's config dir rather than the OS keyring.

### Conversation history parsers

| Agent | Path | Format |
|---|---|---|
| Claude Code | `<config_dir>/projects/<project>/<session>.jsonl` | JSONL, first user message extracted as title |
| Codex CLI | `<CODEX_HOME>/sessions/YYYY/MM/DD/rollout-*.jsonl` | JSONL with date-partitioned dirs (also handles legacy flat layout); first line is `session_meta`, first `response_item` with `role: user` becomes the title |
| Gemini CLI | `<config_dir>/tmp/<project>/chats/session-*.json` | JSONL despite `.json` extension; first line is `metadata`; subsequent records have `type: "user"` / `"model"` |

### Polish

- Status toasts on every meaningful action (Connect, Delete, Import, Copy resume command, Verify outcome).
- Confirm-before-delete prompt with explicit destructive language.
- Optimistic Pending state during async verify.
- Empty-state hero block when no accounts exist anywhere, with primary "Add your first account" CTA + the import button when applicable.

### Out of scope (deliberate)

- API-key authentication for Claude / OpenAI / Google. Subscription auth only.
- Zed's first-party agent (different code path, Keychain-backed credentials).
- GitHub Copilot CLI and Cursor agent. Not yet integrated upstream in Lathe; deferred until they're first-class.
- Auto-injection of the `/login` slash command into the freshly-opened thread. Would require ~50 lines of plumbing across four files; saves one keystroke. Skipped.
- ACP-spawn-based conversation resumption. Currently clipboard-based. Auto-spawning a thread with `--resume` args needs deeper integration with the spawn pipeline, especially for Gemini's project-hash cwd requirement.

### Caveats per agent

- **Claude Code**: the npm shim hardcodes `~/.claude/` for some local-detection paths (anthropics/claude-code#2986, #3833). Auth and memory both honor `CLAUDE_CONFIG_DIR` so per-account isolation works for our use case, but flag if a future feature regresses.

  Lathe also defaults `ENABLE_CLAUDEAI_MCP_SERVERS=false` and `MCP_TIMEOUT=5000` per ACP spawn so claude.ai-managed cloud connectors (Gmail, Calendar, Drive that come down from a Claude Max account) don't hang thread startup waiting for OAuth that the ACP transport can't surface. To opt in to the cloud connectors, set `ENABLE_CLAUDEAI_MCP_SERVERS=true` in the workspace's `.zed/settings.json`:

  ```json
  {
    "agent_servers": {
      "claude-acp": {
        "env": { "ENABLE_CLAUDEAI_MCP_SERVERS": "true" }
      }
    }
  }
  ```

  The upstream issues asking for true lazy/deferred MCP loading (anthropics/claude-code#16254, #13700) were closed inactive; this default-off is the cleanest currently-available workaround.
- **Gemini CLI**: `GEMINI_CONFIG_DIR` is broken on Windows (google-gemini/gemini-cli#8248). macOS/Linux unaffected. Sessions are scoped by project hash, so resuming requires the same cwd as the original conversation.
- **Codex CLI**: see implementation note above re: `cli_auth_credentials_store=file`.
