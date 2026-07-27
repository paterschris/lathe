//! Status-bar AWS profile selector. Hidden unless the machine has AWS
//! profiles configured (or one is already active), so it never shows up for
//! people who don't touch AWS.

use chrono::Utc;
use gpui::{
    App, Context, Empty, IntoElement, Render, SharedString, Task, WeakEntity, Window,
};
use project::terminals::ActiveAwsProfile;
use settings::Settings as _;
use ui::prelude::*;
use ui::{Button, ContextMenu, IconPosition, PopoverMenu, Tooltip};
use workspace::notifications::NotificationId;
use workspace::{
    HideStatusItem, StatusBarSettings, StatusItemView, Toast, Workspace, item::ItemHandle,
};

use crate::{AwsProfile, SessionStatus, discover_profiles, probe_session, run_login};

pub struct AwsProfileSelector {
    workspace: WeakEntity<Workspace>,
    profiles: Vec<AwsProfile>,
    session: SessionStatus,
    login_in_flight: bool,
    _login_task: Option<Task<()>>,
    _poll_task: Task<()>,
}

struct AwsToast;

impl AwsProfileSelector {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        Self {
            workspace: workspace.weak_handle(),
            profiles: Vec::new(),
            session: SessionStatus::Unknown,
            login_in_flight: false,
            _login_task: None,
            _poll_task: Self::spawn_poll(cx),
        }
    }

    fn spawn_poll(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let profiles = cx.background_spawn(async { discover_profiles() }).await;
                let active = cx.update(|cx| {
                    cx.try_global::<ActiveAwsProfile>()
                        .and_then(|state| state.profile.clone())
                });
                let session = match active {
                    Some(profile) => cx.background_spawn(probe_session(profile)).await,
                    None => SessionStatus::Unknown,
                };
                if this
                    .update(cx, |this, cx| {
                        this.profiles = profiles;
                        this.session = session;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                cx.background_executor()
                    .timer(crate::SESSION_POLL_INTERVAL)
                    .await;
            }
        })
    }

    // Replacing the poll task restarts its loop, so state changes probe the
    // session immediately instead of waiting out the current interval.
    fn refresh_now(&mut self, cx: &mut Context<Self>) {
        self._poll_task = Self::spawn_poll(cx);
    }

    fn state(cx: &App) -> ActiveAwsProfile {
        cx.try_global::<ActiveAwsProfile>()
            .cloned()
            .unwrap_or_default()
    }

    fn set_active(&mut self, profile: Option<SharedString>, cx: &mut Context<Self>) {
        let state = cx.default_global::<ActiveAwsProfile>();
        state.profile = profile.as_ref().map(|profile| profile.to_string());
        let v2_compat = state.v2_compat;
        crate::persist_state(cx);
        if v2_compat && let Some(profile) = profile.as_ref() {
            self.write_wrapper(profile.to_string(), cx);
        }
        self.session = SessionStatus::Unknown;
        self.refresh_now(cx);
        cx.notify();
    }

    fn toggle_v2_compat(&mut self, cx: &mut Context<Self>) {
        let state = cx.default_global::<ActiveAwsProfile>();
        state.v2_compat = !state.v2_compat;
        let enabled = state.v2_compat;
        let profile = state.profile.clone();
        crate::persist_state(cx);
        if enabled && let Some(profile) = profile {
            self.write_wrapper(profile, cx);
        }
        cx.notify();
    }

    fn write_wrapper(&mut self, profile: String, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        cx.spawn(async move |_, cx| {
            let result = cx
                .background_spawn(async move { crate::ensure_v2_wrapper(&profile) })
                .await;
            if let Err(error) = result {
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.show_toast(
                            Toast::new(
                                NotificationId::unique::<AwsToast>(),
                                format!("Failed to write the SDK v2 wrapper profile: {error}"),
                            ),
                            cx,
                        );
                    })
                    .ok();
            }
        })
        .detach();
    }

    fn login(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = Self::state(cx).profile else {
            return;
        };
        let sso = self
            .profiles
            .iter()
            .find(|candidate| candidate.name.as_ref() == profile)
            .is_some_and(|candidate| candidate.sso);
        let workspace = self.workspace.clone();
        self.login_in_flight = true;
        self._login_task = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(run_login(profile.clone(), sso)).await;
            let message = match &result {
                Ok(()) => format!("AWS login for '{profile}' succeeded"),
                Err(error) => format!("AWS login for '{profile}' failed: {error}"),
            };
            workspace
                .update(cx, |workspace, cx| {
                    workspace.show_toast(
                        Toast::new(NotificationId::unique::<AwsToast>(), message),
                        cx,
                    );
                })
                .ok();
            this.update(cx, |this, cx| {
                this.login_in_flight = false;
                this.refresh_now(cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn label(&self, state: &ActiveAwsProfile) -> SharedString {
        let Some(profile) = state.profile.as_ref() else {
            return "AWS".into();
        };
        if self.login_in_flight {
            return format!("AWS: {profile} (logging in...)").into();
        }
        match &self.session {
            SessionStatus::Unknown => format!("AWS: {profile}").into(),
            SessionStatus::CliMissing => format!("AWS: {profile} (aws CLI not found)").into(),
            SessionStatus::NotLoggedIn => format!("AWS: {profile} (logged out)").into(),
            SessionStatus::Active { expires_at } => match expires_at {
                Some(expires_at) => {
                    let remaining = *expires_at - Utc::now();
                    let minutes = remaining.num_minutes();
                    if minutes <= 0 {
                        format!("AWS: {profile} (expired)").into()
                    } else if minutes < 60 {
                        format!("AWS: {profile} ({minutes}m)").into()
                    } else {
                        format!("AWS: {profile} ({}h {}m)", minutes / 60, minutes % 60).into()
                    }
                }
                None => format!("AWS: {profile} (active)").into(),
            },
        }
    }
}

impl Render for AwsProfileSelector {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !StatusBarSettings::get_global(cx).aws_profile_selector_button {
            return Empty.into_any_element();
        }
        let state = Self::state(cx);
        if self.profiles.is_empty() && state.profile.is_none() {
            return Empty.into_any_element();
        }

        let label = self.label(&state);
        let selector = cx.entity().downgrade();
        let profiles = self.profiles.clone();
        let active = state.profile.clone();
        let v2_compat = state.v2_compat;
        let can_login = active.is_some() && !self.login_in_flight;

        PopoverMenu::new("aws-profile-selector")
            .trigger(
                Button::new("aws-profile-selector-trigger", label)
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::text(
                        "AWS profile for new terminals, tasks, and debug sessions",
                    )),
            )
            .menu(move |window, cx| {
                let selector = selector.clone();
                let profiles = profiles.clone();
                let active = active.clone();
                Some(ContextMenu::build(
                    window,
                    cx,
                    move |mut menu, _window, _cx| {
                        menu = menu.header("AWS Profile");
                        if profiles.is_empty() {
                            menu = menu.label("No profiles in ~/.aws/config");
                        }
                        for profile in profiles {
                            let toggled = active.as_deref() == Some(profile.name.as_ref());
                            let name = profile.name.clone();
                            let selector = selector.clone();
                            menu = menu.toggleable_entry(
                                profile.name.clone(),
                                toggled,
                                IconPosition::Start,
                                None,
                                move |_window, cx| {
                                    selector
                                        .update(cx, |selector, cx| {
                                            selector.set_active(Some(name.clone()), cx);
                                        })
                                        .ok();
                                },
                            );
                        }
                        menu = menu.separator();
                        if can_login {
                            let selector = selector.clone();
                            menu = menu.entry("Log In (opens browser)", None, move |_window, cx| {
                                selector
                                    .update(cx, |selector, cx| selector.login(cx))
                                    .ok();
                            });
                        }
                        menu = menu.toggleable_entry(
                            "SDK v2 Compat (credential_process)",
                            v2_compat,
                            IconPosition::Start,
                            None,
                            {
                                let selector = selector.clone();
                                move |_window, cx| {
                                    selector
                                        .update(cx, |selector, cx| selector.toggle_v2_compat(cx))
                                        .ok();
                                }
                            },
                        );
                        menu.entry("Deactivate", None, move |_window, cx| {
                            selector
                                .update(cx, |selector, cx| selector.set_active(None, cx))
                                .ok();
                        })
                    },
                ))
            })
            .into_any_element()
    }
}

impl StatusItemView for AwsProfileSelector {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        Some(HideStatusItem::new(|settings| {
            settings
                .status_bar
                .get_or_insert_default()
                .aws_profile_selector_button = Some(false);
        }))
    }
}
