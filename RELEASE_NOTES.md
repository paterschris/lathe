# Lathe Beta — AI Account Switcher

A new feature for managing multiple subscription-authenticated identities (accounts) per AI agent, with per-workspace binding, brand-accented UI, and per-account conversation history.

## Highlights

- **Multi-account support** for the three Tier A ACP-mode CLI agents: Claude Code, Gemini CLI, Codex CLI.
- **Workspace-bound by default** — each workspace's `.zed/settings.json` can pin a different account per agent. Falls back to a global default, which itself falls back to the implicit single-account default when only one exists.
- **In-panel chip** in the Agent Panel header showing the active account for the active agent, brand-tinted with the agent's accent color (Claude Code burnt-orange, Gemini blue, Codex green). Click to switch, add, or manage.
- **Manage AI Accounts modal** (command palette: `agent: manage ai accounts`) — list per agent, add / delete / set-default / verify-connection, expandable conversation history per account, brand-accented section dividers, empty-state hero on first run.
- **Add AI Account modal** — agent picker, optional Sign-up link (opens provider's pricing page in browser), display-name input with case-insensitive uniqueness validation, brand-tinted Connect CTA.
- **Auto-trigger of agent login flow** after Connect:
  - Claude Code & Gemini: opens a fresh ACP thread for the agent (workspace already bound, env var injected at spawn) — user types `/login` or `/auth`.
  - Codex: opens a dock terminal with `CODEX_HOME` env set and `codex login` running, since Codex uses browser-OAuth that needs a real terminal.
- **Per-account conversation history** — click an account row to expand it; shows the 20 most recent conversations parsed from disk. Click any row to copy the agent's resume command (`claude --resume <id>`, `codex exec resume <id>`, `gemini --resume <id>`) to the clipboard.
- **Migration import from `claude-account-switcher`** — when `~/.claude-profiles/` exists, the Claude Code section header gains an "Import from claude-account-switcher" button. Imported profiles are registered by reference (no copying); the shell helper continues to work alongside.

## Implementation details

- New crate `crates/ai_accounts/` provides the descriptor / registry / parser / lifecycle layer. Storage: `paths::config_dir().join("ai_accounts.json")` for the index, `paths::data_dir().join("ai_accounts/<agent>/<id>/")` for new account directories.
- ACP server spawn (`crates/agent_servers/src/acp.rs`) reads `AiAccountsSettings` and the on-disk index at thread spawn time, resolves the bound account per agent, and injects the agent's config-dir env var (`CLAUDE_CONFIG_DIR`, `GEMINI_CONFIG_DIR`, `CODEX_HOME`) into the spawned subprocess.
- `last_used_at` is touched at ACP spawn and on chip switch so the Manage modal sorts most-used-first within each agent.
- Codex's keyring credential bypass is mitigated at create time: the per-account `config.toml` gets `cli_auth_credentials_store = "file"` written so OAuth tokens land inside the account's config dir rather than the OS keyring.

## Conversation history parsers

| Agent | Path | Format |
|---|---|---|
| Claude Code | `<config_dir>/projects/<project>/<session>.jsonl` | JSONL, first user message extracted as title |
| Codex CLI | `<CODEX_HOME>/sessions/YYYY/MM/DD/rollout-*.jsonl` | JSONL with date-partitioned dirs (also handles legacy flat layout); first line is `session_meta`, first `response_item` with `role: user` becomes the title |
| Gemini CLI | `<config_dir>/tmp/<project>/chats/session-*.json` | JSONL despite `.json` extension; first line is `metadata`; subsequent records have `type: "user"` / `"model"` |

## Polish

- Status toasts on every meaningful action (Connect, Delete, Import, Copy resume command, Verify outcome).
- Confirm-before-delete prompt with explicit destructive language.
- Optimistic Pending state during async verify.
- Empty-state hero block when no accounts exist anywhere, with primary "Add your first account" CTA + the import button when applicable.

## Out of scope (deliberate)

- API-key authentication for Claude / OpenAI / Google. Subscription auth only.
- Zed's first-party agent (different code path, Keychain-backed credentials).
- GitHub Copilot CLI and Cursor agent — not yet integrated upstream in Lathe; deferred until they're first-class.
- Auto-injection of the `/login` slash command into the freshly-opened thread — would require ~50 lines of plumbing across four files; saves one keystroke. Skipped.
- ACP-spawn-based conversation resumption — currently clipboard-based. Auto-spawning a thread with `--resume` args needs deeper integration with the spawn pipeline, especially for Gemini's project-hash cwd requirement.

## Caveats per agent

- **Claude Code**: the npm shim hardcodes `~/.claude/` for some local-detection paths (anthropics/claude-code#2986, #3833). Auth and memory both honor `CLAUDE_CONFIG_DIR` so per-account isolation works for our use case, but flag if a future feature regresses.
- **Gemini CLI**: `GEMINI_CONFIG_DIR` is broken on Windows (google-gemini/gemini-cli#8248). macOS/Linux unaffected. Sessions are scoped by project hash, so resuming requires the same cwd as the original conversation.
- **Codex CLI**: see implementation note above re: `cli_auth_credentials_store=file`.
