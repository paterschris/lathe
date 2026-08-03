use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use collections::HashMap;
use serde::{Deserialize, Serialize};
use settings::{RegisterSetting, Settings, SettingsContent};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccountState {
    #[default]
    Created,
    Pending,
    Authenticated,
    Failed,
    Expired,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AiAccount {
    pub id: String,
    pub agent_id: String,
    pub display_name: String,
    pub config_dir: PathBuf,
    #[serde(default)]
    pub state: AccountState,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_used_at: Option<DateTime<Utc>>,
}

impl AiAccount {
    pub fn new(
        agent_id: impl Into<String>,
        display_name: impl Into<String>,
        config_dir: PathBuf,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            agent_id: agent_id.into(),
            display_name: display_name.into(),
            config_dir,
            state: AccountState::Created,
            created_at: Some(Utc::now()),
            last_used_at: None,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AiAccountsIndex {
    #[serde(default)]
    pub accounts: Vec<AiAccount>,
    /// Explicit user-chosen default account per agent (`agent_id` → `account_id`).
    /// When absent, falls back to implicit single-account default.
    #[serde(default)]
    pub global_defaults: HashMap<String, String>,
}

impl AiAccountsIndex {
    pub fn find(&self, id: &str) -> Option<&AiAccount> {
        self.accounts.iter().find(|a| a.id == id)
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut AiAccount> {
        self.accounts.iter_mut().find(|a| a.id == id)
    }

    pub fn for_agent(&self, agent_id: &str) -> impl Iterator<Item = &AiAccount> {
        self.accounts.iter().filter(move |a| a.agent_id == agent_id)
    }

    /// Resolves the default account for an agent. Order:
    /// 1. Explicit `global_defaults` entry, if it points at an existing account.
    /// 2. Implicit: if exactly one account exists for the agent, return it.
    /// 3. None.
    pub fn default_for_agent(&self, agent_id: &str) -> Option<&AiAccount> {
        if let Some(default_id) = self.global_defaults.get(agent_id) {
            if let Some(account) = self.find(default_id) {
                return Some(account);
            }
        }
        let mut iter = self.for_agent(agent_id);
        let first = iter.next()?;
        if iter.next().is_some() {
            return None;
        }
        Some(first)
    }

    pub fn upsert(&mut self, account: AiAccount) {
        if let Some(existing) = self.accounts.iter_mut().find(|a| a.id == account.id) {
            *existing = account;
        } else {
            self.accounts.push(account);
        }
    }

    pub fn remove(&mut self, id: &str) -> Option<AiAccount> {
        let position = self.accounts.iter().position(|a| a.id == id)?;
        let removed = self.accounts.remove(position);
        self.global_defaults
            .retain(|_, account_id| account_id != id);
        Some(removed)
    }

    pub fn set_state(&mut self, id: &str, state: AccountState) -> Result<()> {
        let account = self
            .find_mut(id)
            .ok_or_else(|| anyhow!("no account with id {id}"))?;
        account.state = state;
        Ok(())
    }

    pub fn touch(&mut self, id: &str) -> Result<()> {
        let account = self
            .find_mut(id)
            .ok_or_else(|| anyhow!("no account with id {id}"))?;
        account.last_used_at = Some(Utc::now());
        Ok(())
    }

    pub fn set_default(&mut self, agent_id: &str, account_id: Option<String>) -> Result<()> {
        match account_id {
            Some(id) => {
                let account = self
                    .find(&id)
                    .ok_or_else(|| anyhow!("no account with id {id}"))?;
                if account.agent_id != agent_id {
                    return Err(anyhow!(
                        "account {id} belongs to agent {} not {agent_id}",
                        account.agent_id
                    ));
                }
                self.global_defaults.insert(agent_id.to_string(), id);
            }
            None => {
                self.global_defaults.remove(agent_id);
            }
        }
        Ok(())
    }
}

fn accounts_path() -> PathBuf {
    paths::config_dir().join("ai_accounts.json")
}

pub fn load_index() -> AiAccountsIndex {
    let path = accounts_path();
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        // No file yet is the normal first-run case — an empty index is correct
        // and not worth logging.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return AiAccountsIndex::default();
        }
        Err(error) => {
            log::warn!("ai_accounts: failed to read {}: {error:#}", path.display());
            return AiAccountsIndex::default();
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(index) => index,
        // A parse failure used to silently fall back to an empty index, which
        // both hid the cause (the UI just showed "No AI accounts yet") and
        // risked a later save_index overwriting a recoverable file. Log loudly
        // so the real error is visible.
        Err(error) => {
            log::error!("ai_accounts: failed to parse {}: {error:#}", path.display());
            AiAccountsIndex::default()
        }
    }
}

pub fn save_index(index: &AiAccountsIndex) -> Result<()> {
    let path = accounts_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let bytes = serde_json::to_vec_pretty(index).context("serialize ai accounts index")?;
    std::fs::write(&path, bytes).context("write ai_accounts.json")
}

#[derive(Clone, Copy, Debug)]
pub enum LoginFlow {
    /// Send the command as a slash-message inside the ACP thread bound to the new account.
    InThread { command: &'static str },
    /// Open a regular terminal pane and run the agent's login command interactively.
    EmbeddedTerminal { argv: &'static [&'static str] },
    /// Authenticate by entering a provider API key. Lathe stores the key in the
    /// system keychain scoped to the account (see [`api_key_keychain_url`]) and
    /// injects it into the subprocess at spawn. Used when subscription/OAuth
    /// login is unavailable: Google retired Gemini Code Assist OAuth for
    /// individual accounts (June 2026), so for individuals the Gemini CLI only
    /// authenticates via an AI Studio API key.
    ApiKey {
        /// Where the user obtains a key. Opened in the browser from the modal.
        key_url: &'static str,
        /// Subprocess environment variable the key is injected as (e.g.
        /// `GEMINI_API_KEY`).
        env_var: &'static str,
    },
}

/// Hex-coded per-agent identification color, with light- and dark-theme
/// variants. Applied sparingly (chip dot, modal section dividers, primary
/// CTA tint) — never as a panel-wide skin. Values are muted from each agent's
/// full-saturation marketing color to keep contrast acceptable in both themes
/// and to avoid trademarked-asset replication.
#[derive(Clone, Copy, Debug)]
pub struct BrandAccent {
    pub light: &'static str,
    pub dark: &'static str,
}

#[derive(Clone, Copy, Debug)]
pub struct AgentDescriptor {
    pub agent_id: &'static str,
    pub display_name: &'static str,
    pub config_dir_env_var: &'static str,
    pub login_flow: LoginFlow,
    pub verify_command: &'static [&'static str],
    pub signup_url: &'static str,
    pub brand_accent: BrandAccent,
    /// Default environment variables injected at ACP spawn time, in addition
    /// to the per-account `config_dir_env_var`. These are protective defaults
    /// — e.g. disabling cloud-managed MCP servers that would otherwise block
    /// thread startup waiting for OAuth. Always applied with entry-or-insert
    /// semantics so an explicit value upstream of the spawn (workspace
    /// settings, user shell env) wins.
    pub default_env: &'static [(&'static str, &'static str)],
    /// Environment variables to *remove* from the agent subprocess (and its
    /// login terminal) when a Lathe AI account is bound. These are API-key
    /// vars that would otherwise override the per-account subscription/OAuth
    /// login — e.g. an ambient `GEMINI_API_KEY` makes the Gemini CLI silently
    /// use API-key auth and ignore "Login with Google". Because the subprocess
    /// inherits Lathe's own environment by default, omitting the key from the
    /// injected env map is not enough; it must be explicitly removed.
    pub scrub_env: &'static [&'static str],
}

pub const CLAUDE_CODE_DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    agent_id: "claude-acp",
    display_name: "Claude Agent",
    config_dir_env_var: "CLAUDE_CONFIG_DIR",
    login_flow: LoginFlow::InThread { command: "/login" },
    verify_command: &["claude", "/status"],
    signup_url: "https://claude.ai/upgrade",
    brand_accent: BrandAccent {
        light: "#cc6633",
        dark: "#e89968",
    },
    // Default-off the claude.ai cloud connectors (Gmail/Calendar/Drive that
    // come down from a Claude Max account) so a user who hasn't OAuth'd
    // them doesn't get a hung ACP thread on first launch. Opt in by setting
    // ENABLE_CLAUDEAI_MCP_SERVERS=true in workspace `.zed/settings.json`
    // under `agent_servers.claude-acp.env`, or in the user's shell env.
    //
    // MCP_TIMEOUT is a belt-and-braces 5s cap so any other slow MCP server
    // can't block thread spawn either — Claude Code gives up after the
    // timeout and proceeds without that server's tools.
    default_env: &[
        ("ENABLE_CLAUDEAI_MCP_SERVERS", "false"),
        ("MCP_TIMEOUT", "5000"),
    ],
    scrub_env: &[],
};

