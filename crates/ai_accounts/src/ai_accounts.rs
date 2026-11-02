use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use collections::HashMap;
use gpui::{App, EntityId, Global, SharedString};
use serde::{Deserialize, Serialize};
use settings::{
    AiAccountLoginContent, AiAccountProfileContent, RegisterSetting, Settings, SettingsContent,
};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

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
    paths::account_indexes_dir().join("ai_accounts.json")
}

/// The parsed index, keyed by the file's modification time. The agent panel's
/// account chip reads the index on every render (continuously while a reply
/// streams), so this turns a read and JSON parse into a `stat` while still
/// picking up edits made by hand or by other Lathe instances.
static INDEX_CACHE: Mutex<Option<(SystemTime, AiAccountsIndex)>> = Mutex::new(None);

fn index_cache() -> MutexGuard<'static, Option<(SystemTime, AiAccountsIndex)>> {
    INDEX_CACHE.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn load_index() -> AiAccountsIndex {
    let path = accounts_path();
    let modified = std::fs::metadata(&path)
        .and_then(|metadata| metadata.modified())
        .ok();
    if let Some(modified) = modified
        && let Some((cached_modified, index)) = index_cache().as_ref()
        && *cached_modified == modified
    {
        return index.clone();
    }
    let index = read_index(&path);
    *index_cache() = modified.map(|modified| (modified, index.clone()));
    index
}

fn read_index(path: &Path) -> AiAccountsIndex {
    let bytes = match std::fs::read(path) {
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
    *index_cache() = None;
    paths::write_account_index(&path, &bytes).context("write ai_accounts.json")
}

/// How a new account signs in.
#[derive(Clone, Debug, PartialEq)]
pub enum LoginFlow {
    /// Open an ACP thread bound to the new, still empty profile. The agent
    /// reports that authentication is required, and the thread offers the
    /// authentication methods the agent advertises over ACP.
    AgentAuth,
    /// Open a regular terminal pane and run the agent's login command interactively.
    EmbeddedTerminal { argv: Vec<SharedString> },
}

/// A provider API key the user can paste when adding an account, instead of
/// going through the agent's sign-in. Lathe stores the key in the system
/// keychain scoped to the account (see [`api_key_keychain_url`]) and injects
/// it into the subprocess at spawn.
#[derive(Clone, Debug, PartialEq)]
pub struct ApiKeyOption {
    /// Where the user obtains a key. Opened in the browser from the modal.
    pub key_url: SharedString,
    /// Subprocess environment variable the key is injected as (e.g.
    /// `GEMINI_API_KEY`).
    pub env_var: SharedString,
    /// Run on the profile dir when the account is created with a key, so the
    /// agent uses the key rather than asking to sign in.
    pub setup: ProfileSetup,
}

/// How the Manage AI Accounts modal checks that an account is signed in.
#[derive(Clone, Debug, PartialEq)]
pub enum VerifyStrategy {
    /// Run this command with the profile env var set. A zero exit means the
    /// account is signed in.
    Command(Vec<SharedString>),
    /// A key stored in the keychain counts as signed in. Without one, the
    /// agent checks its own sign-in when a thread connects.
    StoredApiKey,
    /// There is no offline check. The agent reports missing credentials when a
    /// thread connects to it.
    OnConnect,
}

/// Agent-specific preparation of a freshly created profile directory, for
/// CLIs that would otherwise escape the profile or fail to start in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileSetup {
    None,
    /// Without `cli_auth_credentials_store = "file"`, Codex stores OAuth tokens
    /// in the OS keyring, bypassing `CODEX_HOME` and breaking per-account
    /// isolation.
    CodexFileCredentials,
    /// The Gemini CLI chooses its auth method from
    /// `<GEMINI_CLI_HOME>/.gemini/settings.json` (`security.auth.selectedType`),
    /// not from the presence of GEMINI_API_KEY. We inject the key at spawn, but
    /// without a recorded auth type the ACP server reports AuthRequired and
    /// offers its sign-in methods instead.
    GeminiApiKeyAuth,
}

/// Where an agent keeps conversation transcripts inside its profile dir, so
/// switching accounts can copy the open conversation into the new profile and
/// reload it there. Each layout is an adapter for one agent's on-disk format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptLayout {
    /// The layout is unknown, so a switch can't carry the open conversation
    /// and only applies to threads started afterwards. Gemini keys its chat
    /// files by project hash rather than session id, so it stays here too.
    None,
    /// `projects/<encoded cwd>/<session id>.jsonl` plus a sidecar dir.
    ClaudeProjects,
    /// `sessions/**/rollout-<timestamp>-<session id>.jsonl`.
    CodexSessions,
}

/// The contract that lets Lathe keep several accounts for one agent apart: an
/// environment variable that relocates all of the agent's credentials and
/// state, plus how to sign in to and verify a profile.
#[derive(Clone, Debug, PartialEq)]
pub struct ProfileCapability {
    /// Set to the account's config dir in the agent's child process.
    pub profile_env_var: SharedString,
    pub login_flow: LoginFlow,
    pub verify: VerifyStrategy,
    /// Default environment variables injected at ACP spawn time, in addition
    /// to `profile_env_var`. These are protective defaults (e.g. disabling
    /// cloud-managed MCP servers that would otherwise block thread startup
    /// waiting for OAuth). Always applied with entry-or-insert semantics so an
    /// explicit value upstream of the spawn (workspace settings, user shell
    /// env) wins.
    pub default_env: Vec<(SharedString, SharedString)>,
    /// Environment variables to *remove* from the agent subprocess (and its
    /// login terminal) when a Lathe AI account is bound. These are API-key
    /// vars that would otherwise override the per-account subscription/OAuth
    /// login. Because the subprocess inherits Lathe's own environment by
    /// default, omitting the key from the injected env map is not enough; it
    /// must be explicitly removed.
    pub scrub_env: Vec<SharedString>,
    /// Set alongside `profile_env_var` whenever an account is bound, overriding
    /// any configured value, because isolation depends on them.
    pub bound_env: Vec<(SharedString, SharedString)>,
    pub setup: ProfileSetup,
    pub transcripts: TranscriptLayout,
    pub api_key: Option<ApiKeyOption>,
}

/// Hex-coded per-agent identification color, with light- and dark-theme
/// variants. Applied sparingly (chip dot, modal section dividers, primary
/// CTA tint), never as a panel-wide skin. Values are muted from each agent's
/// full-saturation marketing color to keep contrast acceptable in both themes
/// and to avoid trademarked-asset replication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BrandAccent {
    pub light: &'static str,
    pub dark: &'static str,
}

/// An agent Lathe can keep multiple isolated accounts for.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentDescriptor {
    pub agent_id: SharedString,
    pub display_name: SharedString,
    pub signup_url: Option<SharedString>,
    /// Only built-in agents have one; others render with a neutral dot.
    pub brand_accent: Option<BrandAccent>,
    pub profile: ProfileCapability,
}

