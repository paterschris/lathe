use std::rc::Rc;

use std::path::PathBuf;

use ai_accounts::{
    AdvertisedAuthMethods, AiAccountsIndex, AiAccountsSettings, BrandAccent, CatalogEntry,
    InstalledAgent, TranscriptLayout, WorkspaceAccountBindings, build_catalog,
    carry_conversation_to_account, load_index, mark_account_used,
};
use collections::HashMap;
use db::kvp::KeyValueStore;
use gpui::{Entity, EntityId, Hsla, Rgba, WeakEntity, WindowAppearance, prelude::*};
use notifications::status_toast::StatusToast;
use project::{AgentId, Project};
use settings::{Settings as _, SettingsContent};
use ui::{
    ButtonLike, ButtonStyle, CommonAnimationExt, ContextMenu, ContextMenuEntry, PopoverMenu,
    prelude::*,
};
use util::ResultExt as _;
use workspace::{Workspace, WorkspaceId};

use crate::agent_panel::AgentPanel;
use crate::{AddAiAccount, Agent, ManageAiAccounts, NewExternalAgentThread};

pub(crate) fn brand_accent_color(accent: Option<&BrandAccent>, window: &Window) -> Option<Hsla> {
    let accent = accent?;
    let is_dark = matches!(
        window.appearance(),
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    );
    let hex = if is_dark { accent.dark } else { accent.light };
    Rgba::try_from(hex).ok().map(Hsla::from)
}

/// The agents the account UI lists for `project`: built-in agents, every
/// installed external agent, and agents that still have accounts after being
/// uninstalled.
pub(crate) fn account_catalog(
    project: &Entity<Project>,
    index: &AiAccountsIndex,
    cx: &App,
) -> Vec<CatalogEntry> {
    let installed: Vec<InstalledAgent> = {
        let store = project.read(cx).agent_server_store().read(cx);
        store
            .external_agents()
            .map(|agent_id| InstalledAgent {
                agent_id: agent_id.0.to_string().into(),
                display_name: store
                    .agent_display_name(agent_id)
                    .unwrap_or_else(|| agent_id.0.to_string().into()),
            })
            .collect()
    };
    build_catalog(
        &installed,
        AiAccountsSettings::get_global(cx),
        cx.try_global::<AdvertisedAuthMethods>(),
        index,
    )
}

