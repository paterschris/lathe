use std::time::Duration;

use git::GitHostAuthKind;
use git::git_host_credentials::{self, GitHost};
use git_hosting_providers::{DeviceTokenPoll, fetch_login, poll_for_token, request_device_code};
use gpui::{
    ClipboardItem, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Task, WeakEntity,
};
use ui::prelude::*;
use ui_input::InputField;
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(connect);
    workspace.register_action(disconnect);
}

fn connect(
    workspace: &mut Workspace,
    action: &zed_actions::ConnectGitHost,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(host) = GitHost::resolve(cx, &action.host) else {
        return;
    };
    let workspace_weak = cx.weak_entity();
    workspace.toggle_modal(window, cx, |window, cx| {
        ConnectGitHostModal::new(host, workspace_weak, window, cx)
    });
}

fn disconnect(
    _workspace: &mut Workspace,
    action: &zed_actions::DisconnectGitHost,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let host = action.host.clone();
    cx.spawn(async move |workspace, cx| {
        if let Err(error) = git_host_credentials::clear(cx, &host).await {
            workspace
                .update(cx, |workspace, cx| workspace.show_error(error, cx))
                .log_err();
        }
    })
    .detach();
}

/// Which credential-entry flow this host gets. Chosen from the host's protocol
/// and whether it is the vendor's public instance, because the GitHub device
/// flow is backed by an OAuth app registered against `github.com` and cannot
/// serve an enterprise deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectFlow {
    /// GitHub.com: the browser opens and the user types a short code.
    GitHubDeviceFlow,
    /// A single pasted personal access token. GitHub Enterprise and all GitLab.
    Token,
    /// Username plus app password / API token, sent as HTTP Basic. Bitbucket.
    UsernameAndSecret,
}

impl ConnectFlow {
    fn for_host(host: &GitHost) -> ConnectFlow {
        match host.kind() {
            GitHostAuthKind::GitHub if host.is_public_instance() => ConnectFlow::GitHubDeviceFlow,
            GitHostAuthKind::GitHub | GitHostAuthKind::GitLab => ConnectFlow::Token,
            GitHostAuthKind::Bitbucket => ConnectFlow::UsernameAndSecret,
        }
    }
}

/// A modal for connecting a git hosting account. Works against any host the
/// provider registry knows how to authenticate, including enterprise and
/// self-hosted instances, not just the vendors' public ones.
pub struct ConnectGitHostModal {
    host: GitHost,
    flow: ConnectFlow,
    focus_handle: FocusHandle,
    error: Option<SharedString>,
    busy: bool,
    user_code: Option<SharedString>,
    verification_uri: Option<SharedString>,
    username_input: Entity<InputField>,
    secret_input: Entity<InputField>,
    _task: Option<Task<()>>,
}

impl ConnectGitHostModal {
    pub fn new(
        host: GitHost,
        _workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let flow = ConnectFlow::for_host(&host);
        let product = host.kind().product_name();
        // `tab_index` registers each field as a tab stop so the modal's
        // SelectNext/SelectPrevious handlers can move focus between them; the
        // secret is masked since it is a token or app password.
        let username_placeholder = format!("{product} username");
        let username_input = cx.new(|cx| {
            InputField::new(window, cx, username_placeholder.as_str())
                .label("Username")
                .tab_index(1)
        });
        let secret_label = match flow {
            ConnectFlow::UsernameAndSecret => "App password or API token",
            _ => "Personal access token",
        };
        let secret_input = cx.new(|cx| {
            InputField::new(window, cx, secret_label)
                .label(secret_label)
                .masked(true)
                .tab_index(2)
        });

        match flow {
            ConnectFlow::UsernameAndSecret => {
                window.focus(&username_input.focus_handle(cx), cx);
            }
            ConnectFlow::Token => {
                window.focus(&secret_input.focus_handle(cx), cx);
            }
            ConnectFlow::GitHubDeviceFlow => {}
        }

        let mut this = Self {
            host,
            flow,
            focus_handle: cx.focus_handle(),
            error: None,
            busy: false,
            user_code: None,
            verification_uri: None,
            username_input,
            secret_input,
            _task: None,
        };

        if flow == ConnectFlow::GitHubDeviceFlow {
            this.start_github_device_flow(cx);
        }
        this
    }