pub const GEMINI_DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    agent_id: "gemini",
    display_name: "Gemini CLI",
    // The Gemini CLI relocates its config/credential storage via GEMINI_CLI_HOME
    // (it creates a `.gemini` folder under this root). GEMINI_CONFIG_DIR is NOT
    // honored, so using it left every account sharing ~/.gemini.
    config_dir_env_var: "GEMINI_CLI_HOME",
    // Google retired Gemini Code Assist OAuth ("Login with Google") for
    // individual accounts in June 2026, pushing them to Antigravity. For
    // individuals the Gemini CLI now authenticates only via an AI Studio API
    // key, so accounts are created by pasting a key rather than an OAuth flow.
    login_flow: LoginFlow::ApiKey {
        key_url: "https://aistudio.google.com/apikey",
        env_var: "GEMINI_API_KEY",
    },
    // No CLI status check for key auth; the modal validates the key on entry
    // and the Manage modal verifies by key presence in the keychain instead.
    verify_command: &[],
    signup_url: "https://aistudio.google.com/apikey",
    brand_accent: BrandAccent {
        light: "#4285f4",
        dark: "#7aa9f8",
    },
    default_env: &[],
    // Nothing to scrub: with API-key auth the key IS the credential, so the
    // injected GEMINI_API_KEY must reach the subprocess. (The retired OAuth
    // flow stripped it so "Login with Google" would win instead.)
    scrub_env: &[],
};

pub const CODEX_DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    agent_id: "codex-acp",
    display_name: "Codex CLI",
    config_dir_env_var: "CODEX_HOME",
    login_flow: LoginFlow::EmbeddedTerminal {
        argv: &["codex", "login"],
    },
    verify_command: &["codex", "login", "status"],
    signup_url: "https://chatgpt.com/#pricing",
    brand_accent: BrandAccent {
        light: "#10a37f",
        dark: "#2dc59a",
    },
    default_env: &[],
    scrub_env: &[],
};

pub const TIER_A_DESCRIPTORS: &[AgentDescriptor] =
    &[CLAUDE_CODE_DESCRIPTOR, GEMINI_DESCRIPTOR, CODEX_DESCRIPTOR];

pub fn descriptor_for(agent_id: &str) -> Option<&'static AgentDescriptor> {
    TIER_A_DESCRIPTORS.iter().find(|d| d.agent_id == agent_id)
}

#[derive(Clone, Debug, Default, RegisterSetting)]
pub struct AiAccountsSettings {
    /// Workspace-merged binding map: agent ID → account ID. Workspace `.zed/settings.json`
    /// overrides win over user-level settings (per `MergeFrom`).
    pub bindings: HashMap<String, String>,
}

impl Settings for AiAccountsSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        Self {
            bindings: content
                .ai_accounts
                .as_ref()
                .map(|map| map.0.clone())
                .unwrap_or_default(),
        }
    }
}

