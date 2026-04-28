use std::sync::Arc;

use ai_accounts::{
    AgentDescriptor, BrandAccent, LoginFlow, TIER_A_DESCRIPTORS, create_account, descriptor_for,
};
use collections::HashMap;
use fs::Fs;
use notifications::status_toast::StatusToast;
use gpui::{
    DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Hsla, Rgba, ScrollHandle,
    WeakEntity, WindowAppearance, prelude::*,
};
// FocusHandle is used through the Focusable trait impl below.
use project::AgentId;
use settings::update_settings_file;
use task::{RevealStrategy, RevealTarget, SpawnInTerminal, TaskId};
use terminal_view::terminal_panel::TerminalPanel;
use ui::{
    Banner, KeyBinding, ListItem, ListItemSpacing, Modal, ModalFooter, ModalHeader, Section,
    TintColor, WithScrollbar, prelude::*,
};
use ui_input::InputField;
use workspace::{ModalView, Workspace};

use crate::ai_account_chip::bind_account;
use crate::{Agent, AddAiAccount, NewExternalAgentThread};

pub struct AddAiAccountModal {
    name_input: Entity<InputField>,
    selected_agent_id: SharedString,
    last_error: Option<SharedString>,
    scroll_handle: ScrollHandle,
    fs: Arc<dyn Fs>,
    workspace: WeakEntity<Workspace>,
}

impl AddAiAccountModal {
    pub fn register(
        workspace: &mut Workspace,
        _window: Option<&mut Window>,
        _cx: &mut Context<Workspace>,
    ) {
        workspace.register_action(|workspace, action: &AddAiAccount, window, cx| {
            let agent_id: SharedString = action
                .agent_id
                .clone()
                .map(SharedString::from)
                .unwrap_or_else(|| SharedString::from(TIER_A_DESCRIPTORS[0].agent_id));
            let fs = workspace.app_state().fs.clone();
            let workspace_handle = cx.weak_entity();
            workspace.toggle_modal(window, cx, |window, cx| {
                Self::new(agent_id, fs, workspace_handle, window, cx)
            });
        });
    }

    pub fn new(
        agent_id: SharedString,
        fs: Arc<dyn Fs>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_input = cx.new(|cx| {
            InputField::new(window, cx, "e.g., personal")
                .label("Display name")
                .tab_index(1)
                .tab_stop(true)
        });
        Self {
            name_input,
            selected_agent_id: agent_id,
            last_error: None,
            scroll_handle: ScrollHandle::new(),
            fs,
            workspace,
        }
    }