impl AgentPanel {
    /// Renders the AI account chip in the panel header. Returns `None` when
    /// no ACP-mode thread is active or the active agent has no profile
    /// contract, so it can't hold isolated accounts. In those cases the chip
    /// is hidden entirely (no placeholder).
    pub(crate) fn render_ai_account_chip(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        // Resolve the active agent_id from `selected_agent` first so the chip
        // is visible for *draft* threads (before any message has spawned the
        // ACP subprocess). Falls back to the live thread's connection id when
        // for some reason `selected_agent` isn't a Custom variant — that path
        // covers existing in-flight threads cleanly.
        let agent_id_owned: String = match self.currently_selected_agent() {
            crate::Agent::Custom { id } => id.0.as_ref().to_string(),
            _ => self
                .active_agent_thread(cx)?
                .read(cx)
                .connection()
                .agent_id()
                .0
                .to_string(),
        };
        let settings = AiAccountsSettings::get_global(cx);
        let descriptor = settings.descriptor_for(&agent_id_owned)?;
        let agent_id = descriptor.agent_id.clone();

        let index = load_index();
        let workspace_binding =
            WorkspaceAccountBindings::binding_for(self.project_entity_id(), &agent_id, cx);
        let active_account =
            settings.resolve_account(&agent_id, workspace_binding.as_deref(), &index);

        let accent = brand_accent_color(descriptor.brand_accent.as_ref(), window);
        let chip_label: SharedString = active_account
            .map(|account| account.display_name.clone().into())
            .unwrap_or_else(|| SharedString::from("Add account…"));

        // Snapshot data the popover needs into owned values so the menu
        // closure doesn't borrow `self` or the index.
        let other_accounts: Vec<(String, String)> = index
            .for_agent(&agent_id)
            .filter(|account| active_account.map_or(true, |active| active.id != account.id))
            .map(|account| (account.id.clone(), account.display_name.clone()))
            .collect();
        let has_workspace_binding = workspace_binding.is_some();
        // Switching reloads the open thread on the new account's process,
        // which would drop a turn that is still streaming.
        let is_generating = self.active_agent_thread(cx).is_some_and(|thread| {
            !matches!(thread.read(cx).status(), acp_thread::ThreadStatus::Idle)
        });
        let is_switching_account = self
            .active_conversation_view()
            .is_some_and(|conversation_view| conversation_view.read(cx).is_switching_account());
        let panel = cx.weak_entity();
        let menu_id = SharedString::from(format!("ai-account-chip-menu-{agent_id}"));
        let trigger_id = SharedString::from(format!("ai-account-chip-trigger-{agent_id}"));

        let trigger = ButtonLike::new(trigger_id)
            .style(ButtonStyle::Subtle)
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        div()
                            .w_2()
                            .h_2()
                            .rounded_full()
                            .when_some(accent, |this, color| this.bg(color)),
                    )
                    .child(Label::new(chip_label).size(LabelSize::Small))
                    .when(is_switching_account, |this| {
                        this.child(
                            Icon::new(IconName::ArrowCircle)
                                .size(IconSize::XSmall)
                                .color(Color::Muted)
                                .with_rotate_animation(2),
                        )
                    }),
            );

        Some(
            PopoverMenu::new(menu_id)
                .trigger(trigger)
                .menu(move |window, cx| {
                    let other_accounts = other_accounts.clone();
                    let panel = panel.clone();
                    let agent_id = agent_id.clone();
                    Some(ContextMenu::build(
                        window,
                        cx,
                        move |mut menu, _window, _cx| {
                            let has_alternatives = !other_accounts.is_empty();
                            for (account_id, display_name) in other_accounts {
                                let panel = panel.clone();
                                let agent_id = agent_id.clone();
                                menu = menu.item(
                                    ContextMenuEntry::new(format!("Switch to {display_name}"))
                                        .disabled(is_generating || is_switching_account)
                                        .handler(move |window, cx| {
                                            let agent_id = agent_id.to_string();
                                            let account_id = account_id.clone();
                                            panel
                                                .update(cx, |panel, cx| {
                                                    panel.switch_ai_account_in_thread(
                                                        agent_id, account_id, window, cx,
                                                    );
                                                })
                                                .ok();
                                        }),
                                );
                            }
                            if has_workspace_binding {
                                let panel = panel.clone();
                                let agent_id = agent_id.clone();
                                menu = menu.entry(
                                    SharedString::from("Clear binding for this workspace"),
                                    None,
                                    move |_window, cx| {
                                        panel
                                            .update(cx, |panel, cx| {
                                                panel.clear_ai_account_binding(&agent_id, cx);
                                            })
                                            .ok();
                                    },
                                );
                            }
                            if has_alternatives || has_workspace_binding {
                                menu = menu.separator();
                            }
                            menu = menu.entry(
                                SharedString::from("Add account…"),
                                None,
                                move |window, cx| {
                                    window.dispatch_action(
                                        Box::new(AddAiAccount {
                                            agent_id: Some(agent_id.to_string()),
                                        }),
                                        cx,
                                    );
                                },
                            );
                            menu.entry(
                                SharedString::from("Manage accounts…"),
                                None,
                                |window, cx| {
                                    window.dispatch_action(Box::new(ManageAiAccounts), cx);
                                },
                            )
                        },
                    ))
                }),
        )
    }

    /// Re-spawns the cached ACP subprocess for `agent_id`. The per-account
    /// config-dir env var (e.g. `CLAUDE_CONFIG_DIR`) is injected only once, at
    /// subprocess spawn, so a bind or switch that merely rewrites the setting
    /// would silently keep the previous account until the next restart. This
    /// forces that restart so the new binding takes effect.
    fn restart_ai_agent_connection(&mut self, agent_id: &str, cx: &mut Context<Self>) {
        let agent = Agent::Custom {
            id: AgentId::new(agent_id.to_string()),
        };
        let server: Rc<dyn agent_servers::AgentServer> =
            Rc::new(agent_servers::CustomAgentServer::new(agent.id()));
        self.connection_store().clone().update(cx, |store, cx| {
            store.restart_connection(agent, server, cx);
        });
    }

    /// Restarts `agent_id` when `account_id` is the account it runs as in
    /// this workspace, so credentials that changed on disk or in the keychain
    /// take effect. Credentials are only read when the process spawns.
    pub(crate) fn restart_if_account_in_use(
        &mut self,
        agent_id: &str,
        account_id: &str,
        cx: &mut Context<Self>,
    ) {
        let workspace_binding =
            WorkspaceAccountBindings::binding_for(self.project_entity_id(), agent_id, cx);
        let in_use = AiAccountsSettings::get_global(cx)
            .resolve_account(agent_id, workspace_binding.as_deref(), &load_index())
            .is_some_and(|account| account.id == account_id);
        if in_use {
            self.restart_ai_agent_connection(agent_id, cx);
        }
    }

    /// Binds `account_id` for `agent_id` in this workspace only, then restarts
    /// the agent connection so the new account is actually used.
    pub(crate) fn switch_ai_account(
        &mut self,
        agent_id: String,
        account_id: String,
        cx: &mut Context<Self>,
    ) {
        if let Err(error) = mark_account_used(&account_id) {
            log::warn!("ai_accounts: failed to mark {account_id} used: {error:#}");
        }
        bind_workspace_account(
            self.workspace_id(),
            self.project_entity_id(),
            &agent_id,
            Some(account_id),
            cx,
        );
        self.restart_ai_agent_connection(&agent_id, cx);
        cx.notify();
    }

    /// Like `switch_ai_account`, and also moves the open thread over to the
    /// new account so the conversation continues there.
    fn switch_ai_account_in_thread(
        &mut self,
        agent_id: String,
        account_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let index = load_index();
        let settings = AiAccountsSettings::get_global(cx);
        let Some(descriptor) = settings.descriptor_for(&agent_id) else {
            return;
        };
        let previous_config_dir = settings
            .resolve_account(
                &agent_id,
                WorkspaceAccountBindings::binding_for(self.project_entity_id(), &agent_id, cx)
                    .as_deref(),
                &index,
            )
            .map(|account| account.config_dir.clone());
        let next_account = index.find(&account_id);
        let next_config_dir = next_account.map(|account| account.config_dir.clone());
        let next_display_name = next_account
            .map(|account| account.display_name.clone())
            .unwrap_or_default();
        self.switch_ai_account(agent_id.clone(), account_id, cx);
        if let (Some(from), Some(to)) = (previous_config_dir, next_config_dir)
            && from != to
        {
            let moved = self.move_active_thread_to_account(
                agent_id,
                descriptor.profile.transcripts,
                from,
                to,
                window,
                cx,
            );
            if !moved {
                self.show_account_toast(
                    format!(
                        "Switched to \"{next_display_name}\" for new threads. Lathe can't move                          an open {} conversation between accounts yet.",
                        descriptor.display_name
                    ),
                    cx,
                );
            }
        }
    }

    fn show_account_toast(&self, message: String, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace().upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_status_toast(StatusToast::new(message, cx, |this, _cx| this), cx);
        });
    }

    /// The open thread keeps talking to the process it was started with, so
    /// without this a switch only affects threads created afterwards. A thread
    /// with history is reloaded by session id on the new account's process,
    /// which first needs the transcript copied into that account's config dir.
    /// Returns `false` when the open conversation can't follow the switch
    /// because the agent has no [`TranscriptLayout`] adapter.
    fn move_active_thread_to_account(
        &mut self,
        agent_id: String,
        transcripts: TranscriptLayout,
        from_config_dir: PathBuf,
        to_config_dir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(conversation_view) = self.active_conversation_view().cloned() else {
            return true;
        };
        let Some(thread) = conversation_view.read(cx).root_thread(cx) else {
            return true;
        };
        let thread = thread.read(cx);
        if thread.connection().agent_id().0.as_ref() != agent_id {
            return true;
        }
        if thread.entries().is_empty() {
            window.dispatch_action(
                Box::new(NewExternalAgentThread {
                    agent: AgentId::new(agent_id),
                }),
                cx,
            );
            return true;
        }
        if transcripts == TranscriptLayout::None {
            return false;
        }
        let session_id = thread.session_id().0.to_string();
        let conversation_view = conversation_view.downgrade();
        cx.spawn_in(window, async move |_this, cx| {
            let carried = cx
                .background_spawn({
                    let session_id = session_id.clone();
                    async move {
                        carry_conversation_to_account(
                            transcripts,
                            &session_id,
                            &from_config_dir,
                            &to_config_dir,
                        )
                    }
                })
                .await?;
            if !carried {
                log::warn!(
                    "ai_accounts: no {agent_id} transcript found for session {session_id}; \
                     the open thread stays on the previous account"
                );
                return Ok(());
            }
            conversation_view.update_in(cx, |conversation_view, window, cx| {
                conversation_view.reload_for_account_switch(window, cx);
            })
        })
        .detach_and_log_err(cx);
        true
    }

    fn clear_ai_account_binding(&mut self, agent_id: &str, cx: &mut Context<Self>) {
        bind_workspace_account(
            self.workspace_id(),
            self.project_entity_id(),
            agent_id,
            None,
            cx,
        );
        self.restart_ai_agent_connection(agent_id, cx);
        cx.notify();
    }

    /// Connects a freshly added AI account that signs in through the agent's
    /// own ACP authentication (Claude, settings-defined agents):
    /// bind it to this workspace, restart the agent connection so the new
    /// (empty) config dir is used, then open a thread. Because the config dir
    /// has no credentials yet, the agent reports auth-required and the thread
    /// surfaces the real code-based sign-in. Driven from the panel so it
    /// outlives the dismissed Add AI Account modal.
    pub(crate) fn connect_new_ai_account_in_thread(
        &mut self,
        agent_id: String,
        account_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        bind_workspace_account(
            self.workspace_id(),
            self.project_entity_id(),
            &agent_id,
            Some(account_id),
            cx,
        );
        self.restart_ai_agent_connection(&agent_id, cx);
        window.dispatch_action(
            Box::new(NewExternalAgentThread {
                agent: AgentId::new(agent_id),
            }),
            cx,
        );
    }
}