impl AiAccountsSettings {
    /// Resolves the active account for an agent in the current workspace.
    /// Order:
    /// 1. Workspace binding from settings (if it points at an existing account in the registry).
    /// 2. Index-level default resolution (`AiAccountsIndex::default_for_agent`).
    /// 3. None.
    pub fn resolve_account<'a>(
        &self,
        agent_id: &str,
        index: &'a AiAccountsIndex,
    ) -> Option<&'a AiAccount> {
        if let Some(bound_id) = self.bindings.get(agent_id) {
            if let Some(account) = index.find(bound_id) {
                return Some(account);
            }
            // A binding can outlive its account when the account was deleted
            // from a different workspace, or when the binding lives in a
            // workspace `.zed/settings.json` that account deletion doesn't
            // rewrite. Falling through silently would run the agent against
            // some other account's credentials, so leave a trail.
            log::warn!(
                "ai_accounts: {agent_id} is bound to unknown account {bound_id}; \
                 falling back to the default account for {agent_id}"
            );
        }
        index.default_for_agent(agent_id)
    }
}

/// Returns the directory under which all per-agent account config dirs are stored.
/// Each individual account lives at `<accounts_root>/<agent_id>/<account_id>/`.
pub fn accounts_root() -> PathBuf {
    paths::data_dir().join("ai_accounts")
}

/// Whether `config_dir` is a directory Lathe created and owns. Accounts
/// imported from `~/.claude-profiles` (and any other account registered by
/// reference) point at directories the user's own workflow still depends on,
/// so removing such an account must not imply removing its files.
pub fn is_managed_config_dir(config_dir: &Path) -> bool {
    config_dir.starts_with(accounts_root())
}

/// Keychain lookup key under which an account's provider API key is stored, for
/// agents whose [`LoginFlow`] is [`LoginFlow::ApiKey`]. The value is opaque to
/// the keychain (used only as a service identifier); scoping it per account
/// keeps multiple API-key accounts for the same agent from colliding. Shared by
/// the Add-account modal (write), the ACP spawn path (read), and account delete
/// (cleanup) so all three agree on the location.
pub fn api_key_keychain_url(agent_id: &str, account_id: &str) -> String {
    format!("https://ai-account.lathe/{agent_id}/{account_id}")
}