pub const CLAUDE_CODE_AGENT_ID: &str = "claude-acp";
pub const GEMINI_AGENT_ID: &str = "gemini";
pub const CODEX_AGENT_ID: &str = "codex-acp";

/// Agents with a profile contract built into Lathe, in display order. They are
/// always listed in the account UI, installed or not.
pub const BUILTIN_AGENT_IDS: &[&str] = &[CLAUDE_CODE_AGENT_ID, GEMINI_AGENT_ID, CODEX_AGENT_ID];

pub fn builtin_descriptor(agent_id: &str) -> Option<AgentDescriptor> {
    match agent_id {
        CLAUDE_CODE_AGENT_ID => Some(AgentDescriptor {
            agent_id: CLAUDE_CODE_AGENT_ID.into(),
            display_name: "Claude Agent".into(),
            signup_url: Some("https://claude.ai/upgrade".into()),
            brand_accent: Some(BrandAccent {
                light: "#cc6633",
                dark: "#e89968",
            }),
            profile: ProfileCapability {
                profile_env_var: "CLAUDE_CONFIG_DIR".into(),
                login_flow: LoginFlow::AgentAuth,
                verify: VerifyStrategy::Command(vec!["claude".into(), "/status".into()]),
                // Default-off the claude.ai cloud connectors (Gmail/Calendar/Drive
                // that come down from a Claude Max account) so a user who hasn't
                // OAuth'd them doesn't get a hung ACP thread on first launch. Opt
                // in by setting ENABLE_CLAUDEAI_MCP_SERVERS=true in workspace
                // `.zed/settings.json` under `agent_servers.claude-acp.env`, or in
                // the user's shell env.
                //
                // MCP_TIMEOUT is a belt-and-braces 5s cap so any other slow MCP
                // server can't block thread spawn either; Claude Code gives up
                // after the timeout and proceeds without that server's tools.
                default_env: vec![
                    ("ENABLE_CLAUDEAI_MCP_SERVERS".into(), "false".into()),
                    ("MCP_TIMEOUT".into(), "5000".into()),
                ],
                scrub_env: Vec::new(),
                setup: ProfileSetup::None,
                bound_env: Vec::new(),
                transcripts: TranscriptLayout::ClaudeProjects,
                api_key: None,
            },
        }),
        GEMINI_AGENT_ID => Some(AgentDescriptor {
            agent_id: GEMINI_AGENT_ID.into(),
            display_name: "Gemini CLI".into(),
            signup_url: Some("https://aistudio.google.com/apikey".into()),
            brand_accent: Some(BrandAccent {
                light: "#4285f4",
                dark: "#7aa9f8",
            }),
            profile: ProfileCapability {
                // The Gemini CLI relocates its config/credential storage via
                // GEMINI_CLI_HOME (it creates a `.gemini` folder under this
                // root). GEMINI_CONFIG_DIR is NOT honored, so using it left
                // every account sharing ~/.gemini.
                profile_env_var: "GEMINI_CLI_HOME".into(),
                // The CLI advertises its own sign-in methods over ACP (Google
                // login, Gemini API key, Vertex AI, gateway), so a new account
                // signs in through them in a thread.
                login_flow: LoginFlow::AgentAuth,
                verify: VerifyStrategy::StoredApiKey,
                default_env: Vec::new(),
                // Nothing to scrub: an account added with a key needs the
                // injected GEMINI_API_KEY to reach the subprocess.
                scrub_env: Vec::new(),
                // On macOS the CLI keeps Google login tokens in one fixed
                // keychain entry (`gemini-cli-oauth` / `main-account`) that
                // ignores GEMINI_CLI_HOME, so every account would share one
                // login. Forcing file storage puts them in
                // `<GEMINI_CLI_HOME>/.gemini/gemini-credentials.json` instead.
                bound_env: vec![("GEMINI_FORCE_FILE_STORAGE".into(), "true".into())],
                setup: ProfileSetup::None,
                transcripts: TranscriptLayout::None,
                api_key: Some(ApiKeyOption {
                    key_url: "https://aistudio.google.com/apikey".into(),
                    env_var: "GEMINI_API_KEY".into(),
                    setup: ProfileSetup::GeminiApiKeyAuth,
                }),
            },
        }),
        CODEX_AGENT_ID => Some(AgentDescriptor {
            agent_id: CODEX_AGENT_ID.into(),
            display_name: "Codex CLI".into(),
            signup_url: Some("https://chatgpt.com/#pricing".into()),
            brand_accent: Some(BrandAccent {
                light: "#10a37f",
                dark: "#2dc59a",
            }),
            profile: ProfileCapability {
                profile_env_var: "CODEX_HOME".into(),
                login_flow: LoginFlow::EmbeddedTerminal {
                    argv: vec!["codex".into(), "login".into()],
                },
                verify: VerifyStrategy::Command(vec![
                    "codex".into(),
                    "login".into(),
                    "status".into(),
                ]),
                default_env: Vec::new(),
                scrub_env: Vec::new(),
                setup: ProfileSetup::CodexFileCredentials,
                bound_env: Vec::new(),
                transcripts: TranscriptLayout::CodexSessions,
                api_key: None,
            },
        }),
        _ => None,
    }
}

/// Environment variables a settings-defined profile may not claim. Pointing
/// them at a profile dir would change far more than the agent's credentials
/// (the shell, git config, tool lookup), which is exactly the guesswork about
/// unknown agents the profile contract exists to avoid.
const RESERVED_PROFILE_ENV_VARS: &[&str] = &["HOME", "USERPROFILE", "PATH"];

/// A profile contract declared in the user's `ai_account_profiles` setting,
/// for an agent Lathe has no built-in descriptor for.
#[derive(Clone, Debug, PartialEq)]
pub struct ProfileOverride {
    pub profile: ProfileCapability,
    pub signup_url: Option<SharedString>,
}

