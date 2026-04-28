use std::collections::HashSet;

use ai_accounts::{
    AccountState, AgentDescriptor, AiAccount, AiAccountsIndex, BrandAccent, CLAUDE_CODE_DESCRIPTOR,
    ConversationSummary, TIER_A_DESCRIPTORS, claude_profiles_dir, delete_account,
    import_from_claude_profiles, list_conversations, load_index, resume_command, save_index,
    verify_account,
};
#[cfg(test)]
use ai_accounts::descriptor_for;
use gpui::{
    AnyElement, DismissEvent, EventEmitter, FocusHandle, Focusable, Hsla, Rgba, WeakEntity,
    WindowAppearance, prelude::*,
};
use notifications::status_toast::StatusToast;
use ui::{ListItem, ListItemSpacing, ListSeparator, Tooltip, prelude::*};
use workspace::{ModalView, Workspace};

use crate::{AddAiAccount, ManageAiAccounts};

pub struct ManageAiAccountsModal {
    focus_handle: FocusHandle,
    index: AiAccountsIndex,
    expanded_accounts: HashSet<String>,
    workspace: WeakEntity<Workspace>,
}

impl ManageAiAccountsModal {
    pub fn register(
        workspace: &mut Workspace,
        _window: Option<&mut Window>,
        _cx: &mut Context<Workspace>,
    ) {
        workspace.register_action(|workspace, _: &ManageAiAccounts, window, cx| {
            let workspace_handle = cx.weak_entity();
            workspace.toggle_modal(window, cx, |_window, cx| {
                Self::new(workspace_handle, cx)
            });
        });
    }

    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            index: load_index(),
            expanded_accounts: HashSet::new(),
            workspace,
        }
    }

    fn toast(&self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        let message = message.into();
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_status_toast(
                StatusToast::new(message, cx, |this, _cx| this),
                cx,
            );
        });
    }

    fn toggle_expand(&mut self, account_id: String, cx: &mut Context<Self>) {
        if !self.expanded_accounts.remove(&account_id) {
            self.expanded_accounts.insert(account_id);
        }
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.index = load_index();
        cx.notify();
    }

    fn cancel(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn delete(&mut self, account_id: String, window: &mut Window, cx: &mut Context<Self>) {
        // Look up the display name up-front so the prompt is informative even
        // if the index changes between user click and prompt resolution.
        let display_name = self
            .index
            .find(&account_id)
            .map(|account| account.display_name.clone())
            .unwrap_or_else(|| account_id.clone());

        let prompt = window.prompt(
            gpui::PromptLevel::Critical,
            &format!("Delete AI account \"{display_name}\"?"),
            Some("This removes the registry entry and deletes the account's config directory on disk. Cannot be undone."),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            let Ok(Some(0)) = prompt.await.map(Some) else {
                return;
            };
            // Always trash files alongside the registry entry. A "keep files
            // but unregister" toggle can be added later if users ask for it.
            let success = match delete_account(&account_id, true) {
                Ok(_) => true,
                Err(error) => {
                    log::error!("ai_accounts: delete failed: {error:#}");
                    false
                }
            };
            this.update(cx, |this, cx| {
                this.refresh(cx);
                if success {
                    this.toast(format!("Deleted account \"{display_name}\""), cx);
                } else {
                    this.toast("Delete failed — see logs", cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn verify(&mut self, account_id: String, cx: &mut Context<Self>) {
        let Some(account) = self.index.find(&account_id).cloned() else {
            return;
        };
        // Optimistic UI: flip the row to Pending immediately so the user
        // sees something happen. The async verify resolves it to
        // Authenticated or Failed.
        if let Err(error) = self.index.set_state(&account_id, AccountState::Pending) {
            log::error!("ai_accounts: set Pending state failed: {error:#}");
            return;
        }
        let _ = save_index(&self.index);
        cx.notify();

        let display_name = account.display_name.clone();
        cx.spawn(async move |this, cx| {
            let result = verify_account(&account).await;
            this.update(cx, |this, cx| {
                let (state, message) = match result {
                    Ok(true) => (
                        AccountState::Authenticated,
                        format!("Connection verified for \"{display_name}\""),
                    ),
                    Ok(false) => (
                        AccountState::Failed,
                        format!(
                            "Verification failed for \"{display_name}\" — try signing in again"
                        ),
                    ),
                    Err(error) => {
                        log::error!("ai_accounts: verify spawn failed: {error:#}");
                        (
                            AccountState::Failed,
                            format!("Couldn't run verify command for \"{display_name}\""),
                        )
                    }
                };
                if let Err(error) = this.index.set_state(&account_id, state) {
                    log::error!("ai_accounts: set state failed: {error:#}");
                }
                let _ = save_index(&this.index);
                this.refresh(cx);
                this.toast(message, cx);
            })
            .ok();
        })
        .detach();
    }

    fn import_claude_profiles(&mut self, cx: &mut Context<Self>) {
        let toast_message = match import_from_claude_profiles() {
            Ok(report) => {
                if !report.failed.is_empty() {
                    log::warn!(
                        "ai_accounts: failed to import {} claude-account-switcher profile(s): {:?}",
                        report.failed.len(),
                        report.failed
                    );
                }
                let imported = report.imported.len();
                let skipped = report.skipped_existing.len();
                if imported == 0 && skipped == 0 {
                    "No claude-account-switcher profiles found to import.".to_string()
                } else if imported == 0 {
                    format!("All {skipped} profile(s) already imported.")
                } else if skipped == 0 {
                    format!("Imported {imported} profile(s) from claude-account-switcher.")
                } else {
                    format!(
                        "Imported {imported}, skipped {skipped} (already registered)."
                    )
                }
            }
            Err(error) => {
                log::error!("ai_accounts: claude-profiles import failed: {error:#}");
                "Import failed — see logs.".to_string()
            }
        };
        self.refresh(cx);
        self.toast(toast_message, cx);
    }

    fn set_default(
        &mut self,
        agent_id: String,
        account_id: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(error) = self
            .index
            .set_default(&agent_id, Some(account_id))
            .and_then(|_| save_index(&self.index))
        {
            log::error!("ai_accounts: set default failed: {error:#}");
            // Reload to discard any in-memory mutation that didn't persist.
            self.refresh(cx);
            return;
        }
        cx.notify();
    }
}

impl Focusable for ManageAiAccountsModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for ManageAiAccountsModal {}
impl ModalView for ManageAiAccountsModal {}

fn brand_accent_color(accent: &BrandAccent, window: &Window) -> Option<Hsla> {
    let is_dark = matches!(
        window.appearance(),
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    );
    let hex = if is_dark { accent.dark } else { accent.light };
    Rgba::try_from(hex).ok().map(Hsla::from)
}

fn render_conversations(
    account: &AiAccount,
    workspace: WeakEntity<Workspace>,
    _cx: &mut Context<ManageAiAccountsModal>,
) -> impl IntoElement {
    let conversations: Vec<ConversationSummary> = list_conversations(account);
    let supported_agent = matches!(
        account.agent_id.as_str(),
        "claude-acp" | "codex-acp" | "gemini"
    );

    if conversations.is_empty() {
        let message = if supported_agent {
            "No conversations yet for this account."
        } else {
            "Conversation history for this agent is not yet supported."
        };
        return v_flex()
            .pl_8()
            .pr_3()
            .pb_2()
            .child(
                Label::new(SharedString::from(message))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .into_any_element();
    }

    let agent_id_for_rows = account.agent_id.clone();
    v_flex()
        .pl_8()
        .pr_3()
        .pb_2()
        .gap_0p5()
        .children(conversations.into_iter().take(20).map(|conv| {
            let title = SharedString::from(conv.title.clone());
            let project_hint = conv.project_hint.clone().map(SharedString::from);
            let row_id = SharedString::from(format!("conv-{}-{}", agent_id_for_rows, conv.id));
            // Compute the resume command up-front so the click handler is a
            // pure clipboard write — no agent_id dispatch happens at click time.
            let resume_cmd = resume_command(&agent_id_for_rows, &conv.id);

            ListItem::new(row_id)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .when_some(resume_cmd.clone(), |this, _cmd| {
                    this.tooltip(Tooltip::text("Click to copy resume command"))
                })
                .when_some(resume_cmd, |this, cmd| {
                    let workspace = workspace.clone();
                    this.on_click(move |_, _, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(cmd.clone()));
                        if let Some(workspace) = workspace.upgrade() {
                            let toast_message =
                                SharedString::from(format!("Copied: {cmd}"));
                            workspace.update(cx, |workspace, cx| {
                                workspace.toggle_status_toast(
                                    StatusToast::new(toast_message, cx, |this, _cx| this),
                                    cx,
                                );
                            });
                        }
                    })
                })
                .child(
                    v_flex()
                        .gap_0p5()
                        .child(Label::new(title).size(LabelSize::Small))
                        .when_some(project_hint, |this, hint| {
                            this.child(
                                Label::new(hint)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                        }),
                )
        }))
        .into_any_element()
}

fn state_label(state: AccountState) -> (&'static str, Color) {
    match state {
        AccountState::Created => ("Not signed in", Color::Muted),
        AccountState::Pending => ("Signing in…", Color::Warning),
        AccountState::Authenticated => ("Connected", Color::Success),
        AccountState::Failed => ("Sign-in failed", Color::Error),
        AccountState::Expired => ("Session expired", Color::Warning),
    }
}

impl ManageAiAccountsModal {
    fn render_section(
        &mut self,
        descriptor: &'static AgentDescriptor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let agent_id = descriptor.agent_id;
        let mut accounts: Vec<AiAccount> = self
            .index
            .for_agent(agent_id)
            .cloned()
            .collect::<Vec<_>>();
        // Most-used first (last_used_at desc), ties broken by created_at desc,
        // then case-insensitive display_name asc. None values for both
        // timestamps sort to the bottom — newly created/never-used accounts
        // appear after used ones until they're touched.
        accounts.sort_by(|a, b| {
            b.last_used_at
                .cmp(&a.last_used_at)
                .then_with(|| b.created_at.cmp(&a.created_at))
                .then_with(|| a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()))
        });
        let default_id = self
            .index
            .default_for_agent(agent_id)
            .map(|account| account.id.clone());
        let accent = brand_accent_color(&descriptor.brand_accent, window);

        let add_button_id = SharedString::from(format!("add-account-{agent_id}"));
        let is_claude_code = agent_id == CLAUDE_CODE_DESCRIPTOR.agent_id;
        let claude_profiles_present = is_claude_code
            && claude_profiles_dir()
                .map(|path| path.exists())
                .unwrap_or(false);
        let header = h_flex()
            .px_2()
            .pt_2()
            .pb_1()
            .gap_2()
            .child(
                div()
                    .w_2()
                    .h_2()
                    .rounded_full()
                    .when_some(accent, |this, color| this.bg(color)),
            )
            .child(
                Label::new(SharedString::from(descriptor.display_name))
                    .size(LabelSize::Default),
            )
            .child(div().flex_1())
            .when(claude_profiles_present, |this| {
                this.child(
                    Button::new(
                        SharedString::from("import-claude-profiles"),
                        "Import from claude-account-switcher",
                    )
                    .label_size(LabelSize::XSmall)
                    .tooltip(Tooltip::text(
                        "Register profiles from ~/.claude-profiles/ as Lathe AI Accounts",
                    ))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.import_claude_profiles(cx);
                    })),
                )
            })
            .child(
                Button::new(add_button_id, "Add account")
                    .label_size(LabelSize::XSmall)
                    .on_click(cx.listener(move |_, _, window, cx| {
                        // Close this modal first, then dispatch — toggle_modal
                        // for AddAiAccountModal would otherwise stack on top.
                        cx.emit(DismissEvent);
                        window.dispatch_action(
                            Box::new(AddAiAccount {
                                agent_id: Some(agent_id.to_string()),
                            }),
                            cx,
                        );
                    })),
            );

        let body = if accounts.is_empty() {
            v_flex()
                .px_2()
                .pb_2()
                .child(
                    Label::new(SharedString::from(format!(
                        "No accounts yet. Add one to use {} in this workspace.",
                        descriptor.display_name
                    )))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .into_any_element()
        } else {
            let mut rows: Vec<AnyElement> = Vec::with_capacity(accounts.len() * 2);
            for account in accounts {
                let is_default = default_id.as_deref() == Some(account.id.as_str());
                let is_expanded = self.expanded_accounts.contains(&account.id);
                rows.push(
                    self.render_account_row(agent_id, &account, is_default, is_expanded, cx)
                        .into_any_element(),
                );
                if is_expanded {
                    rows.push(
                        render_conversations(&account, self.workspace.clone(), cx)
                            .into_any_element(),
                    );
                }
            }
            v_flex().children(rows).into_any_element()
        };

        v_flex().child(header).child(body)
    }

    fn render_account_row(
        &mut self,
        agent_id: &'static str,
        account: &AiAccount,
        is_default: bool,
        is_expanded: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let only_one_account = self.index.for_agent(agent_id).count() <= 1;
        let (status_text, status_color) = state_label(account.state);
        let row_id = SharedString::from(format!("ai-account-{}", account.id));
        let display_name = SharedString::from(account.display_name.clone());
        let account_id_for_default = account.id.clone();
        let agent_id_for_default = agent_id.to_string();
        let account_id_for_delete = account.id.clone();
        let account_id_for_toggle = account.id.clone();
        let account_id_for_verify = account.id.clone();
        let is_verifying = matches!(account.state, AccountState::Pending);
        let chevron_icon = if is_expanded {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        };

        ListItem::new(row_id)
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .start_slot(Icon::new(chevron_icon).size(IconSize::XSmall).color(Color::Muted))
            .on_click(cx.listener(move |this, _, _window, cx| {
                this.toggle_expand(account_id_for_toggle.clone(), cx);
            }))
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(Label::new(display_name))
                    .when(is_default, |this| {
                        this.child(
                            Label::new("default")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .child(
                        Label::new(SharedString::from(status_text))
                            .size(LabelSize::XSmall)
                            .color(status_color),
                    ),
            )
            .end_slot(
                h_flex()
                    .gap_1()
                    .child(
                        IconButton::new(
                            SharedString::from(format!("verify-{}", account.id)),
                            IconName::Check,
                        )
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .disabled(is_verifying)
                        .tooltip(Tooltip::text(if is_verifying {
                            "Verifying…"
                        } else {
                            "Test connection"
                        }))
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.verify(account_id_for_verify.clone(), cx);
                        })),
                    )
                    .when(!is_default && !only_one_account, |this| {
                        this.child(
                            Button::new(
                                SharedString::from(format!("set-default-{}", account.id)),
                                "Set default",
                            )
                            .label_size(LabelSize::XSmall)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.set_default(
                                    agent_id_for_default.clone(),
                                    account_id_for_default.clone(),
                                    window,
                                    cx,
                                );
                            })),
                        )
                    })
                    .child(
                        IconButton::new(
                            SharedString::from(format!("delete-{}", account.id)),
                            IconName::Trash,
                        )
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Error)
                        .tooltip(Tooltip::text("Delete account"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.delete(account_id_for_delete.clone(), window, cx);
                        })),
                    ),
            )
    }
}