/// Validates a proposed account display name for an agent. Returns Err with a
/// user-presentable message when the name is empty or already in use within the
/// same agent (case-insensitive). Caller has already loaded `index`.
pub fn validate_new_account_name(
    index: &AiAccountsIndex,
    agent_id: &str,
    display_name: &str,
) -> Result<()> {
    let trimmed = display_name.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("display name must not be empty"));
    }
    let collision = index
        .for_agent(agent_id)
        .any(|account| account.display_name.eq_ignore_ascii_case(trimmed));
    if collision {
        return Err(anyhow!(
            "an account named \"{trimmed}\" already exists for {agent_id}"
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn set_dir_permissions_700(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o700);
    std::fs::set_permissions(path, perms).with_context(|| format!("chmod 700 {}", path.display()))
}

#[cfg(not(unix))]
fn set_dir_permissions_700(_path: &Path) -> Result<()> {
    Ok(())
}

/// Per-agent quirks that must run after the account's config dir exists but
/// before the user attempts to log in. Currently only Codex requires one:
/// without `cli_auth_credentials_store = "file"`, Codex stores OAuth tokens
/// in the OS keyring, bypassing `CODEX_HOME` and breaking per-account isolation.
fn apply_create_quirks(agent_id: &str, config_dir: &Path) -> Result<()> {
    if agent_id == CODEX_DESCRIPTOR.agent_id {
        let config_path = config_dir.join("config.toml");
        if !config_path.exists() {
            std::fs::write(&config_path, "cli_auth_credentials_store = \"file\"\n").with_context(
                || format!("writing Codex config.toml at {}", config_path.display()),
            )?;
        }
    } else if agent_id == GEMINI_DESCRIPTOR.agent_id {
        // The Gemini CLI chooses its auth method from
        // `<GEMINI_CLI_HOME>/.gemini/settings.json` (`security.auth.selectedType`),
        // not from the presence of GEMINI_API_KEY. We inject the key at spawn, but
        // without a recorded auth type the `--experimental-acp` server reports
        // AuthRequired ("Failed to Launch: Authentication required"). Pre-select
        // the AI Studio API-key method so the injected key is actually used.
        ensure_gemini_api_key_auth_selected(config_dir)?;
    }
    Ok(())
}

/// Writes `<config_dir>/.gemini/settings.json` selecting AI Studio API-key auth,
/// unless a settings file already exists (the CLI owns it once created). Shared
/// by account creation and the spawn-time backfill for accounts created before
/// this auth type was written.
pub fn ensure_gemini_api_key_auth_selected(config_dir: &Path) -> Result<()> {
    let gemini_dir = config_dir.join(".gemini");
    let settings_path = gemini_dir.join("settings.json");
    if settings_path.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(&gemini_dir)
        .with_context(|| format!("creating Gemini config dir {}", gemini_dir.display()))?;
    std::fs::write(
        &settings_path,
        "{\n  \"security\": {\n    \"auth\": {\n      \"selectedType\": \"gemini-api-key\"\n    }\n  }\n}\n",
    )
    .with_context(|| format!("writing Gemini settings.json at {}", settings_path.display()))
}

/// Creates a new account, materializes its config dir, applies per-agent
/// post-create quirks, and persists it to the registry. The agent must have
/// a known descriptor (Tier A: Claude Code, Gemini, Codex). The account starts
/// in `AccountState::Created` — it has not yet been logged in.
pub fn create_account(agent_id: &str, display_name: &str) -> Result<AiAccount> {
    if descriptor_for(agent_id).is_none() {
        return Err(anyhow!("no descriptor for agent {agent_id}"));
    }

    let mut index = load_index();
    validate_new_account_name(&index, agent_id, display_name)?;

    let account_id = uuid::Uuid::new_v4().to_string();
    let config_dir = accounts_root().join(agent_id).join(&account_id);

    std::fs::create_dir_all(&config_dir)
        .with_context(|| format!("creating account config dir {}", config_dir.display()))?;
    let _ = set_dir_permissions_700(&config_dir);

    apply_create_quirks(agent_id, &config_dir)?;

    let account = AiAccount {
        id: account_id,
        agent_id: agent_id.to_string(),
        display_name: display_name.trim().to_string(),
        config_dir,
        state: AccountState::Created,
        created_at: Some(Utc::now()),
        last_used_at: None,
    };
    index.upsert(account.clone());
    save_index(&index)?;
    Ok(account)
}

/// Touches the `last_used_at` timestamp on an account and persists. Idempotent
/// when the account doesn't exist (returns Ok without writing). Called from
/// the ACP spawn site whenever a thread resolves to a bound account, and from
/// the chip's switch handler when the user explicitly activates an account.
pub fn mark_account_used(account_id: &str) -> Result<()> {
    let mut index = load_index();
    if index.find(account_id).is_none() {
        return Ok(());
    }
    index.touch(account_id)?;
    save_index(&index)
}

/// Removes an account from the registry and returns it. The config dir is
/// deliberately left on disk: a caller that wants it gone should move it to
/// the system trash (see the Manage AI Accounts modal) so the operation stays
/// recoverable, and so accounts that merely reference an external directory
/// (`is_managed_config_dir` is false) can be unregistered without destroying
/// data the user still uses outside Lathe.
///
/// Returns Err if no account with that id exists.
pub fn delete_account(account_id: &str) -> Result<AiAccount> {
    let mut index = load_index();
    let account = index
        .remove(account_id)
        .ok_or_else(|| anyhow!("no account with id {account_id}"))?;
    save_index(&index)?;
    Ok(account)
}

/// Runs the descriptor's `verify_command` with the account's config-dir env
/// var set, returning whether the process exited successfully. A non-zero
/// exit is `Ok(false)` rather than `Err` so callers can flip the account
/// state to `Failed` without distinguishing "verify command crashed" from
/// "verify command said not authenticated." Spawn-time IO errors propagate.
///
/// Note: per-agent verify-command quality varies. Claude Code's `/status`
/// is interactive-only, so `verify_account` for that agent will need to be
/// replaced with a credential-file presence check before v1 ships. Tracked
/// as a per-agent gotcha in the project plan.
/// A single past conversation belonging to an account. Used by the Manage
/// AI Accounts modal to surface per-account history without exposing the
/// raw on-disk files.
#[derive(Clone, Debug)]
pub struct ConversationSummary {
    /// Stable ID for the conversation. For Claude Code, the JSONL filename
    /// (without extension); for Codex, the session JSON filename.
    pub id: String,
    /// First user message (truncated) when available, otherwise a fallback
    /// derived from the project path or file name.
    pub title: String,
    /// For agents that store conversations per-project (Claude Code), the
    /// encoded project path so the user can see where the thread originated.
    pub project_hint: Option<String>,
    /// Filesystem mtime, used for sorting most-recent first.
    pub last_modified: Option<DateTime<Utc>>,
}

/// Outcome of a single `import_from_claude_profiles` invocation.
#[derive(Clone, Debug, Default)]
pub struct ClaudeProfilesImport {
    /// Newly registered AI accounts.
    pub imported: Vec<AiAccount>,
    /// Profile dirs that already had a matching account registered.
    pub skipped_existing: Vec<String>,
    /// Profile dir names that were unreadable (logged for diagnosis).
    pub failed: Vec<String>,
}

/// Resolves the directory used by Chris Paterson's `claude-account-switcher`
/// shell helper. Defaults to `~/.claude-profiles` and respects the same
/// `CLAUDE_PROFILES_DIR` env override that the shell helper uses.
pub fn claude_profiles_dir() -> Option<PathBuf> {
    if let Ok(custom) = std::env::var("CLAUDE_PROFILES_DIR") {
        if !custom.is_empty() {
            return Some(PathBuf::from(custom));
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".claude-profiles"))
}

/// Walks `~/.claude-profiles/<name>/` and registers each existing profile dir
/// as a Claude Code AI account pointing at that directory. The profile dirs
/// are NOT copied — registration is by reference, so the user's existing
/// shell-based workflow keeps working alongside Lathe.
///
/// Idempotent: profile dirs whose path matches an already-registered Claude
/// Code account are reported as `skipped_existing` rather than re-imported.
/// Display name collisions are resolved by appending " (imported)".
pub fn import_from_claude_profiles() -> Result<ClaudeProfilesImport> {
    let mut report = ClaudeProfilesImport::default();
    let Some(root) = claude_profiles_dir() else {
        return Ok(report);
    };
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(error) => return Err(error).context("reading claude-profiles directory"),
    };

    let mut index = load_index();
    let agent_id = CLAUDE_CODE_DESCRIPTOR.agent_id;
    let mut changed = false;

    for entry in entries.flatten() {
        let profile_path = entry.path();
        if !profile_path.is_dir() {
            continue;
        }
        let Some(name) = profile_path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
        else {
            report.failed.push(profile_path.display().to_string());
            continue;
        };
        // Skip dirs that don't look like a Claude Code config dir at all —
        // the shell helper symlinks settings.json from ~/.claude/ into each
        // profile, so the absence of any common indicator is a signal the
        // dir is something else entirely.
        let looks_like_profile = profile_path.join("settings.json").exists()
            || profile_path.join("projects").exists()
            || profile_path.join("sessions").exists();
        if !looks_like_profile {
            continue;
        }

        let already_registered = index
            .for_agent(agent_id)
            .any(|account| account.config_dir == profile_path);
        if already_registered {
            report.skipped_existing.push(name);
            continue;
        }

        let display_name = unique_display_name(&index, agent_id, &name);
        let account = AiAccount {
            id: uuid::Uuid::new_v4().to_string(),
            agent_id: agent_id.to_string(),
            display_name,
            config_dir: profile_path,
            state: AccountState::Authenticated,
            created_at: Some(Utc::now()),
            last_used_at: None,
        };
        report.imported.push(account.clone());
        index.upsert(account);
        changed = true;
    }

    if changed {
        save_index(&index)?;
    }
    Ok(report)
}

fn unique_display_name(index: &AiAccountsIndex, agent_id: &str, base: &str) -> String {
    if validate_new_account_name(index, agent_id, base).is_ok() {
        return base.to_string();
    }
    let candidate = format!("{base} (imported)");
    if validate_new_account_name(index, agent_id, &candidate).is_ok() {
        return candidate;
    }
    // Fall back to a numbered suffix so we never block the import on a name clash.
    for n in 2..1000 {
        let candidate = format!("{base} (imported {n})");
        if validate_new_account_name(index, agent_id, &candidate).is_ok() {
            return candidate;
        }
    }
    format!("{base} (imported {})", uuid::Uuid::new_v4())
}

/// Returns the shell command a user can paste into a terminal to resume
/// the given past conversation. Per-agent command shapes:
///   * Claude Code: `claude --resume <id>`
///   * Codex CLI:   `codex exec resume <id>`
///   * Gemini CLI:  `gemini --resume <id>` (must run from the original
///                  project's cwd — Gemini scopes sessions by project hash)
/// Returns `None` for agents whose resume CLI isn't known.
pub fn resume_command(agent_id: &str, conversation_id: &str) -> Option<String> {
    match agent_id {
        "claude-acp" => Some(format!("claude --resume {conversation_id}")),
        "codex-acp" => Some(format!("codex exec resume {conversation_id}")),
        "gemini" => Some(format!("gemini --resume {conversation_id}")),
        _ => None,
    }
}

/// Lists conversations stored on disk in the given account's config dir.
/// Returns an empty `Vec` for agents whose history format hasn't been
/// implemented yet — currently only Claude Code is parsed; Codex and Gemini
/// are stubbed and return empty.
pub fn list_conversations(account: &AiAccount) -> Vec<ConversationSummary> {
    match account.agent_id.as_str() {
        "claude-acp" => list_claude_conversations(&account.config_dir),
        "codex-acp" => list_codex_conversations(&account.config_dir),
        "gemini" => list_gemini_conversations(&account.config_dir),
        _ => Vec::new(),
    }
}

fn list_claude_conversations(config_dir: &Path) -> Vec<ConversationSummary> {
    let projects_dir = config_dir.join("projects");
    let Ok(project_entries) = std::fs::read_dir(&projects_dir) else {
        return Vec::new();
    };

    let mut summaries: Vec<ConversationSummary> = Vec::new();
    for project_entry in project_entries.flatten() {
        let project_path = project_entry.path();
        if !project_path.is_dir() {
            continue;
        }
        let project_hint = project_entry.file_name().to_string_lossy().into_owned();

        let Ok(session_entries) = std::fs::read_dir(&project_path) else {
            continue;
        };
        for session_entry in session_entries.flatten() {
            let session_path = session_entry.path();
            if session_path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let id = session_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let last_modified = session_entry
                .metadata()
                .ok()
                .and_then(|meta| meta.modified().ok())
                .map(DateTime::<Utc>::from);
            let title = claude_first_user_message(&session_path).unwrap_or_else(|| id.clone());
            summaries.push(ConversationSummary {
                id,
                title,
                project_hint: Some(project_hint.clone()),
                last_modified,
            });
        }
    }

    summaries.sort_by_key(|s| std::cmp::Reverse(s.last_modified));
    summaries
}

/// Reads the first line of a Claude Code JSONL session file and extracts the
/// first user message text, truncated for display. Returns `None` on any
/// parse failure — caller falls back to the session ID.
fn claude_first_user_message(path: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines().take(20).flatten() {
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if value.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        let content = value
            .get("message")
            .and_then(|m| m.get("content"))
            .or_else(|| value.get("content"))?;
        let Some(text) = extract_text_from_content(content) else {
            continue;
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        return Some(truncate_title(trimmed, 80));
    }
    None
}

/// Walks `<CODEX_HOME>/sessions/` (date-partitioned by year/month/day in
/// recent Codex versions, flat in older ones) for `rollout-*.jsonl` files.
/// Each file is one conversation; the first line is a `session_meta`
/// record carrying the conversation `id`, `cwd`, and start `timestamp`.
fn list_codex_conversations(config_dir: &Path) -> Vec<ConversationSummary> {
    let sessions_dir = config_dir.join("sessions");
    let mut summaries: Vec<ConversationSummary> = Vec::new();
    walk_jsonl_files(&sessions_dir, &mut |path| {
        if let Some(summary) = parse_codex_session(path) {
            summaries.push(summary);
        }
    });
    summaries.sort_by_key(|s| std::cmp::Reverse(s.last_modified));
    summaries
}

/// Recursively walks `dir` and invokes `found` for each `.jsonl` file. Bails
/// silently on any IO error so missing or unreadable subdirectories don't
/// poison the rest of the walk.
fn walk_jsonl_files(dir: &Path, found: &mut dyn FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_jsonl_files(&path, found);
        } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            found(&path);
        }
    }
}