impl ProfileOverride {
    fn from_content(agent_id: &str, content: &AiAccountProfileContent) -> Option<Self> {
        let profile_env_var = content
            .profile_env_var
            .as_deref()
            .map(str::trim)
            .unwrap_or_default();
        if profile_env_var.is_empty() {
            log::warn!(
                "ai_accounts: ignoring ai_account_profiles.{agent_id}: profile_env_var is required"
            );
            return None;
        }
        if RESERVED_PROFILE_ENV_VARS
            .iter()
            .any(|reserved| reserved.eq_ignore_ascii_case(profile_env_var))
        {
            log::warn!(
                "ai_accounts: ignoring ai_account_profiles.{agent_id}: {profile_env_var} \
                 can't be used as a profile directory variable"
            );
            return None;
        }
        let login_flow = match &content.login {
            None | Some(AiAccountLoginContent::Agent) => LoginFlow::AgentAuth,
            Some(AiAccountLoginContent::Terminal(argv)) if !argv.is_empty() => {
                LoginFlow::EmbeddedTerminal {
                    argv: argv.iter().cloned().map(SharedString::from).collect(),
                }
            }
            Some(AiAccountLoginContent::Terminal(_)) => {
                log::warn!(
                    "ai_accounts: ignoring ai_account_profiles.{agent_id}: login terminal command is empty"
                );
                return None;
            }
        };
        let verify = match content.verify_command.as_deref() {
            Some(argv) if !argv.is_empty() => {
                VerifyStrategy::Command(argv.iter().cloned().map(SharedString::from).collect())
            }
            _ => VerifyStrategy::OnConnect,
        };
        let mut default_env: Vec<(SharedString, SharedString)> = content
            .env
            .iter()
            .flatten()
            .map(|(key, value)| (key.clone().into(), value.clone().into()))
            .collect();
        default_env.sort();
        Some(Self {
            profile: ProfileCapability {
                profile_env_var: profile_env_var.to_string().into(),
                login_flow,
                verify,
                default_env,
                scrub_env: content
                    .scrub_env
                    .iter()
                    .flatten()
                    .cloned()
                    .map(SharedString::from)
                    .collect(),
                bound_env: Vec::new(),
                setup: ProfileSetup::None,
                transcripts: TranscriptLayout::None,
                api_key: None,
            },
            signup_url: content.signup_url.clone().map(SharedString::from),
        })
    }
}

#[derive(Clone, Debug, Default, RegisterSetting)]
pub struct AiAccountsSettings {
    /// Workspace-merged binding map: agent ID → account ID. Workspace `.zed/settings.json`
    /// overrides win over user-level settings (per `MergeFrom`).
    pub bindings: HashMap<String, String>,
    /// Profile contracts from the `ai_account_profiles` setting, keyed by agent
    /// ID. Entries for built-in agents are ignored: their contract includes
    /// setup steps a settings entry can't express.
    pub profile_overrides: HashMap<String, ProfileOverride>,
}

impl Settings for AiAccountsSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let profile_overrides = content
            .ai_account_profiles
            .iter()
            .flatten()
            .filter(|(agent_id, _)| builtin_descriptor(agent_id).is_none())
            .filter_map(|(agent_id, profile)| {
                Some((
                    agent_id.clone(),
                    ProfileOverride::from_content(agent_id, profile)?,
                ))
            })
            .collect();
        Self {
            bindings: content
                .ai_accounts
                .as_ref()
                .map(|map| map.0.clone())
                .unwrap_or_default(),
            profile_overrides,
        }
    }
}

impl AiAccountsSettings {
    /// The profile contract for `agent_id`: built in, then from settings. An
    /// agent with neither cannot hold isolated accounts. Settings-defined
    /// agents are named by their ID here; [`build_catalog`] substitutes the
    /// installed agent's display name.
    pub fn descriptor_for(&self, agent_id: &str) -> Option<AgentDescriptor> {
        if let Some(descriptor) = builtin_descriptor(agent_id) {
            return Some(descriptor);
        }
        let profile_override = self.profile_overrides.get(agent_id)?;
        Some(AgentDescriptor {
            agent_id: agent_id.to_string().into(),
            display_name: agent_id.to_string().into(),
            signup_url: profile_override.signup_url.clone(),
            brand_accent: None,
            profile: profile_override.profile.clone(),
        })
    }
}

/// Names of the authentication methods each agent advertised over ACP when it
/// last connected in this session. Agents without a profile contract are
/// classified by whether they authenticate themselves this way.
#[derive(Default)]
pub struct AdvertisedAuthMethods(HashMap<String, Vec<SharedString>>);

impl Global for AdvertisedAuthMethods {}

impl AdvertisedAuthMethods {
    pub fn record(agent_id: &str, method_names: Vec<SharedString>, cx: &mut App) {
        cx.default_global::<Self>()
            .0
            .insert(agent_id.to_string(), method_names);
    }

    pub fn for_agent(&self, agent_id: &str) -> Option<&[SharedString]> {
        self.0.get(agent_id).map(Vec::as_slice)
    }
}