impl Render for ManageAiAccountsModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = h_flex().px_3().py_2().child(
            Label::new(SharedString::from("Manage AI Accounts")).size(LabelSize::Large),
        );

        let any_accounts_anywhere = !self.index.accounts.is_empty();
        let claude_profiles_present = claude_profiles_dir()
            .map(|path| path.exists())
            .unwrap_or(false);

        let body = if any_accounts_anywhere {
            // Materialize sections imperatively to avoid an FnMut closure trying
            // to escape `cx`-tied references through `.map(|descriptor| ...)`.
            let mut sections: Vec<AnyElement> = Vec::with_capacity(TIER_A_DESCRIPTORS.len());
            for descriptor in TIER_A_DESCRIPTORS {
                sections.push(
                    self.render_section(descriptor, window, cx)
                        .into_any_element(),
                );
            }
            div().children(sections).into_any_element()
        } else {
            self.render_empty_hero(claude_profiles_present, cx)
                .into_any_element()
        };

        div()
            .elevation_3(cx)
            .w(rems(34.))
            .key_context("ManageAiAccountsModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| {
                this.cancel(window, cx);
            }))
            .capture_any_mouse_down(cx.listener(|this, _, window, cx| {
                this.focus_handle(cx).focus(window, cx);
            }))
            .on_mouse_down_out(cx.listener(|_this, _, _, cx| cx.emit(DismissEvent)))
            .child(header)
            .child(ListSeparator)
            .child(body)
            .child(ListSeparator)
            .child(
                div().px_3().py_2().child(
                    Label::new(SharedString::from(
                        "Tip: each workspace can bind a different account per agent via .zed/settings.json (\"ai_accounts\").",
                    ))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                ),
            )
    }
}

