use std::time::Duration;

use git::git_host_credentials::{self, GitHostKind};
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
    let Some(kind) = GitHostKind::from_host(&action.host) else {
        return;
    };
    let workspace_weak = cx.weak_entity();
    workspace.toggle_modal(window, cx, |window, cx| {
        ConnectGitHostModal::new(kind, workspace_weak, window, cx)
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

/// A modal for connecting a git hosting account. GitHub uses the OAuth device
/// flow (the browser opens and the user enters a code); Bitbucket Cloud takes a
/// pasted username and app password / API token.
pub struct ConnectGitHostModal {
    kind: GitHostKind,
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
        kind: GitHostKind,
        _workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // `tab_index` registers each field as a tab stop so the modal's
        // SelectNext/SelectPrevious handlers can move focus between them; the
        // secret is masked since it is an app password or API token.
        let username_input = cx.new(|cx| {
            InputField::new(window, cx, "Bitbucket username")
                .label("Username")
                .tab_index(1)
        });
        let secret_input = cx.new(|cx| {
            InputField::new(window, cx, "App password or API token")
                .label("App password or API token")
                .masked(true)
                .tab_index(2)
        });

        if kind == GitHostKind::BitbucketCloud {
            window.focus(&username_input.focus_handle(cx), cx);
        }

        let mut this = Self {
            kind,
            focus_handle: cx.focus_handle(),
            error: None,
            busy: false,
            user_code: None,
            verification_uri: None,
            username_input,
            secret_input,
            _task: None,
        };

        if kind == GitHostKind::GitHub {
            this.start_github_device_flow(cx);
        }
        this
    }

    fn start_github_device_flow(&mut self, cx: &mut Context<Self>) {
        self.busy = true;
        let http_client = cx.http_client();
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
                            git_host_credentials::set(cx, GitHostKind::GitHub.host(), &login, &token)
                                .await;
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

    fn submit_bitbucket(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let username = self.username_input.read(cx).text(cx).trim().to_string();
        let secret = self.secret_input.read(cx).text(cx).trim().to_string();
        if username.is_empty() || secret.is_empty() {
            self.error =
                Some("Enter your Bitbucket username and an app password or API token.".into());
            cx.notify();
            return;
        }
        self.busy = true;
        self.error = None;
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let stored =
                git_host_credentials::set(cx, GitHostKind::BitbucketCloud.host(), &username, &secret)
                    .await;
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

    fn render_github(&self, cx: &mut Context<Self>) -> AnyElement {
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

    fn render_bitbucket(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .gap_2()
            .child(
                Label::new(
                    "Paste a Bitbucket app password or API token. Lathe stores it in your keychain.",
                )
                .color(Color::Muted)
                .size(LabelSize::Small),
            )
            .child(self.username_input.clone())
            .child(self.secret_input.clone())
            .child(
                h_flex()
                    .gap_2()
                    .child(Button::new("open-token-page", "Create token…").on_click(
                        cx.listener(move |_, _, _, cx| {
                            cx.open_url("https://bitbucket.org/account/settings/app-passwords/new");
                        }),
                    ))
                    .child(
                        Button::new("connect-bitbucket", "Connect")
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.submit_bitbucket(window, cx);
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
        let body = match self.kind {
            GitHostKind::GitHub => self.render_github(cx),
            GitHostKind::BitbucketCloud => self.render_bitbucket(cx),
        };
        v_flex()
            .key_context("ConnectGitHostModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_tab))
            .on_action(cx.listener(Self::on_tab_prev))
            .elevation_3(cx)
            .w(rems(28.))
            .p_4()
            .gap_3()
            .child(Label::new(format!("Connect {}", self.kind.display_name())).size(LabelSize::Large))
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