/// How far Lathe's account management reaches for one agent.
#[derive(Clone, Debug, PartialEq)]
pub enum AccountSupport {
    /// Multiple Lathe accounts, each its own profile dir, selectable per workspace.
    IsolatedProfile(AgentDescriptor),
    /// The agent signs in through its own ACP authentication methods. Lathe
    /// can't keep separate accounts for it.
    AgentManaged { auth_methods: Vec<SharedString> },
    /// No profile-isolation contract, and no ACP authentication seen yet.
    Unsupported,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InstalledAgent {
    pub agent_id: SharedString,
    pub display_name: SharedString,
}

/// One agent's section in the account UI.
#[derive(Clone, Debug, PartialEq)]
pub struct CatalogEntry {
    pub agent_id: SharedString,
    pub display_name: SharedString,
    pub installed: bool,
    pub support: AccountSupport,
}

impl CatalogEntry {
    pub fn descriptor(&self) -> Option<&AgentDescriptor> {
        match &self.support {
            AccountSupport::IsolatedProfile(descriptor) => Some(descriptor),
            AccountSupport::AgentManaged { .. } | AccountSupport::Unsupported => None,
        }
    }
}

/// Lists every agent the account UI should show: the built-in agents, every
/// installed external agent, and any agent that still has accounts in the
/// registry after being uninstalled, so its bindings and data stay visible
/// rather than silently orphaned.
pub fn build_catalog(
    installed: &[InstalledAgent],
    settings: &AiAccountsSettings,
    advertised_auth: Option<&AdvertisedAuthMethods>,
    index: &AiAccountsIndex,
) -> Vec<CatalogEntry> {
    let classify = |agent_id: &str, display_name: SharedString| {
        let support = match settings.descriptor_for(agent_id) {
            Some(mut descriptor) => {
                if builtin_descriptor(agent_id).is_none() {
                    descriptor.display_name = display_name.clone();
                }
                AccountSupport::IsolatedProfile(descriptor)
            }
            None => match advertised_auth.and_then(|methods| methods.for_agent(agent_id)) {
                Some(methods) if !methods.is_empty() => AccountSupport::AgentManaged {
                    auth_methods: methods.to_vec(),
                },
                _ => AccountSupport::Unsupported,
            },
        };
        (display_name, support)
    };
    let installed_agent = |agent_id: &str| {
        installed
            .iter()
            .find(|agent| agent.agent_id.as_ref() == agent_id)
    };

    let mut entries: Vec<CatalogEntry> = BUILTIN_AGENT_IDS
        .iter()
        .filter_map(|agent_id| {
            let descriptor = builtin_descriptor(agent_id)?;
            Some(CatalogEntry {
                agent_id: descriptor.agent_id.clone(),
                display_name: descriptor.display_name.clone(),
                installed: installed_agent(agent_id).is_some(),
                support: AccountSupport::IsolatedProfile(descriptor),
            })
        })
        .collect();

    let mut external: Vec<CatalogEntry> = installed
        .iter()
        .filter(|agent| builtin_descriptor(&agent.agent_id).is_none())
        .map(|agent| {
            let (display_name, support) = classify(&agent.agent_id, agent.display_name.clone());
            CatalogEntry {
                agent_id: agent.agent_id.clone(),
                display_name,
                installed: true,
                support,
            }
        })
        .collect();
    external.sort_by_key(|entry| entry.display_name.to_lowercase());
    entries.extend(external);

    let mut orphaned: Vec<CatalogEntry> = Vec::new();
    for account in &index.accounts {
        let agent_id = account.agent_id.as_str();
        let known = entries
            .iter()
            .chain(orphaned.iter())
            .any(|entry| entry.agent_id.as_ref() == agent_id);
        if known {
            continue;
        }
        let (display_name, support) = classify(agent_id, agent_id.to_string().into());
        orphaned.push(CatalogEntry {
            agent_id: agent_id.to_string().into(),
            display_name,
            installed: false,
            support,
        });
    }
    orphaned.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
    entries.extend(orphaned);
    entries
}

/// The environment changes Lathe makes to one agent child process for the
/// account bound to it. They apply only to that child: Lathe's own environment
/// is never touched, and nothing here reaches a process that is already
/// running, so a switch takes effect for new or reconnected threads only.
#[derive(Clone, Debug, Default)]
pub struct LaunchProfile {
    pub account: Option<AiAccount>,
    /// Always set, overriding any inherited or configured value.
    pub set_env: Vec<(String, String)>,
    /// Set only when nothing upstream of the spawn already provides a value.
    pub default_env: Vec<(String, String)>,
    /// Removed from both the configured and the inherited environment.
    pub removed_env: Vec<String>,
}

impl LaunchProfile {
    pub fn new(profile: &ProfileCapability, account: Option<&AiAccount>) -> Self {
        let mut set_env = Vec::new();
        let mut removed_env = Vec::new();
        if let Some(account) = account {
            set_env.push((
                profile.profile_env_var.to_string(),
                account.config_dir.display().to_string(),
            ));
            set_env.extend(
                profile
                    .bound_env
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.to_string())),
            );
            // Scrubbing only protects a bound account's login from an ambient
            // API key, so an agent running without an account keeps it.
            removed_env.extend(profile.scrub_env.iter().map(|key| key.to_string()));
        }
        Self {
            account: account.cloned(),
            set_env,
            default_env: profile
                .default_env
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
            removed_env,
        }
    }

    pub fn apply(&self, env: &mut HashMap<String, String>) {
        for (key, value) in &self.set_env {
            env.insert(key.clone(), value.clone());
        }
        for key in &self.removed_env {
            env.remove(key);
        }
        for (key, value) in &self.default_env {
            env.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
}

/// Resolves which account an agent launch uses and the environment that
/// selects it. `None` when the agent has no profile contract: such agents are
/// launched exactly as configured, with no guessed isolation.
pub fn resolve_launch_profile(
    agent_id: &str,
    workspace_binding: Option<&str>,
    settings: &AiAccountsSettings,
    index: &AiAccountsIndex,
) -> Option<(AgentDescriptor, LaunchProfile)> {
    let descriptor = settings.descriptor_for(agent_id)?;
    let account = settings.resolve_account(agent_id, workspace_binding, index);
    let launch_profile = LaunchProfile::new(&descriptor.profile, account);
    Some((descriptor, launch_profile))
}

/// Spawn-time upkeep for the account a launch resolved to. Failures are logged
/// rather than returned because they shouldn't block the agent starting.
pub fn prepare_account_for_launch(account: &AiAccount) {
    // Touch last_used_at so the account floats to the top of the Manage
    // modal's per-agent list.
    if let Err(error) = mark_account_used(&account.id) {
        log::warn!(
            "ai_accounts: failed to update last_used_at for {}: {error:#}",
            account.id
        );
    }
}

/// Account bindings made from inside a workspace, keyed by that workspace's
/// project. The `ai_accounts` setting lives in the user settings file that
/// every window and every running Lathe instance shares, so a switch recorded
/// there would move all of them at once.
#[derive(Default)]
pub struct WorkspaceAccountBindings(HashMap<EntityId, HashMap<String, String>>);

impl Global for WorkspaceAccountBindings {}

impl WorkspaceAccountBindings {
    pub fn binding(&self, project: EntityId, agent_id: &str) -> Option<&str> {
        self.0.get(&project)?.get(agent_id).map(String::as_str)
    }

    pub fn binding_for(project: EntityId, agent_id: &str, cx: &App) -> Option<String> {
        cx.try_global::<Self>()?
            .binding(project, agent_id)
            .map(str::to_owned)
    }

    pub fn set_all(project: EntityId, bindings: HashMap<String, String>, cx: &mut App) {
        cx.default_global::<Self>().0.insert(project, bindings);
    }

    /// Binds (`Some`) or unbinds (`None`) `agent_id` for `project`, returning
    /// the project's full binding map so the caller can persist it.
    pub fn set(
        project: EntityId,
        agent_id: &str,
        account_id: Option<String>,
        cx: &mut App,
    ) -> HashMap<String, String> {
        let bindings = cx.default_global::<Self>().0.entry(project).or_default();
        match account_id {
            Some(account_id) => {
                bindings.insert(agent_id.to_string(), account_id);
            }
            None => {
                bindings.remove(agent_id);
            }
        }
        bindings.clone()
    }
}

impl AiAccountsSettings {
    /// Resolves the active account for an agent in the current workspace.
    /// Order:
    /// 1. `workspace_binding`, from `WorkspaceAccountBindings`.
    /// 2. Binding from the `ai_accounts` setting.
    /// 3. Index-level default resolution (`AiAccountsIndex::default_for_agent`).
    /// 4. None.
    ///
    /// A binding only counts when it points at an account in the registry.
    pub fn resolve_account<'a>(
        &self,
        agent_id: &str,
        workspace_binding: Option<&str>,
        index: &'a AiAccountsIndex,
    ) -> Option<&'a AiAccount> {
        let bound_ids = workspace_binding
            .into_iter()
            .chain(self.bindings.get(agent_id).map(String::as_str));
        for bound_id in bound_ids {
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
///
/// Accounts created before Lathe moved out of Zed's data dir stay under Zed's
/// `ai_accounts/` (moving them would change Claude Code's keychain entry and
/// force a new sign-in). Those count as shared: Lathe builds still on Zed's
/// folders have their own index pointing at the same directories.
pub fn is_managed_config_dir(config_dir: &Path) -> bool {
    config_dir.starts_with(accounts_root())
}

/// Keychain lookup key under which an account's provider API key is stored, for
/// accounts added with an [`ApiKeyOption`]. The value is opaque to
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

/// Runs the descriptor's [`ProfileSetup`] once the account's config dir exists
/// and before the user attempts to log in.
fn apply_profile_setup(setup: ProfileSetup, config_dir: &Path) -> Result<()> {
    match setup {
        ProfileSetup::None => Ok(()),
        ProfileSetup::CodexFileCredentials => {
            let config_path = config_dir.join("config.toml");
            if config_path.exists() {
                return Ok(());
            }
            std::fs::write(&config_path, "cli_auth_credentials_store = \"file\"\n")
                .with_context(|| format!("writing Codex config.toml at {}", config_path.display()))
        }
        // Pre-select the AI Studio API-key method so the injected key is
        // actually used.
        ProfileSetup::GeminiApiKeyAuth => select_gemini_api_key_auth(config_dir),
    }
}

/// Prepares an account's profile to use a stored API key instead of the
/// agent's own sign-in, for a new account added with a key or an existing one
/// given a key later. The caller stores the key.
pub fn prepare_profile_for_api_key(descriptor: &AgentDescriptor, config_dir: &Path) -> Result<()> {
    let api_key = descriptor
        .profile
        .api_key
        .as_ref()
        .ok_or_else(|| anyhow!("{} doesn't accept an API key", descriptor.display_name))?;
    match api_key.setup {
        ProfileSetup::GeminiApiKeyAuth => select_gemini_api_key_auth(config_dir),
        setup => apply_profile_setup(setup, config_dir),
    }
}

/// Sets `security.auth.selectedType` in `<config_dir>/.gemini/settings.json`
/// to AI Studio API-key auth, keeping everything else in the file. A file that
/// doesn't parse is left alone rather than rewritten, since it's the CLI's.
fn select_gemini_api_key_auth(config_dir: &Path) -> Result<()> {
    let gemini_dir = config_dir.join(".gemini");
    let settings_path = gemini_dir.join("settings.json");
    let mut settings = match std::fs::read(&settings_path) {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
            .with_context(|| format!("parsing Gemini settings at {}", settings_path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("reading Gemini settings at {}", settings_path.display())
            });
        }
    };
    let auth = ["security", "auth"]
        .into_iter()
        .try_fold(&mut settings, |value, key| {
            let object = value.as_object_mut()?;
            let entry = object.entry(key).or_insert_with(|| serde_json::json!({}));
            entry.is_object().then_some(entry)
        })
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| {
            anyhow!(
                "Gemini settings at {} have an unexpected shape",
                settings_path.display()
            )
        })?;
    auth.insert(
        "selectedType".to_string(),
        serde_json::Value::String("gemini-api-key".to_string()),
    );
    std::fs::create_dir_all(&gemini_dir)
        .with_context(|| format!("creating Gemini config dir {}", gemini_dir.display()))?;
    let mut bytes = serde_json::to_vec_pretty(&settings).context("serializing Gemini settings")?;
    bytes.push(b'\n');
    std::fs::write(&settings_path, bytes)
        .with_context(|| format!("writing Gemini settings at {}", settings_path.display()))
}

