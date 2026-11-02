use ai_accounts::{
    AccountState, AgentDescriptor, AiAccount, CLAUDE_CODE_AGENT_ID, LoginFlow,
    api_key_keychain_url, builtin_descriptor, create_account, load_index,
    prepare_profile_for_api_key, save_index,
};
use anyhow::Context as _;
use collections::HashMap;
use gpui::{
    ClipboardItem, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, KeyDownEvent,
    ScrollHandle, WeakEntity, prelude::*,
};
use notifications::status_toast::StatusToast;
// FocusHandle is used through the Focusable trait impl below.
use project::{AgentId, Project};
use task::{RevealStrategy, RevealTarget, SpawnInTerminal, TaskId};
use terminal_view::terminal_panel::TerminalPanel;
use ui::{
    Banner, KeyBinding, ListItem, ListItemSpacing, Modal, ModalFooter, ModalHeader, Section,
    TintColor, WithScrollbar, prelude::*,
};
use ui_input::InputField;
use workspace::{ModalView, Workspace};

use crate::agent_panel::AgentPanel;
use crate::ai_account_chip::{account_catalog, bind_account_to_workspace, brand_accent_color};
use crate::{AddAiAccount, NewExternalAgentThread};

pub struct AddAiAccountModal {
    name_input: Entity<InputField>,
    /// Optional API key entry, shown only for agents that accept one in place
    /// of their own sign-in (Gemini). Masked.
    api_key_input: Entity<InputField>,
    /// Agents that can hold isolated accounts and can be launched: the
    /// built-in agents plus installed agents with a profile contract.
    agents: Vec<AgentDescriptor>,
    selected_agent_id: SharedString,
    last_error: Option<SharedString>,
    scroll_handle: ScrollHandle,
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
                .unwrap_or_else(|| SharedString::from(CLAUDE_CODE_AGENT_ID));
            let workspace_handle = cx.weak_entity();
            let project = workspace.project().clone();
            workspace.toggle_modal(window, cx, |window, cx| {
                Self::new(agent_id, workspace_handle, &project, window, cx)
            });
        });
    }

    pub fn new(
        agent_id: SharedString,
        workspace: WeakEntity<Workspace>,
        project: &Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_input = cx.new(|cx| {
            InputField::new(window, cx, "e.g., personal")
                .label("Display name")
                .tab_index(1)
                .tab_stop(true)
        });
        let api_key_input = cx.new(|cx| {
            InputField::new(window, cx, "Paste your API key")
                .label("API key (optional)")
                .masked(true)
                .tab_index(2)
                .tab_stop(true)
        });
        let agents = account_catalog(project, &load_index(), cx)
            .into_iter()
            .filter(|entry| entry.installed || builtin_descriptor(&entry.agent_id).is_some())
            .filter_map(|entry| entry.descriptor().cloned())
            .collect();
        Self {
            name_input,
            api_key_input,
            agents,
            selected_agent_id: agent_id,
            last_error: None,
            scroll_handle: ScrollHandle::new(),
            workspace,
        }
    }

    fn selected_descriptor(&self) -> Option<&AgentDescriptor> {
        self.agents
            .iter()
            .find(|descriptor| descriptor.agent_id == self.selected_agent_id)
    }

    fn select_agent(&mut self, agent_id: SharedString, cx: &mut Context<Self>) {
        self.selected_agent_id = agent_id;
        self.last_error = None;
        cx.notify();
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name_input.read(cx).text(cx);
        let agent_id = self.selected_agent_id.to_string();
        let descriptor = match self.selected_descriptor().cloned() {
            Some(d) => d,
            None => {
                self.last_error = Some(SharedString::from(format!("Unknown agent: {agent_id}")));
                cx.notify();
                return;
            }
        };

        // A pasted key replaces the agent's own sign-in for agents that accept
        // one. Left blank, the account signs in through the agent.
        let api_key = descriptor
            .profile
            .api_key
            .as_ref()
            .map(|_| self.api_key_input.read(cx).text(cx).trim().to_string())
            .filter(|key| !key.is_empty());

        match create_account(&descriptor, &name, api_key.is_some()) {
            Ok(account) => {
                if let Some(key) = api_key {
                    // Save the key, bind the account, confirm via toast. No
                    // sign-in thread or terminal. connect_api_key_account owns
                    // the toast and dismiss for this path.
                    self.connect_api_key_account(&descriptor, account, key, cx);
                    return;
                }
                match descriptor.profile.login_flow {
                    LoginFlow::AgentAuth => {
                        // These agents authenticate inside an ACP thread using
                        // the methods they advertise. Drive the whole bind -> restart -> open
                        // thread sequence from the agent panel: it binds the
                        // account to this workspace, restarts the
                        // agent's subprocess so the new (empty) config dir is
                        // actually used, then opens a thread. The empty config
                        // dir is unauthenticated, so the agent reports
                        // auth-required and the thread surfaces the real
                        // code-based sign-in. The panel outlives this modal.
                        let panel = self
                            .workspace
                            .upgrade()
                            .and_then(|workspace| workspace.read(cx).panel::<AgentPanel>(cx));
                        match panel {
                            Some(panel) => panel.update(cx, |panel, cx| {
                                panel.connect_new_ai_account_in_thread(
                                    agent_id.clone(),
                                    account.id.clone(),
                                    window,
                                    cx,
                                );
                            }),
                            None => {
                                // No agent panel (shouldn't happen): at least
                                // record the binding so the account becomes the
                                // workspace default for the next thread.
                                bind_account_to_workspace(
                                    &self.workspace,
                                    &agent_id,
                                    Some(account.id.clone()),
                                    cx,
                                );
                            }
                        }
                    }
                    LoginFlow::EmbeddedTerminal { .. } => {
                        // Codex signs in through a real terminal that gets the
                        // config-dir env injected explicitly, so it does not
                        // depend on the cached ACP connection. Bind for the chip,
                        // then spawn the login terminal.
                        bind_account_and_restart_agent(
                            &self.workspace,
                            agent_id.clone(),
                            account.id.clone(),
                            cx,
                        );
                        self.trigger_login_flow(&descriptor, &account.config_dir, window, cx);
                    }
                }

                // Confirm via toast so the user knows the account was created
                // and bound. Sign-in then continues in the thread/terminal.
                if let Some(workspace) = self.workspace.upgrade() {
                    let display_name = account.display_name.clone();
                    let agent_display_name = descriptor.display_name.clone();
                    workspace.update(cx, |workspace, cx| {
                        workspace.toggle_status_toast(
                            StatusToast::new(
                                SharedString::from(format!(
                                    "{agent_display_name} account \"{display_name}\" added — bound to this workspace"
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

    /// Stores an API-key account's key in the system keychain, binds the account
    /// to this workspace, and confirms via toast. The bound account's key is
    /// injected into the agent subprocess at spawn (see `api_key_for_gemini_cli`
    /// in `agent_servers`), so no shell config is needed, but a running agent
    /// is restarted to pick it up.
    fn connect_api_key_account(
        &self,
        descriptor: &AgentDescriptor,
        account: ai_accounts::AiAccount,
        key: String,
        cx: &mut Context<Self>,
    ) {
        let credentials_provider = zed_credentials_provider::global(cx);
        let keychain_url = api_key_keychain_url(&account.agent_id, &account.id);
        let workspace = self.workspace.clone();
        let agent_id = account.agent_id.clone();
        let account_id = account.id.clone();
        let display_name = account.display_name;
        let agent_display_name = descriptor.display_name.clone();

        cx.spawn(async move |this, cx| {
            // Username must be non-empty: the macOS keychain read
            // (gpui_macos `read_credentials`) requires a `kSecAttrAccount`
            // attribute and errors with "account was missing from keychain
            // item" otherwise, so an empty username makes the key unreadable at
            // spawn. "Bearer" matches the convention used for every other API
            // key in the codebase (e.g. `language_model::ApiKey`).
            if let Err(error) = credentials_provider
                .write_credentials(&keychain_url, "Bearer", key.as_bytes(), &*cx)
                .await
            {
                this.update(cx, |this, cx| {
                    this.last_error = Some(SharedString::from(format!(
                        "Couldn't save the API key to the keychain: {error}"
                    )));
                    cx.notify();
                })
                .ok();
                return;
            }

            this.update(cx, |_this, cx| {
                bind_account_and_restart_agent(&workspace, agent_id, account_id, cx);
                if let Some(workspace) = workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        workspace.toggle_status_toast(
                            StatusToast::new(
                                SharedString::from(format!(
                                    "{agent_display_name} account \"{display_name}\" added and bound to this workspace. Open a {agent_display_name} thread to start."
                                )),
                                cx,
                                |this, _cx| this,
                            ),
                            cx,
                        );
                    });
                }
                cx.emit(DismissEvent);
            })
            .ok();
        })
        .detach();
    }

    fn trigger_login_flow(
        &self,
        descriptor: &AgentDescriptor,
        config_dir: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &descriptor.profile.login_flow {
            LoginFlow::AgentAuth => {
                // Open a new ACP thread for this agent. The ACP spawn pipeline
                // will inject the just-set workspace binding's profile env var,
                // so the spawned subprocess is already pointed at the new
                // account, and the agent's own ACP sign-in runs against it.
                window.dispatch_action(
                    Box::new(NewExternalAgentThread {
                        agent: AgentId::new(descriptor.agent_id.to_string()),
                    }),
                    cx,
                );
            }
            LoginFlow::EmbeddedTerminal { argv } => {
                // Terminal logins (Codex's browser OAuth, settings-defined
                // commands) have to run in a real terminal, not in an ACP
                // context. Spawn a dock terminal
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
                    descriptor.profile.profile_env_var.to_string(),
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

impl AddAiAccountModal {
    fn render_agent_picker(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().children(self.agents.iter().map(|descriptor| {
            let agent_id = descriptor.agent_id.clone();
            let is_selected = self.selected_agent_id == agent_id;
            let accent = brand_accent_color(descriptor.brand_accent.as_ref(), window);
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
                .child(Label::new(descriptor.display_name.clone()))
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.select_agent(agent_id.clone(), cx);
                }))
        }))
    }

    /// Copies the selected API-key agent's "get a key" URL to the clipboard so
    /// the user can open it in a non-default browser. No-op for other agents.
    fn copy_api_key_url(&self, cx: &mut Context<Self>) {
        let Some(descriptor) = self.selected_descriptor() else {
            return;
        };
        let Some(api_key) = &descriptor.profile.api_key else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(api_key.key_url.to_string()));
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.toggle_status_toast(
                    StatusToast::new(
                        SharedString::from("Link copied to clipboard"),
                        cx,
                        |this, _cx| this,
                    ),
                    cx,
                );
            });
        }
    }

    fn render_api_key_section(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let descriptor = self.selected_descriptor()?;
        let key_url = descriptor.profile.api_key.as_ref()?.key_url.clone();
        let get_key_id = SharedString::from(format!("get-api-key-{}", descriptor.agent_id));
        let copy_key_id = SharedString::from(format!("copy-api-key-url-{}", descriptor.agent_id));
        Some(
            v_flex()
                .gap_1()
                .child(self.api_key_input.clone())
                .child(
                    Label::new(SharedString::from(format!(
                        "Leave blank to sign in through {} in a thread. A key is stored in your system keychain and used only for this account.",
                        descriptor.display_name
                    )))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new(get_key_id, "Get an API key")
                                .label_size(LabelSize::Small)
                                .on_click(move |_, _, cx| cx.open_url(&key_url)),
                        )
                        .child(
                            Button::new(copy_key_id, "Copy link")
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|this, _, _, cx| this.copy_api_key_url(cx))),
                        ),
                )
                .child(
                    Label::new("Press C to copy the link to open it in another browser.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
        )
    }

    fn render_signup_section(&self, _cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let descriptor = self.selected_descriptor()?;
        // Agents that accept a key get a "Get an API key" link in the key
        // section instead.
        if descriptor.profile.api_key.is_some() {
            return None;
        }
        let signup_url = descriptor.signup_url.clone()?;
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
                        .on_click(move |_, _, cx| cx.open_url(&signup_url)),
                ),
        )
    }
}