fn parse_codex_session(path: &Path) -> Option<ConversationSummary> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path).ok()?;
    let mut lines = BufReader::new(file).lines();

    let meta_line = lines.next()?.ok()?;
    let meta: serde_json::Value = serde_json::from_str(&meta_line).ok()?;

    // RolloutLine is `{ timestamp, type, payload }` after serde-flatten.
    // The first line must be session_meta or this isn't a Codex rollout we recognize.
    if meta.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
        return None;
    }
    let payload = meta.get("payload")?;
    let session_id = payload
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            // Filename fallback: `rollout-<timestamp>-<uuid>.jsonl`.
            path.file_stem()
                .and_then(|s| s.to_str())
                .and_then(extract_uuid_suffix)
        })?;
    let project_hint = payload
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let timestamp = meta
        .get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|dt| dt.with_timezone(&Utc));

    let title = first_codex_user_text(&mut lines).unwrap_or_else(|| session_id.clone());

    Some(ConversationSummary {
        id: session_id,
        title,
        project_hint,
        last_modified: timestamp,
    })
}

fn first_codex_user_text<I>(lines: &mut I) -> Option<String>
where
    I: Iterator<Item = std::io::Result<String>>,
{
    for line in lines.take(50) {
        let Ok(line) = line else { continue };
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if value.get("type").and_then(|t| t.as_str()) != Some("response_item") {
            continue;
        }
        let payload = match value.get("payload") {
            Some(p) => p,
            None => continue,
        };
        if payload.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        let text = payload
            .get("content")
            .and_then(extract_text_from_content)
            .or_else(|| {
                payload
                    .get("text")
                    .and_then(|t| t.as_str())
                    .map(str::to_string)
            });
        if let Some(text) = text {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(truncate_title(trimmed, 80));
            }
        }
    }
    None
}