/// Creates a new account for an agent with a profile contract, materializes
/// its config dir, runs the descriptor's profile setup, and persists it to the
/// registry. `with_api_key` also prepares the profile for the descriptor's
/// [`ApiKeyOption`]; the caller stores the key. The account starts in
/// `AccountState::Created`: it has not yet been logged in.
pub fn create_account(
    descriptor: &AgentDescriptor,
    display_name: &str,
    with_api_key: bool,
) -> Result<AiAccount> {
    let agent_id = descriptor.agent_id.as_ref();
    let mut index = load_index();
    validate_new_account_name(&index, agent_id, display_name)?;

    let account_id = uuid::Uuid::new_v4().to_string();
    let config_dir = accounts_root().join(agent_id).join(&account_id);

    std::fs::create_dir_all(&config_dir)
        .with_context(|| format!("creating account config dir {}", config_dir.display()))?;
    let _ = set_dir_permissions_700(&config_dir);

    apply_profile_setup(descriptor.profile.setup, &config_dir)?;
    if with_api_key {
        prepare_profile_for_api_key(descriptor, &config_dir)?;
    }

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
    let agent_id = CLAUDE_CODE_AGENT_ID;
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

/// Copies one conversation's on-disk transcript from one account's config dir
/// into another's, so the agent spawned for `to_config_dir` can load it by
/// session id. Agents only look for transcripts under their own config dir,
/// so switching accounts mid-thread needs this. An existing copy is
/// overwritten because the account being switched away from holds the newest
/// turns. Returns `false` when there is nothing to carry: no transcript was
/// written yet, or the layout is [`TranscriptLayout::None`].
pub fn carry_conversation_to_account(
    layout: TranscriptLayout,
    session_id: &str,
    from_config_dir: &Path,
    to_config_dir: &Path,
) -> Result<bool> {
    if session_id.is_empty()
        || session_id
            .chars()
            .any(|character| !(character.is_ascii_alphanumeric() || character == '-'))
    {
        return Err(anyhow!("unexpected session id {session_id:?}"));
    }
    let transcripts = match layout {
        TranscriptLayout::ClaudeProjects => claude_transcript_paths(from_config_dir, session_id),
        TranscriptLayout::CodexSessions => codex_transcript_paths(from_config_dir, session_id),
        TranscriptLayout::None => Vec::new(),
    };
    for source in &transcripts {
        let relative = source.strip_prefix(from_config_dir)?;
        let destination = to_config_dir.join(relative);
        copy_recursively(source, &destination).with_context(|| {
            format!("copying {} to {}", source.display(), destination.display())
        })?;
    }
    Ok(!transcripts.is_empty())
}

/// Claude Code keeps `projects/<encoded cwd>/<session id>.jsonl`, plus a
/// sibling `<session id>/` dir holding subagent transcripts and large tool
/// results the main transcript refers to.
fn claude_transcript_paths(config_dir: &Path, session_id: &str) -> Vec<PathBuf> {
    let Ok(project_entries) = std::fs::read_dir(config_dir.join("projects")) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for project_entry in project_entries.flatten() {
        let transcript = project_entry.path().join(format!("{session_id}.jsonl"));
        if transcript.is_file() {
            paths.push(transcript);
            let sidecar = project_entry.path().join(session_id);
            if sidecar.is_dir() {
                paths.push(sidecar);
            }
        }
    }
    paths
}

fn codex_transcript_paths(config_dir: &Path, session_id: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    walk_jsonl_files(&config_dir.join("sessions"), &mut |path| {
        let is_match = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem.starts_with("rollout-") && stem.ends_with(session_id));
        if is_match {
            paths.push(path.to_path_buf());
        }
    });
    paths
}

