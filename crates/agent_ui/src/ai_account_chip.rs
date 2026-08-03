use std::rc::Rc;

use ai_accounts::{
    AgentDescriptor, AiAccountsSettings, BrandAccent, descriptor_for, load_index, mark_account_used,
};
use gpui::{Hsla, Rgba, WindowAppearance, prelude::*};
use project::AgentId;
use settings::{
    Settings as _, SettingsContent, update_settings_file, update_settings_file_with_completion,
};
use ui::{ButtonLike, ButtonStyle, ContextMenu, PopoverMenu, prelude::*};

use crate::agent_panel::AgentPanel;
use crate::{AddAiAccount, Agent, ManageAiAccounts, NewExternalAgentThread};

fn brand_accent_color(accent: &BrandAccent, window: &Window) -> Option<Hsla> {
    let is_dark = matches!(
        window.appearance(),
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    );
    let hex = if is_dark { accent.dark } else { accent.light };
    Rgba::try_from(hex).ok().map(Hsla::from)
}

impl AgentPanel {
    /// Renders the AI account chip in the panel header. Returns `None` when
    /// no ACP-mode thread is active or the active agent isn't a Tier A agent
    /// — in those cases the chip is hidden entirely (no placeholder).
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
        let descriptor: &'static AgentDescriptor = descriptor_for(&agent_id_owned)?;
        let agent_id_static: &'static str = descriptor.agent_id;

        let settings = AiAccountsSettings::get_global(cx).clone();
        let index = load_index();
        let active_account = settings.resolve_account(agent_id_static, &index);

        let accent = brand_accent_color(&descriptor.brand_accent, window);
        let chip_label: SharedString = active_account
            .map(|account| account.display_name.clone().into())
            .unwrap_or_else(|| SharedString::from("Add account…"));

        // Snapshot data the popover needs into owned values so the menu
        // closure doesn't borrow `self` or the index.
        let other_accounts: Vec<(String, String)> = index
            .for_agent(agent_id_static)
            .filter(|account| active_account.map_or(true, |active| active.id != account.id))
            .map(|account| (account.id.clone(), account.display_name.clone()))
            .collect();
        let active_id = active_account.map(|account| account.id.clone());
        let fs = self.fs();
        let panel = cx.weak_entity();
        let menu_id = SharedString::from(format!("ai-account-chip-menu-{agent_id_static}"));
        let trigger_id = SharedString::from(format!("ai-account-chip-trigger-{agent_id_static}"));

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
                    .child(Label::new(chip_label).size(LabelSize::Small)),
            );

        Some(
            PopoverMenu::new(menu_id)
                .trigger(trigger)
                .menu(move |window, cx| {
                    let other_accounts = other_accounts.clone();
                    let fs = fs.clone();
                    let active_id = active_id.clone();
                    let panel = panel.clone();
                    Some(ContextMenu::build(
                        window,
                        cx,
                        move |mut menu, _window, _cx| {
                            let has_alternatives = !other_accounts.is_empty();
                            for (account_id, display_name) in other_accounts {
                                let panel = panel.clone();
                                menu = menu.entry(
                                    SharedString::from(format!("Switch to {display_name}")),
                                    None,
                                    move |_window, cx| {
                                        let agent_id = agent_id_static.to_string();
                                        let account_id = account_id.clone();
                                        // Bind, wait for the binding to land, then
                                        // restart the agent's subprocess so the new
                                        // account's config dir is actually applied
                                        // (the env is only read at spawn time).
                                        panel
                                            .update(cx, |panel, cx| {
                                                panel.switch_ai_account(agent_id, account_id, cx);
                                            })
                                            .ok();
                                    },
                                );
                            }
                            if active_id.is_some() {
                                let fs = fs.clone();
                                menu = menu.entry(
                                    SharedString::from("Clear binding for this workspace"),
                                    None,
                                    move |_window, cx| {
                                        let agent_id = agent_id_static.to_string();
                                        update_settings_file(
                                            fs.clone(),
                                            cx,
                                            move |settings, _cx| {
                                                bind_account(settings, &agent_id, None);
                                            },
                                        );
                                    },
                                );
                            }
                            if has_alternatives || active_id.is_some() {
                                menu = menu.separator();
                            }
                            menu = menu.entry(
                                SharedString::from("Add account…"),
                                None,
                                move |window, cx| {
                                    window.dispatch_action(
                                        Box::new(AddAiAccount {
                                            agent_id: Some(agent_id_static.to_string()),
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

    /// Binds `account_id` for `agent_id`, waits for the binding to apply to the
    /// global settings store, then restarts the agent connection so the new
    /// account is actually used. `update_settings_file_with_completion` applies
    /// `set_user_settings` before resolving, so by the time the restart runs the
    /// global `AiAccountsSettings` already reflects the binding (no race).
    pub(crate) fn switch_ai_account(
        &mut self,
        agent_id: String,
        account_id: String,
        cx: &mut Context<Self>,
    ) {
        if let Err(error) = mark_account_used(&account_id) {
            log::warn!("ai_accounts: failed to mark {account_id} used: {error:#}");
        }
        let completion = update_settings_file_with_completion(self.fs(), cx, {
            let agent_id = agent_id.clone();
            move |settings, _cx| bind_account(settings, &agent_id, Some(account_id))
        });
        cx.spawn(async move |panel, cx| {
            completion.await.ok();
            panel
                .update(cx, |panel, cx| {
                    panel.restart_ai_agent_connection(&agent_id, cx);
                })
                .ok();
        })
        .detach();
    }

    /// Connects a freshly added in-thread-login AI account (Claude, Gemini):
    /// bind, wait for it to apply, restart the agent connection so the new
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
        let completion = update_settings_file_with_completion(self.fs(), cx, {
            let agent_id = agent_id.clone();
            move |settings, _cx| bind_account(settings, &agent_id, Some(account_id))
        });
        cx.spawn_in(window, async move |panel, cx| {
            completion.await.ok();
            panel
                .update_in(cx, |panel, window, cx| {
                    panel.restart_ai_agent_connection(&agent_id, cx);
                    window.dispatch_action(
                        Box::new(NewExternalAgentThread {
                            agent: AgentId::new(agent_id),
                        }),
                        cx,
                    );
                })
                .ok();
        })
        .detach();
    }
}

/// Updates the `ai_accounts` mapping in workspace settings to bind (or unbind)
/// an agent to a specific account. Called from the chip's switch action and
/// from the Add Account modal's Connect handler. Pass `Some(id)` to bind,
/// `None` to clear the binding.
pub(crate) fn bind_account(
    settings: &mut SettingsContent,
    agent_id: &str,
    account_id: Option<String>,
) {
    let map = settings.ai_accounts.get_or_insert_default();
    match account_id {
        Some(id) => {
            map.0.insert(agent_id.to_string(), id);
        }
        None => {
            map.0.remove(agent_id);
        }
    }
}

/// Drops every binding that points at `account_id`, whichever agent holds it.
/// Called after an account is deleted: a binding left pointing at a missing
/// account makes `AiAccountsSettings::resolve_account` fall back to some other
/// account, which silently runs the agent under credentials the user didn't
/// pick. Only user-level settings are rewritten, so a stale binding in a
/// workspace `.zed/settings.json` survives; `resolve_account` logs when it
/// hits one.
pub(crate) fn unbind_account(settings: &mut SettingsContent, account_id: &str) {
    let Some(map) = settings.ai_accounts.as_mut() else {
        return;
    };
    map.0.retain(|_, bound_id| bound_id.as_str() != account_id);
}