/// Extracts text from a content field that may be a string or an array of
/// `{ "type": "input_text", "text": "..." }` parts. Used by both Claude and
/// Codex parsers.
fn extract_text_from_content(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(parts) => {
            let joined: String = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join(" ");
            if joined.is_empty() {
                None
            } else {
                Some(joined)
            }
        }
        _ => None,
    }
}

/// Walks `<config_dir>/tmp/<project-id>/chats/` for `session-*.json` files.
/// Despite the `.json` extension, these are JSONL — one record per line, with
/// the first line carrying `type: "metadata"` and subsequent `type: "user"`
/// or `type: "model"` records. Auto-saved by ChatRecordingService on every
/// turn (not just `/chat save`), so this captures the full session log.
fn list_gemini_conversations(config_dir: &Path) -> Vec<ConversationSummary> {
    // GEMINI_CLI_HOME makes the CLI store everything under `<config_dir>/.gemini`,
    // so the per-project chat logs live at `<config_dir>/.gemini/tmp/...`.
    let tmp_dir = config_dir.join(".gemini").join("tmp");
    let mut summaries: Vec<ConversationSummary> = Vec::new();
    let Ok(project_entries) = std::fs::read_dir(&tmp_dir) else {
        return Vec::new();
    };

    for project_entry in project_entries.flatten() {
        let project_path = project_entry.path();
        if !project_path.is_dir() {
            continue;
        }
        let project_hint = project_entry.file_name().to_string_lossy().into_owned();
        let chats_dir = project_path.join("chats");
        let Ok(session_entries) = std::fs::read_dir(&chats_dir) else {
            continue;
        };
        for session_entry in session_entries.flatten() {
            let session_path = session_entry.path();
            let stem = match session_path.file_stem().and_then(|s| s.to_str()) {
                Some(stem) if stem.starts_with("session-") => stem,
                _ => continue,
            };
            if session_path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if let Some(summary) = parse_gemini_session(&session_path, stem, &project_hint) {
                summaries.push(summary);
            }
        }
    }

    summaries.sort_by_key(|s| std::cmp::Reverse(s.last_modified));
    summaries
}

fn parse_gemini_session(
    path: &Path,
    stem: &str,
    project_hint: &str,
) -> Option<ConversationSummary> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path).ok()?;
    let mut lines = BufReader::new(file).lines();

    let meta_line = lines.next()?.ok()?;
    let meta: serde_json::Value = serde_json::from_str(&meta_line).ok()?;

    // Defensive — bail if the first line isn't a metadata record. Shape can
    // evolve, so we don't assume too much beyond the type tag.
    if meta.get("type").and_then(|t| t.as_str()) != Some("metadata") {
        return None;
    }

    let session_id = stem.trim_start_matches("session-").to_string();
    let timestamp = meta
        .get("startTime")
        .and_then(|t| t.as_str())
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|dt| dt.with_timezone(&Utc));

    let title = first_gemini_user_text(&mut lines).unwrap_or_else(|| session_id.clone());

    Some(ConversationSummary {
        id: session_id,
        title,
        project_hint: Some(project_hint.to_string()),
        last_modified: timestamp,
    })
}

fn first_gemini_user_text<I>(lines: &mut I) -> Option<String>
where
    I: Iterator<Item = std::io::Result<String>>,
{
    for line in lines.take(50) {
        let Ok(line) = line else { continue };
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if value.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        let content = match value.get("content") {
            Some(c) => c,
            None => continue,
        };
        if let Some(text) = extract_text_from_content(content) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(truncate_title(trimmed, 80));
            }
        }
    }
    None
}

/// Pulls the trailing UUID-like segment off `rollout-<timestamp>-<uuid>`.
/// Codex timestamps embed hyphens (date AND time-with-hyphen-separators), so
/// the simple "split on last hyphen" trick doesn't work — we look for the
/// last 5 hyphen-separated groups (the canonical UUID shape) instead.
fn extract_uuid_suffix(stem: &str) -> Option<String> {
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() < 5 {
        return None;
    }
    Some(parts[parts.len() - 5..].join("-"))
}

