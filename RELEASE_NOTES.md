# Lathe Release Notes

Most recent releases first. Beta releases (`-beta` suffix) ship as GitHub pre-releases and typically batch new features ahead of a stable cut.

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