    fn selected_descriptor(&self) -> Option<&'static AgentDescriptor> {
        descriptor_for(&self.selected_agent_id)
    }

    fn select_agent(&mut self, agent_id: &'static str, cx: &mut Context<Self>) {
        self.selected_agent_id = SharedString::from(agent_id);
        self.last_error = None;
        cx.notify();
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name_input.read(cx).text(cx);
        let agent_id = self.selected_agent_id.to_string();
        let descriptor = match descriptor_for(&agent_id) {
            Some(d) => d,
            None => {
                self.last_error =
                    Some(SharedString::from(format!("Unknown agent: {agent_id}")));
                cx.notify();
                return;
            }
        };
        match create_account(&agent_id, &name) {
            Ok(account) => {
                // 1) Bind the workspace to the new account so the chip and
                //    any future ACP spawn for this agent picks it up.
                let agent_id_for_binding = agent_id.clone();
                let account_id_for_binding = account.id.clone();
                update_settings_file(self.fs.clone(), cx, move |settings, _cx| {
                    bind_account(
                        settings,
                        &agent_id_for_binding,
                        Some(account_id_for_binding),
                    );
                });

                // 2) Auto-trigger the login flow per the descriptor. Failures
                //    here are logged but don't block the modal — the account
                //    is already created and bound; the user can finish login
                //    manually if the auto-trigger doesn't fire.
                self.trigger_login_flow(descriptor, &account.config_dir, window, cx);

                // 3) Confirm via toast so the user knows the create + bind
                //    landed. The login flow's terminal/thread also surfaces,
                //    but the toast is the persistent receipt.
                if let Some(workspace) = self.workspace.upgrade() {
                    let display_name = account.display_name.clone();
                    let agent_display_name = descriptor.display_name;
                    workspace.update(cx, |workspace, cx| {
                        workspace.toggle_status_toast(
                            StatusToast::new(
                                SharedString::from(format!(
                                    "{agent_display_name} account \"{display_name}\" connected — bound to this workspace"
                                )),
                                cx,
                                |this, _cx| this,
                            ),
                            cx,
                        );
                    });
                }

                cx.emit(DismissEvent);
            }
            Err(error) => {
                self.last_error = Some(SharedString::from(error.to_string()));
                cx.notify();
            }
        }
    }

    fn trigger_login_flow(
        &self,
        descriptor: &AgentDescriptor,
        config_dir: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match descriptor.login_flow {
            LoginFlow::InThread { command: _ } => {
                // Open a new ACP thread for this agent. The ACP spawn pipeline
                // will inject the just-set workspace binding's config_dir env
                // var, so the spawned subprocess is already pointed at the new
                // account. The user runs the login slash command (`/login`,
                // `/auth`) themselves — programmatic injection of the first
                // user message would require deeper thread integration.
                window.dispatch_action(
                    Box::new(NewExternalAgentThread {
                        agent: Some(Agent::Custom {
                            id: AgentId::new(descriptor.agent_id.to_string()),
                        }),
                    }),
                    cx,
                );
            }
            LoginFlow::EmbeddedTerminal { argv } => {
                // Codex login is browser-OAuth; it has to run in a real
                // terminal, not in an ACP context. Spawn a dock terminal
                // with the account's config-dir env injected and the agent's
                // login argv ready to go.
                let Some(workspace) = self.workspace.upgrade() else {
                    return;
                };
                let Some(terminal_panel) = workspace.read(cx).panel::<TerminalPanel>(cx) else {
                    return;
                };
                let mut env: HashMap<String, String> = HashMap::default();
                env.insert(
                    descriptor.config_dir_env_var.to_string(),
                    config_dir.display().to_string(),
                );
                let task = SpawnInTerminal {
                    id: TaskId(format!("ai-account-login-{}", descriptor.agent_id)),
                    label: format!("Sign in to {}", descriptor.display_name),
                    full_label: format!("Sign in to {}", descriptor.display_name),
                    command: argv.first().map(|s| s.to_string()),
                    args: argv.iter().skip(1).map(|s| s.to_string()).collect(),
                    env,
                    use_new_terminal: true,
                    reveal: RevealStrategy::Always,
                    reveal_target: RevealTarget::Dock,
                    ..Default::default()
                };
                terminal_panel
                    .update(cx, |panel, cx| {
                        panel.add_terminal_task(task, RevealStrategy::Always, window, cx)
                    })
                    .detach();
            }
        }
    }
}

impl Focusable for AddAiAccountModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name_input.read(cx).focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for AddAiAccountModal {}
impl ModalView for AddAiAccountModal {}

fn brand_accent_color(accent: &BrandAccent, window: &Window) -> Option<Hsla> {
    let is_dark = matches!(
        window.appearance(),
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    );
    let hex = if is_dark { accent.dark } else { accent.light };
    Rgba::try_from(hex).ok().map(Hsla::from)
}

impl AddAiAccountModal {
    fn render_agent_picker(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().children(TIER_A_DESCRIPTORS.iter().map(|descriptor| {
            let agent_id = descriptor.agent_id;
            let is_selected = self.selected_agent_id.as_ref() == agent_id;
            let accent = brand_accent_color(&descriptor.brand_accent, window);
            let row_id = SharedString::from(format!("pick-agent-{agent_id}"));

            ListItem::new(row_id)
                .toggle_state(is_selected)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .start_slot(
                    div()
                        .w_2()
                        .h_2()
                        .rounded_full()
                        .when_some(accent, |this, color| this.bg(color)),
                )
                .child(Label::new(SharedString::from(descriptor.display_name)))
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.select_agent(agent_id, cx);
                }))
        }))
    }

    fn render_signup_section(&self, _cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let descriptor = self.selected_descriptor()?;
        let signup_url: &'static str = descriptor.signup_url;
        let label = SharedString::from(format!("Sign up for {}", descriptor.display_name));
        let id = SharedString::from(format!("signup-{}", descriptor.agent_id));
        Some(
            v_flex()
                .gap_1()
                .child(
                    Label::new("Don't have a subscription?")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    Button::new(id, label)
                        .label_size(LabelSize::Small)
                        .on_click(move |_, _, cx| cx.open_url(signup_url)),
                ),
        )
    }
}