    /// Where the user creates a credential for this host. Built from the host's
    /// own base URL so an enterprise instance links to its own settings page
    /// rather than the vendor's public one.
    fn token_page_url(&self) -> Option<String> {
        let host = self.host.host();
        match self.host.kind() {
            GitHostAuthKind::GitHub => Some(format!(
                "https://{host}/settings/tokens/new?scopes=repo,read:org&description=Lathe"
            )),
            GitHostAuthKind::GitLab => Some(format!(
                "https://{host}/-/user_settings/personal_access_tokens?name=Lathe&scopes=api"
            )),
            GitHostAuthKind::Bitbucket => {
                Some(format!("https://{host}/account/settings/app-passwords/new"))
            }
        }
    }

    fn start_github_device_flow(&mut self, cx: &mut Context<Self>) {
        self.busy = true;
        let http_client = cx.http_client();
        let host = self.host.host().to_string();
        let task = cx.spawn(async move |this, cx| {
            let device = match request_device_code(&http_client).await {
                Ok(device) => device,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.busy = false;
                        this.error =
                            Some(format!("Could not start GitHub sign-in: {error:#}").into());
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };

            this.update(cx, |this, cx| {
                this.user_code = Some(device.user_code.clone().into());
                this.verification_uri = Some(device.verification_uri.clone().into());
                cx.open_url(&device.verification_uri);
                cx.notify();
            })
            .ok();

            let mut interval = device.interval.max(1);
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(interval))
                    .await;
                match poll_for_token(&http_client, &device.device_code).await {
                    Ok(DeviceTokenPoll::Pending) => continue,
                    Ok(DeviceTokenPoll::SlowDown) => {
                        interval += 5;
                        continue;
                    }
                    Ok(DeviceTokenPoll::Authorized(token)) => {
                        let login = fetch_login(&http_client, &token).await.unwrap_or_default();
                        let stored =
                            git_host_credentials::set(cx, &host, &login, &token).await;
                        this.update(cx, |this, cx| match stored {
                            Ok(()) => cx.emit(DismissEvent),
                            Err(error) => {
                                this.busy = false;
                                this.error =
                                    Some(format!("Could not save credential: {error}").into());
                                cx.notify();
                            }
                        })
                        .ok();
                        return;
                    }
                    Err(error) => {
                        this.update(cx, |this, cx| {
                            this.busy = false;
                            this.error = Some(format!("{error:#}").into());
                            cx.notify();
                        })
                        .ok();
                        return;
                    }
                }
            }
        });
        self._task = Some(task);
    }

    /// Stores a manually-entered credential. The username is only meaningful for
    /// Basic-auth hosts; token hosts resolve the account name from the host
    /// itself so the connected-account menu entry is still populated.
    fn submit_credential(&mut self, cx: &mut Context<Self>) {
        let needs_username = self.flow == ConnectFlow::UsernameAndSecret;
        let username = self.username_input.read(cx).text(cx).trim().to_string();
        let secret = self.secret_input.read(cx).text(cx).trim().to_string();
        let product = self.host.kind().product_name();
        if secret.is_empty() || (needs_username && username.is_empty()) {
            self.error = Some(
                if needs_username {
                    format!("Enter your {product} username and an app password or API token.")
                } else {
                    format!("Enter a {product} personal access token.")
                }
                .into(),
            );
            cx.notify();
            return;
        }
        self.busy = true;
        self.error = None;
        cx.notify();

        let host = self.host.clone();
        let auth = host.auth(username.clone(), secret.clone());
        let http_client = cx.http_client();
        let task = cx.spawn(async move |this, cx| {
            // Resolve the account name from the host so the title-bar menu can
            // show who is connected. Best-effort: a host that cannot report it
            // still connects, just without a name.
            let resolved_login = match cx.update(|cx| {
                git::GitHostingProviderRegistry::try_global(cx).map(|registry| {
                    registry
                        .list_hosting_providers()
                        .into_iter()
                        .find(|provider| provider.base_url().host_str() == Some(host.host()))
                })
            }) {
                Some(Some(provider)) => provider
                    .fetch_authenticated_user(Some(auth), http_client)
                    .await
                    .log_err()
                    .flatten(),
                _ => None,
            };
            let display_username = if needs_username {
                username
            } else {
                resolved_login
                    .map(|login| login.to_string())
                    .unwrap_or_default()
            };

            let stored =
                git_host_credentials::set(cx, host.host(), &display_username, &secret).await;
            this.update(cx, |this, cx| match stored {
                Ok(()) => cx.emit(DismissEvent),
                Err(error) => {
                    this.busy = false;
                    this.error = Some(format!("Could not save credential: {error}").into());
                    cx.notify();
                }
            })
            .ok();
        });
        self._task = Some(task);
    }

    fn on_confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        if self.flow != ConnectFlow::GitHubDeviceFlow && !self.busy {
            self.submit_credential(cx);
        }
    }

    fn on_tab(&mut self, _: &menu::SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        window.focus_next(cx);
    }

    fn on_tab_prev(
        &mut self,
        _: &menu::SelectPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus_prev(cx);
    }

    fn render_github_device_flow(&self, cx: &mut Context<Self>) -> AnyElement {
        match (self.user_code.clone(), self.verification_uri.clone()) {
            (Some(code), Some(uri)) => {
                let code_for_copy = code.to_string();
                v_flex()
                    .gap_2()
                    .child(Label::new(
                        "In the browser window that opened, enter this code to authorize Lathe:",
                    ))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Label::new(code).size(LabelSize::Large))
                            .child(Button::new("copy-code", "Copy").on_click(cx.listener(
                                move |_, _, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        code_for_copy.clone(),
                                    ));
                                },
                            ))),
                    )
                    .child(
                        Button::new("open-browser", "Open browser again").on_click(cx.listener(
                            move |_, _, _, cx| {
                                cx.open_url(&uri);
                            },
                        )),
                    )
                    .child(
                        Label::new("Waiting for authorization…")
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .into_any_element()
            }
            _ => Label::new("Starting GitHub sign-in…")
                .color(Color::Muted)
                .into_any_element(),
        }
    }

    fn render_credential_form(&self, cx: &mut Context<Self>) -> AnyElement {
        let needs_username = self.flow == ConnectFlow::UsernameAndSecret;
        let product = self.host.kind().product_name();
        let blurb = if needs_username {
            format!(
                "Paste a {product} app password or API token. Lathe stores it in your keychain."
            )
        } else {
            format!(
                "Paste a {product} personal access token with API access to your repositories. \
                 Lathe stores it in your keychain."
            )
        };
        let token_page = self.token_page_url();
        v_flex()
            .gap_2()
            .child(
                Label::new(blurb)
                    .color(Color::Muted)
                    .size(LabelSize::Small),
            )
            .when(needs_username, |this| {
                this.child(self.username_input.clone())
            })
            .child(self.secret_input.clone())
            .child(
                h_flex()
                    .gap_2()
                    .when_some(token_page, |this, url| {
                        this.child(Button::new("open-token-page", "Create token…").on_click(
                            cx.listener(move |_, _, _, cx| {
                                cx.open_url(&url);
                            }),
                        ))
                    })
                    .child(
                        Button::new("connect-host", "Connect")
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.submit_credential(cx);
                            })),
                    ),
            )
            .into_any_element()
    }
}

impl EventEmitter<DismissEvent> for ConnectGitHostModal {}

impl Focusable for ConnectGitHostModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for ConnectGitHostModal {}

impl Render for ConnectGitHostModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.flow {
            ConnectFlow::GitHubDeviceFlow => self.render_github_device_flow(cx),
            ConnectFlow::Token | ConnectFlow::UsernameAndSecret => self.render_credential_form(cx),
        };
        v_flex()
            .key_context("ConnectGitHostModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_confirm))
            .on_action(cx.listener(Self::on_tab))
            .on_action(cx.listener(Self::on_tab_prev))
            .elevation_3(cx)
            .w(rems(28.))
            .p_4()
            .gap_3()
            .child(
                Label::new(format!("Connect {}", self.host.display_name()))
                    .size(LabelSize::Large),
            )
            .child(body)
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).color(Color::Error).size(LabelSize::Small))
            })
            .child(
                h_flex().justify_end().child(
                    Button::new("close-connect-modal", "Cancel")
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                ),
            )
    }
}