impl ManageAiAccountsModal {
    fn render_empty_hero(
        &self,
        claude_profiles_present: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .px_4()
            .py_6()
            .gap_3()
            .items_center()
            .child(
                Label::new(SharedString::from("No AI accounts yet"))
                    .size(LabelSize::Large),
            )
            .child(
                Label::new(SharedString::from(
                    "Add an account to use Claude Code, Gemini, or Codex with a subscription-authenticated identity. Each workspace can bind to a different account per agent.",
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("empty-add-account", "Add your first account")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|_, _, window, cx| {
                                cx.emit(DismissEvent);
                                window.dispatch_action(
                                    Box::new(AddAiAccount { agent_id: None }),
                                    cx,
                                );
                            })),
                    )
                    .when(claude_profiles_present, |this| {
                        this.child(
                            Button::new(
                                "empty-import-claude-profiles",
                                "Import from claude-account-switcher",
                            )
                            .label_size(LabelSize::Default)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.import_claude_profiles(cx);
                            })),
                        )
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_have_unique_brand_accents() {
        let mut light_seen = std::collections::HashSet::new();
        let mut dark_seen = std::collections::HashSet::new();
        for descriptor in TIER_A_DESCRIPTORS {
            assert!(
                light_seen.insert(descriptor.brand_accent.light),
                "duplicate light accent for {}",
                descriptor.agent_id
            );
            assert!(
                dark_seen.insert(descriptor.brand_accent.dark),
                "duplicate dark accent for {}",
                descriptor.agent_id
            );
        }
    }

    #[test]
    fn brand_accents_parse_as_rgba() {
        for descriptor in TIER_A_DESCRIPTORS {
            assert!(
                Rgba::try_from(descriptor.brand_accent.light).is_ok(),
                "light hex parse failed for {}",
                descriptor.agent_id
            );
            assert!(
                Rgba::try_from(descriptor.brand_accent.dark).is_ok(),
                "dark hex parse failed for {}",
                descriptor.agent_id
            );
        }
    }

    #[test]
    fn state_label_covers_every_variant() {
        // Forces a compile error if a new AccountState variant is added without a label.
        for state in [
            AccountState::Created,
            AccountState::Pending,
            AccountState::Authenticated,
            AccountState::Failed,
            AccountState::Expired,
        ] {
            let (text, _color) = state_label(state);
            assert!(!text.is_empty());
        }
    }

    #[test]
    fn descriptor_for_returns_known_agents() {
        assert!(descriptor_for("claude-acp").is_some());
        assert!(descriptor_for("gemini").is_some());
        assert!(descriptor_for("codex-acp").is_some());
    }
}
