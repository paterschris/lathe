use crate::{AgentServer, AgentServerDelegate, load_proxy_env};
use acp_thread::AgentConnection;
use agent_client_protocol::schema::v1 as acp;
use ai_accounts::{AiAccountsSettings, api_key_keychain_url, load_index};
use anyhow::{Context as _, Result};
use collections::HashSet;
use fs::Fs;
use gpui::{App, AppContext as _, Entity, Task};
use language_model::{ApiKey, EnvVar};
use project::{
    Project,
    agent_server_store::{AgentId, AllAgentServersSettings},
};
use settings::{SettingsStore, update_settings_file};
use std::{rc::Rc, sync::Arc};
use ui::IconName;

pub const GEMINI_ID: &str = "gemini";
pub const CLAUDE_AGENT_ID: &str = "claude-acp";
pub const CODEX_ID: &str = "codex-acp";
pub const CURSOR_ID: &str = "cursor";

/// A generic agent server implementation for custom user-defined agents
pub struct CustomAgentServer {
    agent_id: AgentId,
}

impl CustomAgentServer {
    pub fn new(agent_id: AgentId) -> Self {
        Self { agent_id }
    }
}

impl AgentServer for CustomAgentServer {
    fn agent_id(&self) -> AgentId {
        self.agent_id.clone()
    }

    fn logo(&self) -> IconName {
        IconName::Terminal
    }

    fn default_mode(&self, cx: &App) -> Option<acp::SessionModeId> {
        let settings = cx.read_global(|settings: &SettingsStore, _| {
            settings
                .get::<AllAgentServersSettings>(None)
                .get(self.agent_id().0.as_ref())
                .cloned()
        });

        settings
            .as_ref()
            .and_then(|s| s.default_mode().map(acp::SessionModeId::new))
    }