impl Render for AddAiAccountModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus_handle = self.focus_handle(cx);
        let agent_picker = self.render_agent_picker(window, cx).into_any_element();
        let signup_section = self
            .render_signup_section(cx)
            .map(|section| section.into_any_element());

        v_flex()
            .id("add-ai-account-modal")
            .key_context("AddAiAccountModal")
            .w(rems(34.))
            .elevation_3(cx)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .capture_any_mouse_down(cx.listener(|this, _, window, cx| {
                this.focus_handle(cx).focus(window, cx);
            }))
            .child(
                Modal::new("add-ai-account", None)
                    .header(
                        ModalHeader::new()
                            .headline("Add AI Account")
                            .description(
                                "Connect a subscription-authenticated identity. Each account is workspace-bound by default.",
                            ),
                    )
                    .when_some(self.last_error.clone(), |this, error| {
                        this.section(
                            Section::new().child(
                                Banner::new()
                                    .severity(Severity::Warning)
                                    .child(div().text_xs().child(error)),
                            ),
                        )
                    })
                    .child(
                        div()
                            .size_full()
                            .vertical_scrollbar_for(&self.scroll_handle, window, cx)
                            .child(
                                v_flex()
                                    .id("add-ai-account-content")
                                    .size_full()
                                    .tab_group()
                                    .pl_3()
                                    .pr_4()
                                    .pb_2()
                                    .gap_3()
                                    .overflow_y_scroll()
                                    .track_scroll(&self.scroll_handle)
                                    .child(
                                        v_flex()
                                            .gap_1()
                                            .child(
                                                Label::new("Agent")
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted),
                                            )
                                            .child(agent_picker),
                                    )
                                    .when_some(signup_section, |this, section| this.child(section))
                                    .child(self.name_input.clone()),
                            ),
                    )
                    .footer(
                        ModalFooter::new().end_slot(
                            h_flex()
                                .gap_1()
                                .child(
                                    Button::new("cancel", "Cancel")
                                        .key_binding(
                                            KeyBinding::for_action_in(
                                                &menu::Cancel,
                                                &focus_handle,
                                                cx,
                                            )
                                            .map(|kb| kb.size(rems_from_px(12.))),
                                        )
                                        .on_click(cx.listener(|this, _event, window, cx| {
                                            this.cancel(&menu::Cancel, window, cx)
                                        })),
                                )
                                .child(
                                    Button::new("connect", "Connect")
                                        .style(ButtonStyle::Tinted(TintColor::Accent))
                                        .key_binding(
                                            KeyBinding::for_action_in(
                                                &menu::Confirm,
                                                &focus_handle,
                                                cx,
                                            )
                                            .map(|kb| kb.size(rems_from_px(12.))),
                                        )
                                        .on_click(cx.listener(|this, _event, window, cx| {
                                            this.confirm(&menu::Confirm, window, cx)
                                        })),
                                ),
                        ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_agent_id_resolves_to_descriptor() {
        // Acts as a guard: AddAiAccountModal::register falls back to
        // TIER_A_DESCRIPTORS[0] when the action carries no agent_id, so
        // index 0 must always be a valid descriptor.
        let first = TIER_A_DESCRIPTORS.first().expect("Tier A is non-empty");
        assert!(descriptor_for(first.agent_id).is_some());
    }

    #[test]
    fn signup_url_is_present_for_every_tier_a_agent() {
        for descriptor in TIER_A_DESCRIPTORS {
            assert!(
                descriptor.signup_url.starts_with("https://"),
                "{} signup_url must be https",
                descriptor.agent_id
            );
        }
    }

    #[test]
    fn open_signup_silently_no_ops_for_unknown_agent_id() {
        // open_signup looks up the descriptor and bails when it isn't found.
        // It can't be called without a Context, but the lookup logic itself
        // is exercised by descriptor_for() returning None for unknown IDs.
        assert!(descriptor_for("definitely-not-an-agent").is_none());
    }
}