fn copy_recursively(source: &Path, destination: &Path) -> Result<()> {
    if source.is_dir() {
        std::fs::create_dir_all(destination)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_recursively(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(source, destination)?;
    }
    Ok(())
}

/// Whether [`list_conversations`] can read this agent's on-disk history.
pub fn has_conversation_history(agent_id: &str) -> bool {
    matches!(
        agent_id,
        CLAUDE_CODE_AGENT_ID | CODEX_AGENT_ID | GEMINI_AGENT_ID
    )
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

/// Runs a [`VerifyStrategy::Command`] check with the account's profile env var
/// set, returning whether the process exited successfully. A non-zero exit is
/// `Ok(false)` rather than `Err` so callers can flip the account state to
/// `Failed` without distinguishing "verify command crashed" from "verify
/// command said not authenticated." Spawn-time IO errors propagate, as does a
/// descriptor whose strategy is not a command.
pub async fn verify_account(account: &AiAccount, descriptor: &AgentDescriptor) -> Result<bool> {
    let VerifyStrategy::Command(argv) = &descriptor.profile.verify else {
        return Err(anyhow!("{} has no verify command", descriptor.display_name));
    };
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| anyhow!("verify command is empty for agent {}", account.agent_id))?;
    let status = smol::process::Command::new(program.as_ref())
        .args(args.iter().map(SharedString::as_ref))
        .env(
            descriptor.profile.profile_env_var.as_ref(),
            &account.config_dir,
        )
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
        if let Some(legacy) = lathe_data_import::legacy_data_dir() {
            let created_before_the_move = legacy.join("ai_accounts").join("gemini").join("id");
            assert!(!is_managed_config_dir(&created_before_the_move));
        }
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
    fn resolve_account_prefers_workspace_binding_over_setting() {
        let mut index = AiAccountsIndex::default();
        let personal = account("claude-acp", "personal");
        let work = account("claude-acp", "work");
        let personal_id = personal.id.clone();
        let work_id = work.id.clone();
        index.upsert(personal);
        index.upsert(work);
        let settings = AiAccountsSettings {
            bindings: HashMap::from_iter([("claude-acp".to_string(), work_id.clone())]),
            ..Default::default()
        };

        let resolve = |workspace_binding| {
            settings
                .resolve_account("claude-acp", workspace_binding, &index)
                .map(|account| account.id.clone())
        };
        assert_eq!(
            resolve(Some(personal_id.as_str())),
            Some(personal_id.clone())
        );
        assert_eq!(resolve(None), Some(work_id.clone()));
        assert_eq!(resolve(Some("deleted-account")), Some(work_id));
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
        let settings = AiAccountsSettings::default();
        let display_name = |agent_id| settings.descriptor_for(agent_id).map(|d| d.display_name);
        assert_eq!(display_name("claude-acp"), Some("Claude Agent".into()));
        assert_eq!(display_name("gemini"), Some("Gemini CLI".into()));
        assert_eq!(display_name("codex-acp"), Some("Codex CLI".into()));
        assert!(display_name("nope").is_none());
    }

    #[test]
    fn builtin_transcript_layouts() {
        let layout = |agent_id| {
            builtin_descriptor(agent_id)
                .expect("built-in agent")
                .profile
                .transcripts
        };
        assert_eq!(layout("claude-acp"), TranscriptLayout::ClaudeProjects);
        assert_eq!(layout("codex-acp"), TranscriptLayout::CodexSessions);
        assert_eq!(layout("gemini"), TranscriptLayout::None);
        let settings = settings_with_profiles(serde_json::json!({
            "opencode": { "profile_env_var": "OPENCODE_CONFIG_DIR" },
        }));
        assert_eq!(
            settings
                .descriptor_for("opencode")
                .expect("opencode")
                .profile
                .transcripts,
            TranscriptLayout::None
        );
    }

    #[test]
    fn gemini_signs_in_through_the_agent_with_an_optional_key() {
        let gemini = builtin_descriptor("gemini").expect("gemini descriptor");
        assert_eq!(gemini.profile.login_flow, LoginFlow::AgentAuth);
        assert_eq!(gemini.profile.setup, ProfileSetup::None);
        let api_key = gemini.profile.api_key.expect("gemini accepts a key");
        assert!(
            api_key.key_url.starts_with("https://"),
            "key_url must be https"
        );
        assert_eq!(api_key.env_var.as_ref(), "GEMINI_API_KEY");
        assert_eq!(api_key.setup, ProfileSetup::GeminiApiKeyAuth);
    }

    #[test]
    fn gemini_account_forces_file_token_storage() {
        let gemini = builtin_descriptor("gemini").expect("gemini descriptor");
        let bound = account("gemini", "personal");
        let mut env =
            HashMap::from_iter([("GEMINI_FORCE_FILE_STORAGE".to_string(), "false".to_string())]);
        LaunchProfile::new(&gemini.profile, Some(&bound)).apply(&mut env);
        assert_eq!(
            env.get("GEMINI_FORCE_FILE_STORAGE").map(String::as_str),
            Some("true"),
            "the shared keychain entry would merge every account's Google login"
        );

        let mut unbound_env = HashMap::default();
        LaunchProfile::new(&gemini.profile, None).apply(&mut unbound_env);
        assert!(
            unbound_env.is_empty(),
            "without an account Gemini keeps the user's own token storage"
        );
    }

    #[test]
    fn api_key_agents_do_not_scrub_their_own_key() {
        // Scrubbing is an OAuth-only protection. An agent that stripped its
        // own key var would defeat the auth an API-key account depends on
        // (this is the exact conflict that made "add a Gemini account" break
        // before).
        for descriptor in BUILTIN_AGENT_IDS
            .iter()
            .filter_map(|id| builtin_descriptor(id))
        {
            if let Some(api_key) = &descriptor.profile.api_key {
                assert!(
                    !descriptor.profile.scrub_env.contains(&api_key.env_var),
                    "{} must not scrub its own API-key var {}",
                    descriptor.agent_id,
                    api_key.env_var
                );
            }
        }
    }

    #[test]
    fn api_key_setup_switches_gemini_profile_to_key_auth() {
        let root = tempfile::tempdir().expect("tempdir");
        let gemini = builtin_descriptor("gemini").expect("gemini descriptor");
        let settings_path = root.path().join(".gemini/settings.json");
        let selected_type = |path: &Path| {
            let settings: serde_json::Value =
                serde_json::from_slice(&std::fs::read(path).expect("read settings"))
                    .expect("parse settings");
            settings["security"]["auth"]["selectedType"].clone()
        };

        prepare_profile_for_api_key(&gemini, root.path()).expect("new profile");
        assert_eq!(selected_type(&settings_path), "gemini-api-key");

        // A profile that already signed in another way keeps its other
        // settings but switches its auth type.
        write_file(
            &settings_path,
            r#"{"general":{"vimMode":true},"security":{"auth":{"selectedType":"oauth-personal"}}}"#,
        );
        prepare_profile_for_api_key(&gemini, root.path()).expect("existing profile");
        assert_eq!(selected_type(&settings_path), "gemini-api-key");
        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&settings_path).expect("read settings"))
                .expect("parse settings");
        assert_eq!(settings["general"]["vimMode"], true);

        write_file(&settings_path, "{ not json");
        assert!(prepare_profile_for_api_key(&gemini, root.path()).is_err());
        assert_eq!(
            std::fs::read_to_string(&settings_path).expect("read settings"),
            "{ not json",
            "a file that doesn't parse must not be overwritten"
        );

        let codex = builtin_descriptor("codex-acp").expect("codex descriptor");
        assert!(
            prepare_profile_for_api_key(&codex, root.path()).is_err(),
            "agents without an API-key option can't be switched to one"
        );
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

    fn write_file(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        std::fs::write(path, contents).expect("write file");
    }

    #[test]
    fn carry_conversation_copies_codex_rollout_to_same_relative_path() {
        let root = tempfile::tempdir().expect("tempdir");
        let from = root.path().join("work");
        let to = root.path().join("personal");
        let session_id = "01a13696-573d-7741-abbc-f16801d52361";
        let relative =
            format!("sessions/2026/10/13/rollout-2026-10-13T14-46-24-{session_id}.jsonl");
        write_file(&from.join(&relative), "newest");
        write_file(&to.join(&relative), "stale");
        write_file(
            &from.join("sessions/2026/10/13/rollout-2026-10-13T09-00-00-other.jsonl"),
            "other",
        );

        let carried =
            carry_conversation_to_account(TranscriptLayout::CodexSessions, session_id, &from, &to)
                .expect("carry");

        assert!(carried);
        assert_eq!(
            std::fs::read_to_string(to.join(&relative)).expect("read copy"),
            "newest"
        );
        assert!(
            !to.join("sessions/2026/10/13/rollout-2026-10-13T09-00-00-other.jsonl")
                .exists()
        );
    }

    #[test]
    fn carry_conversation_copies_claude_transcript_and_sidecar_dir() {
        let root = tempfile::tempdir().expect("tempdir");
        let from = root.path().join("work");
        let to = root.path().join("personal");
        let session_id = "5973b6c0-94b8-487b-a530-2aeb6098ae0e";
        let project = "projects/-Users-someone-code-app";
        write_file(&from.join(format!("{project}/{session_id}.jsonl")), "turns");
        write_file(
            &from.join(format!("{project}/{session_id}/subagents/agent-1.jsonl")),
            "subagent",
        );

        let carried =
            carry_conversation_to_account(TranscriptLayout::ClaudeProjects, session_id, &from, &to)
                .expect("carry");

        assert!(carried);
        assert!(to.join(format!("{project}/{session_id}.jsonl")).is_file());
        assert!(
            to.join(format!("{project}/{session_id}/subagents/agent-1.jsonl"))
                .is_file()
        );
    }

    #[test]
    fn carry_conversation_reports_nothing_to_carry() {
        let root = tempfile::tempdir().expect("tempdir");
        let from = root.path().join("work");
        let to = root.path().join("personal");
        let session_id = "5973b6c0-94b8-487b-a530-2aeb6098ae0e";
        assert!(
            !carry_conversation_to_account(TranscriptLayout::CodexSessions, session_id, &from, &to)
                .expect("carry")
        );
        write_file(
            &from.join(format!("projects/-a/{session_id}.jsonl")),
            "turns",
        );
        assert!(
            !carry_conversation_to_account(TranscriptLayout::None, session_id, &from, &to)
                .expect("carry"),
            "an unknown layout must not guess at files to copy"
        );
    }

    #[test]
    fn carry_conversation_rejects_path_like_session_ids() {
        let root = tempfile::tempdir().expect("tempdir");
        let result = carry_conversation_to_account(
            TranscriptLayout::ClaudeProjects,
            "../escape",
            root.path(),
            root.path(),
        );
        assert!(result.is_err());
    }

    fn settings_with_profiles(profiles: serde_json::Value) -> AiAccountsSettings {
        let mut content = SettingsContent::default();
        content.ai_account_profiles =
            Some(serde_json::from_value(profiles).expect("parse ai_account_profiles"));
        AiAccountsSettings::from_settings(&content)
    }

    fn installed(agent_id: &str, display_name: &str) -> InstalledAgent {
        InstalledAgent {
            agent_id: agent_id.to_string().into(),
            display_name: display_name.to_string().into(),
        }
    }

    #[test]
    fn settings_profile_resolves_for_unknown_agent() {
        let settings = settings_with_profiles(serde_json::json!({
            "opencode": {
                "profile_env_var": "OPENCODE_CONFIG_DIR",
                "login": { "terminal": ["opencode", "auth", "login"] },
                "verify_command": ["opencode", "auth", "list"],
                "env": { "OPENCODE_DISABLE_AUTOUPDATE": "1" },
                "scrub_env": ["OPENAI_API_KEY"],
            },
            "agent-auth-only": { "profile_env_var": "AGENT_HOME" },
        }));

        let opencode = settings
            .descriptor_for("opencode")
            .expect("opencode profile");
        assert_eq!(
            opencode.profile.profile_env_var.as_ref(),
            "OPENCODE_CONFIG_DIR"
        );
        assert_eq!(
            opencode.profile.login_flow,
            LoginFlow::EmbeddedTerminal {
                argv: vec!["opencode".into(), "auth".into(), "login".into()]
            }
        );
        assert_eq!(
            opencode.profile.verify,
            VerifyStrategy::Command(vec!["opencode".into(), "auth".into(), "list".into()])
        );
        assert_eq!(opencode.profile.setup, ProfileSetup::None);

        let minimal = settings
            .descriptor_for("agent-auth-only")
            .expect("minimal profile");
        assert_eq!(minimal.profile.login_flow, LoginFlow::AgentAuth);
        assert_eq!(minimal.profile.verify, VerifyStrategy::OnConnect);
    }

    #[test]
    fn settings_profile_rejects_missing_or_reserved_env_vars() {
        let settings = settings_with_profiles(serde_json::json!({
            "no-var": { "login": "agent" },
            "home": { "profile_env_var": "HOME" },
            "path": { "profile_env_var": "path" },
            "empty-terminal": { "profile_env_var": "X_HOME", "login": { "terminal": [] } },
        }));
        for agent_id in ["no-var", "home", "path", "empty-terminal"] {
            assert!(
                settings.descriptor_for(agent_id).is_none(),
                "{agent_id} must not get a profile contract"
            );
        }
    }

    #[test]
    fn settings_profile_cannot_replace_builtin_contract() {
        let settings = settings_with_profiles(serde_json::json!({
            "codex-acp": { "profile_env_var": "SOMETHING_ELSE" },
        }));
        let codex = settings.descriptor_for("codex-acp").expect("codex");
        assert_eq!(codex.profile.profile_env_var.as_ref(), "CODEX_HOME");
        assert_eq!(codex.profile.setup, ProfileSetup::CodexFileCredentials);
    }

    #[test]
    fn launch_profile_sets_scrubs_and_defaults_env() {
        let profile = ProfileCapability {
            profile_env_var: "AGENT_HOME".into(),
            login_flow: LoginFlow::AgentAuth,
            verify: VerifyStrategy::OnConnect,
            default_env: vec![
                ("MCP_TIMEOUT".into(), "5000".into()),
                ("AGENT_TELEMETRY".into(), "off".into()),
            ],
            scrub_env: vec!["AGENT_API_KEY".into()],
            bound_env: vec![("AGENT_TOKEN_STORE".into(), "file".into())],
            setup: ProfileSetup::None,
            transcripts: TranscriptLayout::None,
            api_key: None,
        };
        let bound = account("custom-agent", "work");
        let mut env = HashMap::from_iter([
            ("AGENT_HOME".to_string(), "/somewhere/else".to_string()),
            ("AGENT_API_KEY".to_string(), "ambient".to_string()),
            ("MCP_TIMEOUT".to_string(), "60000".to_string()),
            ("AGENT_TOKEN_STORE".to_string(), "keychain".to_string()),
        ]);

        let launch_profile = LaunchProfile::new(&profile, Some(&bound));
        launch_profile.apply(&mut env);

        assert_eq!(
            env.get("AGENT_HOME").map(String::as_str),
            Some(bound.config_dir.to_str().expect("utf-8 path")),
            "the bound account's dir must override any configured value"
        );
        assert!(!env.contains_key("AGENT_API_KEY"));
        assert_eq!(
            launch_profile.removed_env,
            vec!["AGENT_API_KEY".to_string()]
        );
        assert_eq!(
            env.get("MCP_TIMEOUT").map(String::as_str),
            Some("60000"),
            "an explicit value wins over a protective default"
        );
        assert_eq!(env.get("AGENT_TELEMETRY").map(String::as_str), Some("off"));
        assert_eq!(
            env.get("AGENT_TOKEN_STORE").map(String::as_str),
            Some("file"),
            "isolation settings override a configured value"
        );
    }

    #[test]
    fn launch_profile_without_account_only_applies_defaults() {
        let descriptor = builtin_descriptor("claude-acp").expect("claude");
        let mut env = HashMap::from_iter([("CLAUDE_CONFIG_DIR".to_string(), "/mine".to_string())]);
        let launch_profile = LaunchProfile::new(&descriptor.profile, None);
        launch_profile.apply(&mut env);
        assert_eq!(
            env.get("CLAUDE_CONFIG_DIR").map(String::as_str),
            Some("/mine")
        );
        assert_eq!(
            env.get("ENABLE_CLAUDEAI_MCP_SERVERS").map(String::as_str),
            Some("false")
        );
        assert!(launch_profile.removed_env.is_empty());
    }

    #[test]
    fn resolve_launch_profile_follows_workspace_binding() {
        let settings = settings_with_profiles(serde_json::json!({
            "custom-agent": { "profile_env_var": "AGENT_HOME" },
        }));
        let mut index = AiAccountsIndex::default();
        let personal = account("custom-agent", "personal");
        let work = account("custom-agent", "work");
        index.upsert(personal.clone());
        index.upsert(work.clone());

        let profile_dir = |binding: Option<&str>| {
            let (_, launch_profile) =
                resolve_launch_profile("custom-agent", binding, &settings, &index)
                    .expect("custom-agent has a profile contract");
            launch_profile.set_env
        };
        assert_eq!(
            profile_dir(Some(&personal.id)),
            vec![(
                "AGENT_HOME".to_string(),
                personal.config_dir.display().to_string()
            )]
        );
        assert_eq!(
            profile_dir(Some(&work.id)),
            vec![(
                "AGENT_HOME".to_string(),
                work.config_dir.display().to_string()
            )]
        );
        // Two accounts and no binding or default: nothing to select, so the
        // agent launches against its own default profile.
        assert!(profile_dir(None).is_empty());

        assert!(
            resolve_launch_profile("unknown-agent", None, &settings, &index).is_none(),
            "an agent without a contract is launched untouched"
        );
    }

    #[test]
    fn catalog_lists_builtins_installed_agents_and_classifies_them() {
        let settings = settings_with_profiles(serde_json::json!({
            "opencode": { "profile_env_var": "OPENCODE_CONFIG_DIR" },
        }));
        let mut advertised = AdvertisedAuthMethods::default();
        advertised
            .0
            .insert("auggie".to_string(), vec!["Sign in with Augment".into()]);
        let installed_agents = [
            installed("codex-acp", "Codex"),
            installed("opencode", "OpenCode"),
            installed("auggie", "Auggie"),
            installed("command-code", "Command Code"),
        ];

        let catalog = build_catalog(
            &installed_agents,
            &settings,
            Some(&advertised),
            &AiAccountsIndex::default(),
        );
        let ids: Vec<&str> = catalog
            .iter()
            .map(|entry| entry.agent_id.as_ref())
            .collect();
        assert_eq!(
            ids,
            [
                "claude-acp",
                "gemini",
                "codex-acp",
                "auggie",
                "command-code",
                "opencode"
            ]
        );

        let entry = |agent_id: &str| {
            catalog
                .iter()
                .find(|entry| entry.agent_id.as_ref() == agent_id)
                .expect("catalog entry")
        };
        assert!(!entry("claude-acp").installed);
        assert!(entry("codex-acp").installed);
        assert_eq!(entry("codex-acp").display_name.as_ref(), "Codex CLI");
        let opencode = entry("opencode").descriptor().expect("isolated profile");
        assert_eq!(opencode.display_name.as_ref(), "OpenCode");
        assert_eq!(
            entry("auggie").support,
            AccountSupport::AgentManaged {
                auth_methods: vec!["Sign in with Augment".into()]
            }
        );
        assert_eq!(entry("command-code").support, AccountSupport::Unsupported);
    }

    #[test]
    fn catalog_retains_agents_with_accounts_after_uninstall() {
        let settings = settings_with_profiles(serde_json::json!({
            "opencode": { "profile_env_var": "OPENCODE_CONFIG_DIR" },
        }));
        let mut index = AiAccountsIndex::default();
        index.upsert(account("opencode", "personal"));
        index.upsert(account("opencode", "work"));
        index.upsert(account("removed-agent", "old"));

        let catalog = build_catalog(&[], &settings, None, &index);
        let orphaned: Vec<(&str, bool, bool)> = catalog
            .iter()
            .filter(|entry| builtin_descriptor(&entry.agent_id).is_none())
            .map(|entry| {
                (
                    entry.agent_id.as_ref(),
                    entry.installed,
                    entry.descriptor().is_some(),
                )
            })
            .collect();
        assert_eq!(
            orphaned,
            [("opencode", false, true), ("removed-agent", false, false)],
            "each uninstalled agent with accounts appears once"
        );
    }

    #[test]
    fn profile_setup_writes_codex_file_credentials_once() {
        let root = tempfile::tempdir().expect("tempdir");
        apply_profile_setup(ProfileSetup::CodexFileCredentials, root.path()).expect("setup");
        let config_path = root.path().join("config.toml");
        assert_eq!(
            std::fs::read_to_string(&config_path).expect("read config"),
            "cli_auth_credentials_store = \"file\"\n"
        );
        std::fs::write(&config_path, "model = \"user-choice\"\n").expect("write config");
        apply_profile_setup(ProfileSetup::CodexFileCredentials, root.path()).expect("setup");
        assert_eq!(
            std::fs::read_to_string(&config_path).expect("read config"),
            "model = \"user-choice\"\n"
        );
    }
}