const WORKSPACE_AI_ACCOUNTS_KEY: &str = "workspace_ai_accounts";

/// Restores the account bindings a workspace recorded in an earlier session.
pub(crate) fn load_workspace_ai_accounts(
    workspace_id: Option<WorkspaceId>,
    project: EntityId,
    cx: &mut App,
) {
    let Some(workspace_id) = workspace_id else {
        return;
    };
    let bindings = KeyValueStore::global(cx)
        .scoped(WORKSPACE_AI_ACCOUNTS_KEY)
        .read(&i64::from(workspace_id).to_string())
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str::<HashMap<String, String>>(&json).log_err());
    if let Some(bindings) = bindings {
        WorkspaceAccountBindings::set_all(project, bindings, cx);
    }
}

pub(crate) fn bind_account_to_workspace(
    workspace: &WeakEntity<Workspace>,
    agent_id: &str,
    account_id: Option<String>,
    cx: &mut App,
) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    let (workspace_id, project) = workspace.read_with(cx, |workspace, _cx| {
        (workspace.database_id(), workspace.project().entity_id())
    });
    bind_workspace_account(workspace_id, project, agent_id, account_id, cx);
}

/// Binds (`Some`) or unbinds (`None`) an agent's account for one workspace.
/// Workspaces that have never been saved have no database id, so their
/// binding lasts only as long as the window.
pub(crate) fn bind_workspace_account(
    workspace_id: Option<WorkspaceId>,
    project: EntityId,
    agent_id: &str,
    account_id: Option<String>,
    cx: &mut App,
) {
    let bindings = WorkspaceAccountBindings::set(project, agent_id, account_id, cx);
    let Some(workspace_id) = workspace_id else {
        return;
    };
    let kvp = KeyValueStore::global(cx);
    cx.background_spawn(async move {
        let scope = kvp.scoped(WORKSPACE_AI_ACCOUNTS_KEY);
        let key = i64::from(workspace_id).to_string();
        if bindings.is_empty() {
            scope.delete(key).await
        } else {
            scope.write(key, serde_json::to_string(&bindings)?).await
        }
    })
    .detach_and_log_err(cx);
}

/// Drops every binding that points at `account_id`, whichever agent holds it.
/// Called after an account is deleted: a binding left pointing at a missing
/// account makes `AiAccountsSettings::resolve_account` fall back to some other
/// account, which silently runs the agent under credentials the user didn't
/// pick. Only user-level settings are rewritten, so a stale binding in a
/// workspace `.zed/settings.json` or in `WorkspaceAccountBindings` survives;
/// `resolve_account` logs when it hits one.
pub(crate) fn unbind_account(settings: &mut SettingsContent, account_id: &str) {
    let Some(map) = settings.ai_accounts.as_mut() else {
        return;
    };
    map.0.retain(|_, bound_id| bound_id.as_str() != account_id);
}
