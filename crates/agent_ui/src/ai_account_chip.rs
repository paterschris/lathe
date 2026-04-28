use ai_accounts::{
    AgentDescriptor, AiAccountsSettings, BrandAccent, descriptor_for, load_index,
    mark_account_used,
};
use gpui::{Hsla, Rgba, WindowAppearance, prelude::*};
use settings::{Settings as _, SettingsContent, update_settings_file};
use ui::{ButtonLike, ButtonStyle, ContextMenu, PopoverMenu, prelude::*};

use crate::agent_panel::AgentPanel;
use crate::{AddAiAccount, ManageAiAccounts};

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
            .filter(|account| {
                active_account.map_or(true, |active| active.id != account.id)
            })
            .map(|account| (account.id.clone(), account.display_name.clone()))
            .collect();
        let active_id = active_account.map(|account| account.id.clone());
        let fs = self.fs();
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
                    Some(ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
                        let has_alternatives = !other_accounts.is_empty();
                        for (account_id, display_name) in other_accounts {
                            let fs = fs.clone();
                            menu = menu.entry(
                                SharedString::from(format!("Switch to {display_name}")),
                                None,
                                move |_window, cx| {
                                    let agent_id = agent_id_static.to_string();
                                    let account_id_for_settings = account_id.clone();
                                    let account_id_for_touch = account_id.clone();
                                    update_settings_file(fs.clone(), cx, move |settings, _cx| {
                                        bind_account(
                                            settings,
                                            &agent_id,
                                            Some(account_id_for_settings),
                                        );
                                    });
                                    if let Err(error) = mark_account_used(&account_id_for_touch) {
                                        log::warn!(
                                            "ai_accounts: failed to mark {account_id_for_touch} used: {error:#}"
                                        );
                                    }
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
                                    update_settings_file(fs.clone(), cx, move |settings, _cx| {
                                        bind_account(settings, &agent_id, None);
                                    });
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
                    }))
                }),
        )
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