fn truncate_title(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let head: String = (&mut chars).take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

pub async fn verify_account(account: &AiAccount) -> Result<bool> {
    let descriptor = descriptor_for(&account.agent_id)
        .ok_or_else(|| anyhow!("no descriptor for agent {}", account.agent_id))?;
    let mut iter = descriptor.verify_command.iter().copied();
    let program = iter
        .next()
        .ok_or_else(|| anyhow!("verify_command is empty for agent {}", account.agent_id))?;
    let args: Vec<&'static str> = iter.collect();
    let status = smol::process::Command::new(program)
        .args(&args)
        .env(descriptor.config_dir_env_var, &account.config_dir)
        .stdout(smol::process::Stdio::null())
        .stderr(smol::process::Stdio::null())
        .status()
        .await
        .with_context(|| format!("spawning verify command for agent {}", account.agent_id))?;
    Ok(status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(agent: &str, name: &str) -> AiAccount {
        AiAccount::new(agent, name, PathBuf::from(format!("/tmp/{agent}/{name}")))
    }

    #[test]
    fn upsert_replaces_existing_id() {
        let mut index = AiAccountsIndex::default();
        let mut a = account("claude-acp", "personal");
        index.upsert(a.clone());
        assert_eq!(index.accounts.len(), 1);

        a.display_name = "renamed".into();
        index.upsert(a);
        assert_eq!(index.accounts.len(), 1);
        assert_eq!(index.accounts[0].display_name, "renamed");
    }

    #[test]
    fn remove_clears_global_defaults_pointing_at_id() {
        let mut index = AiAccountsIndex::default();
        let a = account("claude-acp", "personal");
        let id = a.id.clone();
        index.upsert(a);
        index
            .set_default("claude-acp", Some(id.clone()))
            .expect("set default");

        assert_eq!(index.global_defaults.get("claude-acp"), Some(&id));
        index.remove(&id);
        assert!(!index.global_defaults.contains_key("claude-acp"));
    }

    #[test]
    fn is_managed_config_dir_only_matches_lathe_owned_dirs() {
        let managed = accounts_root().join("claude-acp").join("account-id");
        assert!(is_managed_config_dir(&managed));
        // Imported profiles are registered by reference and stay the user's.
        assert!(!is_managed_config_dir(Path::new(
            "/Users/someone/.claude-profiles/personal"
        )));
        assert!(!is_managed_config_dir(Path::new("/Users/someone/.claude")));
    }

    #[test]
    fn default_for_agent_uses_explicit_first() {
        let mut index = AiAccountsIndex::default();
        let a = account("claude-acp", "personal");
        let b = account("claude-acp", "work");
        let a_id = a.id.clone();
        let b_id = b.id.clone();
        index.upsert(a);
        index.upsert(b);
        index
            .set_default("claude-acp", Some(b_id.clone()))
            .expect("set default");
        assert_eq!(
            index.default_for_agent("claude-acp").map(|a| &a.id),
            Some(&b_id)
        );
        let _ = a_id;
    }

    #[test]
    fn default_for_agent_implicit_single_account() {
        let mut index = AiAccountsIndex::default();
        let a = account("gemini", "only");
        let id = a.id.clone();
        index.upsert(a);
        assert_eq!(index.default_for_agent("gemini").map(|a| &a.id), Some(&id));
    }

    #[test]
    fn default_for_agent_none_when_two_without_explicit() {
        let mut index = AiAccountsIndex::default();
        index.upsert(account("codex-acp", "personal"));
        index.upsert(account("codex-acp", "work"));
        assert!(index.default_for_agent("codex-acp").is_none());
    }

    #[test]
    fn set_default_rejects_account_belonging_to_different_agent() {
        let mut index = AiAccountsIndex::default();
        let a = account("claude-acp", "personal");
        let id = a.id.clone();
        index.upsert(a);
        let result = index.set_default("gemini", Some(id));
        assert!(result.is_err());
    }

    #[test]
    fn set_default_rejects_unknown_id() {
        let mut index = AiAccountsIndex::default();
        let result = index.set_default("claude-acp", Some("nope".into()));
        assert!(result.is_err());
    }

    #[test]
    fn json_round_trip() {
        let mut index = AiAccountsIndex::default();
        index.upsert(account("claude-acp", "personal"));
        index.upsert(account("gemini", "default"));
        let bytes = serde_json::to_vec_pretty(&index).expect("serialize");
        let parsed: AiAccountsIndex = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(parsed.accounts.len(), 2);
        assert_eq!(parsed.accounts[0].agent_id, "claude-acp");
    }

    #[test]
    fn descriptor_lookup() {
        assert_eq!(
            descriptor_for("claude-acp").map(|d| d.display_name),
            Some("Claude Agent")
        );
        assert_eq!(
            descriptor_for("gemini").map(|d| d.display_name),
            Some("Gemini CLI")
        );
        assert_eq!(
            descriptor_for("codex-acp").map(|d| d.display_name),
            Some("Codex CLI")
        );
        assert!(descriptor_for("nope").is_none());
    }

    #[test]
    fn gemini_uses_api_key_login_with_https_key_url() {
        let gemini = descriptor_for("gemini").expect("gemini descriptor");
        match gemini.login_flow {
            LoginFlow::ApiKey { key_url, env_var } => {
                assert!(key_url.starts_with("https://"), "key_url must be https");
                assert_eq!(env_var, "GEMINI_API_KEY");
            }
            other => panic!("expected Gemini to use ApiKey login flow, got {other:?}"),
        }
    }

    #[test]
    fn api_key_agents_do_not_scrub_their_own_key() {
        // Scrubbing is an OAuth-only protection. An ApiKey agent that stripped
        // its own key var would defeat the auth it depends on (this is the
        // exact conflict that made "add a Gemini account" break before).
        for descriptor in TIER_A_DESCRIPTORS {
            if let LoginFlow::ApiKey { env_var, .. } = descriptor.login_flow {
                assert!(
                    !descriptor.scrub_env.contains(&env_var),
                    "{} must not scrub its own API-key var {env_var}",
                    descriptor.agent_id
                );
            }
        }
    }

    #[test]
    fn api_key_keychain_url_is_scoped_per_account() {
        let a = api_key_keychain_url("gemini", "acct-1");
        let b = api_key_keychain_url("gemini", "acct-2");
        let c = api_key_keychain_url("codex-acp", "acct-1");
        assert_ne!(a, b, "different accounts must not collide");
        assert_ne!(a, c, "different agents must not collide");
        assert!(a.contains("gemini") && a.contains("acct-1"));
    }

    #[test]
    fn validate_rejects_empty_and_whitespace_names() {
        let index = AiAccountsIndex::default();
        assert!(validate_new_account_name(&index, "claude-acp", "").is_err());
        assert!(validate_new_account_name(&index, "claude-acp", "   ").is_err());
    }

    #[test]
    fn validate_rejects_case_insensitive_collision_within_agent() {
        let mut index = AiAccountsIndex::default();
        index.upsert(account("claude-acp", "Personal"));
        assert!(validate_new_account_name(&index, "claude-acp", "personal").is_err());
        assert!(validate_new_account_name(&index, "claude-acp", "PERSONAL").is_err());
    }

    #[test]
    fn validate_allows_same_name_across_different_agents() {
        let mut index = AiAccountsIndex::default();
        index.upsert(account("claude-acp", "personal"));
        // Same display name is fine on a different agent.
        assert!(validate_new_account_name(&index, "gemini", "personal").is_ok());
    }

    #[test]
    fn validate_trims_whitespace_before_collision_check() {
        let mut index = AiAccountsIndex::default();
        index.upsert(account("codex-acp", "work"));
        assert!(validate_new_account_name(&index, "codex-acp", "  work  ").is_err());
    }

    #[test]
    fn truncate_title_keeps_short_strings_intact() {
        assert_eq!(truncate_title("hello", 10), "hello");
    }

    #[test]
    fn truncate_title_appends_ellipsis_when_cut() {
        assert_eq!(truncate_title("abcdefghij", 5), "abcde…");
    }

    #[test]
    fn truncate_title_counts_chars_not_bytes() {
        // Multi-byte characters: each "é" is 2 bytes but 1 char. Truncation
        // is char-aware so we don't slice through a multi-byte boundary.
        assert_eq!(truncate_title("éééééé", 3), "ééé…");
    }

    #[test]
    fn list_conversations_returns_empty_for_unimplemented_agents() {
        let codex = AiAccount::new("codex-acp", "work", PathBuf::from("/nonexistent"));
        assert!(list_conversations(&codex).is_empty());
        let gemini = AiAccount::new("gemini", "default", PathBuf::from("/nonexistent"));
        assert!(list_conversations(&gemini).is_empty());
        let unknown = AiAccount::new("nope", "x", PathBuf::from("/nonexistent"));
        assert!(list_conversations(&unknown).is_empty());
    }

    #[test]
    fn list_conversations_returns_empty_when_projects_dir_missing() {
        // Claude Code accounts whose config dir doesn't exist (or doesn't
        // contain a `projects/` subdir) should produce zero conversations
        // rather than an error.
        let claude = AiAccount::new("claude-acp", "personal", PathBuf::from("/nonexistent"));
        assert!(list_conversations(&claude).is_empty());
    }

    #[test]
    fn unique_display_name_returns_input_when_unique() {
        let index = AiAccountsIndex::default();
        assert_eq!(
            unique_display_name(&index, "claude-acp", "personal"),
            "personal"
        );
    }

    #[test]
    fn unique_display_name_appends_imported_suffix_on_collision() {
        let mut index = AiAccountsIndex::default();
        index.upsert(account("claude-acp", "personal"));
        assert_eq!(
            unique_display_name(&index, "claude-acp", "personal"),
            "personal (imported)"
        );
    }

    #[test]
    fn unique_display_name_numbers_subsequent_imports() {
        let mut index = AiAccountsIndex::default();
        index.upsert(account("claude-acp", "personal"));
        index.upsert(account("claude-acp", "personal (imported)"));
        assert_eq!(
            unique_display_name(&index, "claude-acp", "personal"),
            "personal (imported 2)"
        );
    }

    #[test]
    fn extract_uuid_suffix_pulls_trailing_uuid() {
        // Codex filename shape: `rollout-<timestamp>-<uuid>`
        // where timestamp itself contains hyphens (date + hyphen-separated time).
        assert_eq!(
            extract_uuid_suffix("rollout-2025-05-07T17-24-21-5973b6c0-94b8-487b-a530-2aeb6098ae0e"),
            Some("5973b6c0-94b8-487b-a530-2aeb6098ae0e".to_string())
        );
    }

    #[test]
    fn extract_uuid_suffix_returns_none_for_short_input() {
        assert!(extract_uuid_suffix("not-enough").is_none());
    }

    #[test]
    fn extract_text_from_content_handles_string() {
        let v = serde_json::Value::String("hello world".into());
        assert_eq!(extract_text_from_content(&v), Some("hello world".into()));
    }

    #[test]
    fn extract_text_from_content_joins_array_parts() {
        let v: serde_json::Value = serde_json::json!([
            {"type": "input_text", "text": "hello"},
            {"type": "input_text", "text": "world"},
        ]);
        assert_eq!(extract_text_from_content(&v), Some("hello world".into()));
    }

    #[test]
    fn extract_text_from_content_returns_none_when_array_has_no_text() {
        let v: serde_json::Value = serde_json::json!([
            {"type": "image_ref", "url": "..."},
        ]);
        assert!(extract_text_from_content(&v).is_none());
    }

    #[test]
    fn list_codex_conversations_returns_empty_when_sessions_dir_missing() {
        let codex = AiAccount::new("codex-acp", "work", PathBuf::from("/nonexistent"));
        assert!(list_conversations(&codex).is_empty());
    }

    #[test]
    fn list_gemini_conversations_returns_empty_when_tmp_dir_missing() {
        let gemini = AiAccount::new("gemini", "default", PathBuf::from("/nonexistent"));
        assert!(list_conversations(&gemini).is_empty());
    }

    #[test]
    fn resume_command_uses_correct_per_agent_shape() {
        assert_eq!(
            resume_command("claude-acp", "abc-123"),
            Some("claude --resume abc-123".into())
        );
        assert_eq!(
            resume_command("codex-acp", "abc-123"),
            Some("codex exec resume abc-123".into())
        );
        assert_eq!(
            resume_command("gemini", "abc-123"),
            Some("gemini --resume abc-123".into())
        );
    }

    #[test]
    fn resume_command_returns_none_for_unknown_agent() {
        assert!(resume_command("nope", "abc-123").is_none());
    }
}