    fn favorite_config_option_value_ids(
        &self,
        config_id: &acp::SessionConfigId,
        cx: &mut App,
    ) -> HashSet<acp::SessionConfigValueId> {
        let settings = cx.read_global(|settings: &SettingsStore, _| {
            settings
                .get::<AllAgentServersSettings>(None)
                .get(self.agent_id().0.as_ref())
                .cloned()
        });

        settings
            .as_ref()
            .and_then(|s| s.favorite_config_option_values(config_id.0.as_ref()))
            .map(|values| {
                values
                    .iter()
                    .cloned()
                    .map(acp::SessionConfigValueId::new)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn toggle_favorite_config_option_value(
        &self,
        config_id: acp::SessionConfigId,
        value_id: acp::SessionConfigValueId,
        should_be_favorite: bool,
        fs: Arc<dyn Fs>,
        cx: &App,
    ) {
        let agent_id = self.agent_id();
        let config_id = config_id.to_string();
        let value_id = value_id.to_string();

        update_settings_file(fs, cx, move |settings, _cx| {
            let settings = settings
                .agent_servers
                .get_or_insert_default()
                .entry(agent_id.0.to_string())
                .or_insert_with(default_settings_for_agent);

            match settings {
                settings::CustomAgentServerSettings::Custom {
                    favorite_config_option_values,
                    ..
                }
                | settings::CustomAgentServerSettings::Registry {
                    favorite_config_option_values,
                    ..
                } => {
                    let entry = favorite_config_option_values
                        .entry(config_id.clone())
                        .or_insert_with(Vec::new);

                    if should_be_favorite {
                        if !entry.iter().any(|v| v == &value_id) {
                            entry.push(value_id.clone());
                        }
                    } else {
                        entry.retain(|v| v != &value_id);
                        if entry.is_empty() {
                            favorite_config_option_values.remove(&config_id);
                        }
                    }
                }
            }
        });
    }

    fn set_default_mode(&self, mode_id: Option<acp::SessionModeId>, fs: Arc<dyn Fs>, cx: &mut App) {
        let agent_id = self.agent_id();
        update_settings_file(fs, cx, move |settings, _cx| {
            let settings = settings
                .agent_servers
                .get_or_insert_default()
                .entry(agent_id.0.to_string())
                .or_insert_with(default_settings_for_agent);

            match settings {
                settings::CustomAgentServerSettings::Custom { default_mode, .. }
                | settings::CustomAgentServerSettings::Registry { default_mode, .. } => {
                    *default_mode = mode_id.map(|m| m.to_string());
                }
            }
        });
    }

    fn default_approval(&self, cx: &App) -> Option<String> {
        cx.read_global(|settings: &SettingsStore, _| {
            settings
                .get::<AllAgentServersSettings>(None)
                .get(self.agent_id().0.as_ref())
                .and_then(|s| s.approval())
                .map(|level| level.to_string())
        })
    }

    fn set_default_approval(&self, level: Option<String>, fs: Arc<dyn Fs>, cx: &mut App) {
        let agent_id = self.agent_id();
        update_settings_file(fs, cx, move |settings, _cx| {
            let settings = settings
                .agent_servers
                .get_or_insert_default()
                .entry(agent_id.0.to_string())
                .or_insert_with(default_settings_for_agent);

            match settings {
                settings::CustomAgentServerSettings::Custom { approval, .. }
                | settings::CustomAgentServerSettings::Registry { approval, .. } => {
                    *approval = level;
                }
            }
        });
    }

    fn default_config_option(&self, config_id: &str, cx: &App) -> Option<String> {
        let settings = cx.read_global(|settings: &SettingsStore, _| {
            settings
                .get::<AllAgentServersSettings>(None)
                .get(self.agent_id().as_ref())
                .cloned()
        });

        settings
            .as_ref()
            .and_then(|s| s.default_config_option(config_id).map(|s| s.to_string()))
    }

    fn set_default_config_option(
        &self,
        config_id: &str,
        value_id: Option<&str>,
        fs: Arc<dyn Fs>,
        cx: &mut App,
    ) {
        let agent_id = self.agent_id();
        let config_id = config_id.to_string();
        let value_id = value_id.map(|s| s.to_string());
        update_settings_file(fs, cx, move |settings, _cx| {
            let settings = settings
                .agent_servers
                .get_or_insert_default()
                .entry(agent_id.0.to_string())
                .or_insert_with(default_settings_for_agent);

            match settings {
                settings::CustomAgentServerSettings::Custom {
                    default_config_options,
                    ..
                }
                | settings::CustomAgentServerSettings::Registry {
                    default_config_options,
                    ..
                } => {
                    if let Some(value) = value_id.clone() {
                        default_config_options.insert(config_id.clone(), value);
                    } else {
                        default_config_options.remove(&config_id);
                    }
                }
            }
        });
    }

    fn connect(
        &self,
        delegate: AgentServerDelegate,
        project: Entity<Project>,
        cx: &mut App,
    ) -> Task<Result<Rc<dyn AgentConnection>>> {
        let agent_id = self.agent_id();
        let default_mode = self.default_mode(cx);
        let default_approval = self.default_approval(cx);
        let is_registry_agent = is_registry_agent(agent_id.clone(), cx);
        let default_config_options = cx.read_global(|settings: &SettingsStore, _| {
            settings
                .get::<AllAgentServersSettings>(None)
                .get(self.agent_id().as_ref())
                .map(|s| match s {
                    project::agent_server_store::CustomAgentServerSettings::Custom {
                        default_config_options,
                        ..
                    }
                    | project::agent_server_store::CustomAgentServerSettings::Registry {
                        default_config_options,
                        ..
                    } => default_config_options.clone(),
                })
                .unwrap_or_default()
        });

        if is_registry_agent {
            if let Some(registry_store) = project::AgentRegistryStore::try_global(cx) {
                registry_store.update(cx, |store, cx| store.refresh_if_stale(cx));
            }
        }

        let mut extra_env = load_proxy_env(cx);
        if delegate.store.read(cx).no_browser() {
            extra_env.insert("NO_BROWSER".to_owned(), "1".to_owned());
        }
        if is_registry_agent {
            match agent_id.as_ref() {
                CLAUDE_AGENT_ID => {
                    extra_env.insert("ANTHROPIC_API_KEY".into(), "".into());
                }
                CODEX_ID => {
                    if let Ok(api_key) = std::env::var("CODEX_API_KEY") {
                        extra_env.insert("CODEX_API_KEY".into(), api_key);
                    }
                    if let Ok(api_key) = std::env::var("OPEN_AI_API_KEY") {
                        extra_env.insert("OPEN_AI_API_KEY".into(), api_key);
                    }
                }
                GEMINI_ID => {
                    extra_env.insert("SURFACE".to_owned(), "zed".to_owned());
                }
                _ => {}
            }
        }
        let extra_args = if is_registry_agent {
            default_approval
                .as_deref()
                .map(|level| approval_args(agent_id.as_ref(), level))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let store = delegate.store.downgrade();
        cx.spawn(async move |cx| {
            if is_registry_agent && agent_id.as_ref() == GEMINI_ID {
                match cx.update(api_key_for_gemini_cli).await {
                    Ok(api_key) => {
                        // Confirm injection positively (length only, never the
                        // key) so the log distinguishes "key was injected" from
                        // the warning paths above.
                        log::info!(
                            "gemini: injected GEMINI_API_KEY ({} chars) into the subprocess env",
                            api_key.len()
                        );
                        extra_env.insert("GEMINI_API_KEY".into(), api_key);
                    }
                    // Surface why the key is missing instead of silently dropping
                    // it: without GEMINI_API_KEY the Gemini CLI falls back to
                    // OAuth ("Login with Google") and reports auth-required / 401.
                    Err(error) => log::warn!(
                        "gemini: no API key injected, the CLI will fail to authenticate: {error:#}"
                    ),
                }
            }
            let command = store
                .update(cx, |store, cx| {
                    let agent = store.get_external_agent(&agent_id).with_context(|| {
                        format!("Custom agent server `{}` is not registered", agent_id)
                    })?;
                    if let Some(new_version_available_tx) = delegate.new_version_available {
                        agent.set_new_version_available_tx(new_version_available_tx);
                    }
                    if let Some(loading_status_tx) = delegate.loading_status {
                        agent.set_loading_status_tx(loading_status_tx);
                    }
                    anyhow::Ok(agent.get_command(extra_args, extra_env, &mut cx.to_async()))
                })??
                .await?;
            let connection = crate::acp::connect(
                agent_id,
                project,
                command,
                store.clone(),
                default_mode,
                default_config_options,
                cx,
            )
            .await?;
            Ok(connection)
        })
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn std::any::Any> {
        self
    }
}

fn api_key_for_gemini_cli(cx: &mut App) -> Task<Result<String>> {
    // Prefer the key the user saved for the workspace-bound Gemini account, so
    // each AI account carries its own AI Studio key. Fall back to an ambient
    // env var, then a legacy global keychain entry, so setups that predate
    // per-account keys keep working.
    let account_url = cx
        .try_global::<SettingsStore>()
        .map(|store| store.get::<AiAccountsSettings>(None).clone())
        .and_then(|settings| {
            settings
                .resolve_account(GEMINI_ID, &load_index())
                .map(|account| api_key_keychain_url(&account.agent_id, &account.id))
        });

    let env_var = EnvVar::new("GEMINI_API_KEY".into()).or(EnvVar::new("GOOGLE_AI_API_KEY".into()));
    let credentials_provider = zed_credentials_provider::global(cx);
    let global_url = google_ai::API_URL.to_string();

    cx.spawn(async move |cx| {
        if let Some(url) = account_url {
            match ApiKey::load_from_system_keychain(&url, credentials_provider.as_ref(), cx).await {
                Ok(api_key) => return Ok(api_key.key().to_string()),
                // Log instead of swallowing: a keychain miss here (e.g. a Dev
                // build reading the file-backed credentials store while the key
                // was written to the system keychain) is the difference between
                // a working agent and a silent OAuth fallback.
                Err(error) => log::warn!(
                    "gemini: failed to load the per-account API key from the keychain ({url}): {error}"
                ),
            }
        } else {
            log::warn!(
                "gemini: no AI account resolved for this workspace, so no per-account API key could be loaded"
            );
        }
        if let Some(key) = env_var.value {
            return Ok(key);
        }
        ApiKey::load_from_system_keychain(&global_url, credentials_provider.as_ref(), cx)
            .await
            .map(|api_key| api_key.key().to_string())
            .map_err(|error| {
                anyhow::anyhow!(
                    "no Gemini API key available (per-account keychain and GEMINI_API_KEY env both \
                     unavailable; legacy global keychain at {global_url} failed: {error})"
                )
            })
    })
}

fn is_registry_agent(agent_id: impl Into<AgentId>, cx: &App) -> bool {
    let agent_id = agent_id.into();
    let is_in_registry = project::AgentRegistryStore::try_global(cx)
        .map(|store| store.read(cx).agent(&agent_id).is_some())
        .unwrap_or(false);
    let is_settings_registry = cx.read_global(|settings: &SettingsStore, _| {
        settings
            .get::<AllAgentServersSettings>(None)
            .get(agent_id.as_ref())
            .is_some_and(|s| {
                matches!(
                    s,
                    project::agent_server_store::CustomAgentServerSettings::Registry { .. }
                )
            })
    });
    is_in_registry || is_settings_registry
}

fn default_settings_for_agent() -> settings::CustomAgentServerSettings {
    settings::CustomAgentServerSettings::Registry {
        default_mode: None,
        approval: None,
        env: Default::default(),
        default_config_options: Default::default(),
        favorite_config_option_values: Default::default(),
    }
}

/// Translate a stored approval level into the CLI flags to launch the agent with.
///
/// The valid levels differ per agent and are the ones offered by the approval
/// selector in the agent panel. An unknown agent or level yields no flags, so the
/// agent falls back to its own default behaviour.
fn approval_args(agent_id: &str, level: &str) -> Vec<String> {
    match agent_id {
        CODEX_ID => match level {
            "read_only" => codex_config_args("on-request", "read-only"),
            "auto" => codex_config_args("on-failure", "workspace-write"),
            "full_access" => codex_config_args("never", "danger-full-access"),
            _ => Vec::new(),
        },
        GEMINI_ID => match level {
            "default" | "auto_edit" | "yolo" => {
                vec!["--approval-mode".to_string(), level.to_string()]
            }
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Codex reads approval and sandbox policy from `config.toml`; `-c key=value`
/// overrides those at launch. Values are quoted so codex parses them as TOML strings.
fn codex_config_args(approval_policy: &str, sandbox_mode: &str) -> Vec<String> {
    vec![
        "-c".to_string(),
        format!("approval_policy=\"{approval_policy}\""),
        "-c".to_string(),
        format!("sandbox_mode=\"{sandbox_mode}\""),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use collections::HashMap;
    use gpui::TestAppContext;
    use project::agent_registry_store::{
        AgentRegistryStore, RegistryAgent, RegistryAgentMetadata, RegistryNpxAgent,
    };
    use settings::Settings as _;
    use ui::SharedString;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    #[test]
    fn approval_args_maps_levels_to_flags() {
        assert_eq!(
            approval_args(CODEX_ID, "full_access"),
            vec![
                "-c",
                "approval_policy=\"never\"",
                "-c",
                "sandbox_mode=\"danger-full-access\"",
            ]
        );
        assert_eq!(
            approval_args(CODEX_ID, "read_only"),
            vec![
                "-c",
                "approval_policy=\"on-request\"",
                "-c",
                "sandbox_mode=\"read-only\"",
            ]
        );
        assert_eq!(
            approval_args(GEMINI_ID, "yolo"),
            vec!["--approval-mode", "yolo"]
        );

        // Unknown agent or level yields no flags (agent uses its own default).
        assert!(approval_args(CODEX_ID, "bogus").is_empty());
        assert!(approval_args("other-agent", "full_access").is_empty());
    }

    fn init_registry_with_agents(cx: &mut TestAppContext, agent_ids: &[&str]) {
        let agents: Vec<RegistryAgent> = agent_ids
            .iter()
            .map(|id| {
                let id = SharedString::from(id.to_string());
                RegistryAgent::Npx(RegistryNpxAgent {
                    metadata: RegistryAgentMetadata {
                        id: AgentId::new(id.clone()),
                        name: id.clone(),
                        description: SharedString::from(""),
                        version: SharedString::from("1.0.0"),
                        repository: None,
                        website: None,
                        icon_path: None,
                    },
                    package: id,
                    args: Vec::new(),
                    env: HashMap::default(),
                })
            })
            .collect();
        cx.update(|cx| {
            AgentRegistryStore::init_test_global(cx, agents);
        });
    }

    fn set_agent_server_settings(
        cx: &mut TestAppContext,
        entries: Vec<(&str, settings::CustomAgentServerSettings)>,
    ) {
        cx.update(|cx| {
            AllAgentServersSettings::override_global(
                project::agent_server_store::AllAgentServersSettings(
                    entries
                        .into_iter()
                        .map(|(name, settings)| (name.to_string(), settings.into()))
                        .collect(),
                ),
                cx,
            );
        });
    }

    #[gpui::test]
    fn test_unknown_agent_is_not_registry(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            assert!(!is_registry_agent("my-custom-agent", cx));
        });
    }

    #[gpui::test]
    fn test_agent_in_registry_store_is_registry(cx: &mut TestAppContext) {
        init_test(cx);
        init_registry_with_agents(cx, &["some-new-registry-agent"]);
        cx.update(|cx| {
            assert!(is_registry_agent("some-new-registry-agent", cx));
            assert!(!is_registry_agent("not-in-registry", cx));
        });
    }

    #[gpui::test]
    fn test_agent_with_registry_settings_type_is_registry(cx: &mut TestAppContext) {
        init_test(cx);
        set_agent_server_settings(
            cx,
            vec![(
                "agent-from-settings",
                settings::CustomAgentServerSettings::Registry {
                    env: HashMap::default(),
                    default_mode: None,
                    approval: None,
                    default_config_options: HashMap::default(),
                    favorite_config_option_values: HashMap::default(),
                },
            )],
        );
        cx.update(|cx| {
            assert!(is_registry_agent("agent-from-settings", cx));
        });
    }
}