impl Render for AddAiAccountModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus_handle = self.focus_handle(cx);
        let agent_picker = self.render_agent_picker(window, cx).into_any_element();
        let api_key_section = self
            .render_api_key_section(cx)
            .map(|section| section.into_any_element());
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
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                // Bare `c` copies the "get a key" URL so the user can open it in
                // a non-default browser. Skipped while the key field is focused
                // so typing a literal `c` into the key still works.
                if event.keystroke.key.as_str() == "c"
                    && !event.keystroke.modifiers.modified()
                    && !this.api_key_input.focus_handle(cx).is_focused(window)
                {
                    this.copy_api_key_url(cx);
                }
            }))
            .capture_any_mouse_down(cx.listener(|this, _, window, cx| {
                this.focus_handle(cx).focus(window, cx);
            }))
            .child(
                Modal::new("add-ai-account", None)
                    .header(
                        ModalHeader::new()
                            .headline("Add AI Account")
                            .description(
                                "Connect an account for an AI agent. Each account is workspace-bound by default.",
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
                                    .child(self.name_input.clone())
                                    .when_some(api_key_section, |this, section| {
                                        this.child(section)
                                    })
                                    .when_some(signup_section, |this, section| {
                                        this.child(section)
                                    }),
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
                                            .map(|kb| kb.size(rems_from_px(12.0_f32))),
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
                                            .map(|kb| kb.size(rems_from_px(12.0_f32))),
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

/// Gives an existing account a provider API key, or replaces the one it has,
/// for agents that accept one in place of their own sign-in (Gemini).
pub struct SetAiAccountKeyModal {
    api_key_input: Entity<InputField>,
    account: AiAccount,
    descriptor: AgentDescriptor,
    last_error: Option<SharedString>,
    is_saving: bool,
    workspace: WeakEntity<Workspace>,
}

impl SetAiAccountKeyModal {
    pub fn new(
        account: AiAccount,
        descriptor: AgentDescriptor,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let api_key_input = cx.new(|cx| {
            InputField::new(window, cx, "Paste your API key")
                .label("API key")
                .masked(true)
                .tab_index(1)
                .tab_stop(true)
        });
        Self {
            api_key_input,
            account,
            descriptor,
            last_error: None,
            is_saving: false,
            workspace,
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        if self.is_saving {
            return;
        }
        let key = self.api_key_input.read(cx).text(cx).trim().to_string();
        if key.is_empty() {
            self.last_error = Some("Paste a key to save.".into());
            cx.notify();
            return;
        }
        self.is_saving = true;
        self.last_error = None;
        cx.notify();

        let credentials_provider = zed_credentials_provider::global(cx);
        let keychain_url = api_key_keychain_url(&self.account.agent_id, &self.account.id);
        let account = self.account.clone();
        let descriptor = self.descriptor.clone();
        let workspace = self.workspace.clone();
        cx.spawn(async move |this, cx| {
            let saved = async {
                // Username must be non-empty: the macOS keychain read requires a
                // `kSecAttrAccount` attribute. See `connect_api_key_account`.
                credentials_provider
                    .write_credentials(&keychain_url, "Bearer", key.as_bytes(), &*cx)
                    .await
                    .context("saving the key to the keychain")?;
                cx.background_spawn({
                    let account = account.clone();
                    async move {
                        prepare_profile_for_api_key(&descriptor, &account.config_dir)?;
                        let mut index = load_index();
                        index.set_state(&account.id, AccountState::Authenticated)?;
                        save_index(&index)
                    }
                })
                .await
            }
            .await;

            this.update(cx, |this, cx| {
                if let Err(error) = saved {
                    log::error!("ai_accounts: setting the API key failed: {error:#}");
                    this.is_saving = false;
                    this.last_error = Some(format!("Couldn't save the key: {error:#}").into());
                    cx.notify();
                    return;
                }
                let Some(workspace) = workspace.upgrade() else {
                    cx.emit(DismissEvent);
                    return;
                };
                if let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) {
                    panel.update(cx, |panel, cx| {
                        panel.restart_if_account_in_use(&account.agent_id, &account.id, cx);
                    });
                }
                let message = format!(
                    "Saved an API key for \"{}\". New {} threads use it.",
                    account.display_name, this.descriptor.display_name
                );
                workspace.update(cx, |workspace, cx| {
                    workspace
                        .toggle_status_toast(StatusToast::new(message, cx, |this, _cx| this), cx);
                });
                cx.emit(DismissEvent);
            })
            .ok();
        })
        .detach();
    }
}

impl Focusable for SetAiAccountKeyModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.api_key_input.read(cx).focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for SetAiAccountKeyModal {}
impl ModalView for SetAiAccountKeyModal {}

impl Render for SetAiAccountKeyModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus_handle = self.focus_handle(cx);
        let key_url = self
            .descriptor
            .profile
            .api_key
            .as_ref()
            .map(|api_key| api_key.key_url.clone());

        v_flex()
            .id("set-ai-account-key-modal")
            .key_context("SetAiAccountKeyModal")
            .w(rems(30.))
            .elevation_3(cx)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .capture_any_mouse_down(cx.listener(|this, _, window, cx| {
                this.focus_handle(cx).focus(window, cx);
            }))
            .child(
                Modal::new("set-ai-account-key", None)
                    .header(
                        ModalHeader::new()
                            .headline(format!("API key for \"{}\"", self.account.display_name))
                            .description(format!(
                                "{} uses this key instead of its own sign-in. It's stored in your \
                                 system keychain and replaces any key the account had.",
                                self.descriptor.display_name
                            )),
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
                        v_flex()
                            .px_3()
                            .pb_2()
                            .gap_1()
                            .child(self.api_key_input.clone())
                            .when_some(key_url, |this, key_url| {
                                this.child(
                                    h_flex().child(
                                        Button::new("set-key-get-api-key", "Get an API key")
                                            .label_size(LabelSize::Small)
                                            .on_click(move |_, _, cx| cx.open_url(&key_url)),
                                    ),
                                )
                            }),
                    )
                    .footer(
                        ModalFooter::new().end_slot(
                            h_flex()
                                .gap_1()
                                .child(
                                    Button::new("set-key-cancel", "Cancel")
                                        .key_binding(
                                            KeyBinding::for_action_in(
                                                &menu::Cancel,
                                                &focus_handle,
                                                cx,
                                            )
                                            .map(|kb| kb.size(rems_from_px(12.0_f32))),
                                        )
                                        .on_click(cx.listener(|this, _event, window, cx| {
                                            this.cancel(&menu::Cancel, window, cx)
                                        })),
                                )
                                .child(
                                    Button::new("set-key-save", "Save Key")
                                        .style(ButtonStyle::Tinted(TintColor::Accent))
                                        .disabled(self.is_saving)
                                        .key_binding(
                                            KeyBinding::for_action_in(
                                                &menu::Confirm,
                                                &focus_handle,
                                                cx,
                                            )
                                            .map(|kb| kb.size(rems_from_px(12.0_f32))),
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
        // CLAUDE_CODE_AGENT_ID when the action carries no agent_id, and
        // built-in agents are always offered.
        assert!(builtin_descriptor(CLAUDE_CODE_AGENT_ID).is_some());
    }

    #[test]
    fn signup_url_is_present_for_every_builtin_agent() {
        for descriptor in ai_accounts::BUILTIN_AGENT_IDS
            .iter()
            .filter_map(|agent_id| builtin_descriptor(agent_id))
        {
            assert!(
                descriptor
                    .signup_url
                    .as_ref()
                    .is_some_and(|url| url.starts_with("https://")),
                "{} signup_url must be https",
                descriptor.agent_id
            );
        }
    }
}

/// Binds the account to the workspace and restarts the agent's connection.
/// Account config and keys are only read when the agent process spawns, so an
/// agent already running would otherwise keep using the previous account.
fn bind_account_and_restart_agent(
    workspace: &WeakEntity<Workspace>,
    agent_id: String,
    account_id: String,
    cx: &mut App,
) {
    let panel = workspace
        .upgrade()
        .and_then(|workspace| workspace.read(cx).panel::<AgentPanel>(cx));
    match panel {
        Some(panel) => panel.update(cx, |panel, cx| {
            panel.switch_ai_account(agent_id, account_id, cx);
        }),
        None => bind_account_to_workspace(workspace, &agent_id, Some(account_id), cx),
    }
}
