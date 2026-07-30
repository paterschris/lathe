//! Status-bar AWS profile selector. Hidden unless the machine has AWS
//! profiles configured (or one is already active), so it never shows up for
//! people who don't touch AWS.

use std::path::PathBuf;

use chrono::Utc;
use gpui::{
    App, Context, Empty, Entity, IntoElement, Render, SharedString, Task, WeakEntity, Window,
};
use project::Project;
use project::terminals::ActiveAwsProfile;
use settings::Settings as _;
use task::{HideStrategy, RevealStrategy, Shell, SpawnInTerminal, TaskId};
use terminal_view::terminal_panel::TerminalPanel;
use ui::prelude::*;
use ui::{Button, ContextMenu, IconPosition, PopoverMenu, PopoverMenuHandle, Tooltip};
use util::ResultExt as _;
use workspace::notifications::NotificationId;
use workspace::{
    HideStatusItem, OpenOptions, OpenVisible, StatusBarSettings, StatusItemView, Toast, Workspace,
    WorkspaceId, item::ItemHandle,
};

use crate::{AwsProfile, SessionStatus, discover_profiles, probe_session, run_login};

pub struct AwsProfileSelector {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    workspace_id: Option<WorkspaceId>,
    profiles: Vec<AwsProfile>,
    /// Profiles that have been selected in this workspace at some point. The
    /// menu shows only these (plus the active profile) so a window isn't
    /// cluttered with every profile in `~/.aws/config`; the rest sit behind a
    /// "Show All Profiles" entry.
    known_profiles: Vec<String>,
    show_all_profiles: bool,
    /// A `.aws/config` found at one of the project's worktree roots. When
    /// present it replaces the global config entirely: only its profiles are
    /// listed, and everything spawned gets `AWS_CONFIG_FILE` pointing at it.
    project_config: Option<PathBuf>,
    menu_handle: PopoverMenuHandle<ContextMenu>,
    session: SessionStatus,
    login_in_flight: bool,
    _login_task: Option<Task<()>>,
    _poll_task: Task<()>,
}

struct AwsToast;

impl AwsProfileSelector {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let workspace_id = workspace.database_id();
        if let Some(workspace_id) = workspace_id
            && let Some(state) = crate::restore_state(workspace_id, cx)
        {
            project.update(cx, |project, cx| {
                project.set_active_aws_profile(state, cx);
            });
        }
        let known_profiles = workspace_id
            .map(|workspace_id| crate::restore_known_profiles(workspace_id, cx))
            .unwrap_or_default();
        Self {
            workspace: workspace.weak_handle(),
            project,
            workspace_id,
            profiles: Vec::new(),
            known_profiles,
            show_all_profiles: false,
            project_config: None,
            menu_handle: PopoverMenuHandle::default(),
            session: SessionStatus::Unknown,
            login_in_flight: false,
            _login_task: None,
            _poll_task: Self::spawn_poll(cx),
        }
    }

    fn spawn_poll(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let worktree_roots = this
                    .read_with(cx, |this, cx| {
                        this.project
                            .read(cx)
                            .visible_worktrees(cx)
                            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                            .collect::<Vec<_>>()
                    })
                    .ok()
                    .unwrap_or_default();
                let (project_config, profiles) = cx
                    .background_spawn(async move {
                        let project_config = worktree_roots
                            .iter()
                            .map(|root| root.join(".aws").join("config"))
                            .find(|path| path.is_file());
                        let config_path = project_config
                            .clone()
                            .unwrap_or_else(crate::aws_config_path);
                        (project_config, discover_profiles(config_path))
                    })
                    .await;
                let active = this
                    .read_with(cx, |this, cx| {
                        this.project.read(cx).active_aws_profile().clone()
                    })
                    .ok();
                let session = match active.as_ref().and_then(|state| state.profile.clone()) {
                    Some(profile) => {
                        let config_file = active.and_then(|state| state.config_file);
                        cx.background_spawn(probe_session(profile, config_file)).await
                    }
                    None => SessionStatus::Unknown,
                };
                if this
                    .update(cx, |this, cx| {
                        this.profiles = profiles;
                        this.project_config = project_config;
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

    fn state(&self, cx: &App) -> ActiveAwsProfile {
        self.project.read(cx).active_aws_profile().clone()
    }

    fn update_state(
        &mut self,
        update: impl FnOnce(&mut ActiveAwsProfile),
        cx: &mut Context<Self>,
    ) -> ActiveAwsProfile {
        let state = self.project.update(cx, |project, cx| {
            let mut state = project.active_aws_profile().clone();
            update(&mut state);
            project.set_active_aws_profile(state.clone(), cx);
            state
        });
        if let Some(workspace_id) = self.workspace_id {
            crate::persist_state(workspace_id, &state, cx);
        }
        state
    }

    fn set_active(&mut self, profile: Option<SharedString>, cx: &mut Context<Self>) {
        // The shortlist only curates the global config's list; a project
        // config is already scoped, so there's nothing to remember.
        if self.project_config.is_none()
            && let Some(profile) = profile.as_ref()
        {
            self.remember_profile(profile.as_ref(), cx);
        }
        self.show_all_profiles = false;
        let config_file = if profile.is_some() {
            self.project_config.clone()
        } else {
            None
        };
        let state = self.update_state(
            |state| {
                state.profile = profile.as_ref().map(|profile| profile.to_string());
                state.config_file = config_file;
            },
            cx,
        );
        if state.v2_compat && let Some(profile) = profile.as_ref() {
            self.write_wrapper(profile.to_string(), cx);
        }
        self.session = SessionStatus::Unknown;
        self.refresh_now(cx);
        cx.notify();
    }

    fn remember_profile(&mut self, name: &str, cx: &mut Context<Self>) {
        if self.known_profiles.iter().any(|known| known == name) {
            return;
        }
        self.known_profiles.push(name.to_string());
        self.persist_known_profiles(cx);
    }

    fn reset_known_profiles(&mut self, cx: &mut Context<Self>) {
        self.known_profiles.clear();
        self.show_all_profiles = false;
        self.persist_known_profiles(cx);
        cx.notify();
    }

    fn persist_known_profiles(&mut self, cx: &mut Context<Self>) {
        if let Some(workspace_id) = self.workspace_id {
            crate::persist_known_profiles(workspace_id, &self.known_profiles, cx);
        }
    }

    fn toggle_v2_compat(&mut self, cx: &mut Context<Self>) {
        let state = self.update_state(|state| state.v2_compat = !state.v2_compat, cx);
        if state.v2_compat && let Some(profile) = state.profile {
            self.write_wrapper(profile, cx);
        }
        cx.notify();
    }

    fn write_wrapper(&mut self, profile: String, cx: &mut Context<Self>) {
        let config_path = self
            .state(cx)
            .config_file
            .unwrap_or_else(crate::aws_config_path);
        let workspace = self.workspace.clone();
        cx.spawn(async move |_, cx| {
            let result = cx
                .background_spawn(async move { crate::ensure_v2_wrapper(&profile, config_path) })
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
        let state = self.state(cx);
        let Some(profile) = state.profile else {
            return;
        };
        let config_file = state.config_file;
        // A profile that proxies another via `aws configure
        // export-credentials` can't hold a login session itself; the browser
        // login has to happen on the profile it chains to.
        let login_profile = self
            .profiles
            .iter()
            .find(|candidate| candidate.name.as_ref() == profile)
            .and_then(|candidate| candidate.chained_to.clone())
            .unwrap_or(profile);
        let sso = self
            .profiles
            .iter()
            .find(|candidate| candidate.name.as_ref() == login_profile)
            .is_some_and(|candidate| candidate.sso);
        let workspace = self.workspace.clone();
        self.login_in_flight = true;
        self._login_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(run_login(login_profile.clone(), sso, config_file))
                .await;
            let message = match &result {
                Ok(()) => format!("AWS login for '{login_profile}' succeeded"),
                Err(error) => format!("AWS login for '{login_profile}' failed: {error}"),
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

    /// The AWS CLI's own `aws configure sso` wizard handles naming, browser
    /// auth, and account/role selection, so profile creation is delegated to
    /// it in a terminal tab instead of reimplementing the flow as a modal.
    fn open_sso_wizard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(terminal_panel) = workspace.read(cx).panel::<TerminalPanel>(cx) else {
            return;
        };
        let mut task = SpawnInTerminal {
            id: TaskId("aws-configure-sso".to_string()),
            full_label: "aws configure sso".to_string(),
            label: "aws configure sso".to_string(),
            command: Some("aws".to_string()),
            args: vec!["configure".to_string(), "sso".to_string()],
            command_label: "aws configure sso".to_string(),
            use_new_terminal: true,
            reveal: RevealStrategy::Always,
            hide: HideStrategy::Never,
            shell: Shell::System,
            show_summary: true,
            show_command: true,
            ..Default::default()
        };
        // In a project-config window the wizard should create the profile in
        // the project's config file, not the global one.
        if let Some(config) = self.project_config.as_ref() {
            task.env.insert(
                "AWS_CONFIG_FILE".to_string(),
                config.to_string_lossy().into_owned(),
            );
        }
        terminal_panel
            .update(cx, |terminal_panel, cx| {
                terminal_panel.spawn_task(&task, window, cx)
            })
            .detach_and_log_err(cx);
    }

    fn open_aws_config(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let path = self
            .project_config
            .clone()
            .unwrap_or_else(crate::aws_config_path);
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).log_err();
            }
            std::fs::write(&path, "").log_err();
        }
        workspace.update(cx, |workspace, cx| {
            workspace
                .open_abs_path(
                    path,
                    OpenOptions {
                        visible: Some(OpenVisible::None),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
                .detach_and_log_err(cx);
        });
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
        let state = self.state(cx);
        if self.profiles.is_empty() && state.profile.is_none() {
            return Empty.into_any_element();
        }

        let label = self.label(&state);
        let selector = cx.entity().downgrade();
        let profiles = self.profiles.clone();
        let active = state.profile.clone();
        let v2_compat = state.v2_compat;
        let can_login = active.is_some() && !self.login_in_flight;
        let menu_handle = self.menu_handle.clone();
        let project_mode = self.project_config.is_some();

        PopoverMenu::new("aws-profile-selector")
            .with_handle(self.menu_handle.clone())
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
                let menu_handle = menu_handle.clone();
                // Rescan on open so profiles created since the last poll (via
                // the wizard or a config edit) show up on the next open.
                selector
                    .update(cx, |selector, cx| selector.refresh_now(cx))
                    .ok();
                // Read at open time rather than render time so the deferred
                // reopen from "Show All Profiles" builds with the new flag.
                let (show_all, known_profiles) = selector
                    .read_with(cx, |selector, _| {
                        (selector.show_all_profiles, selector.known_profiles.clone())
                    })
                    .unwrap_or((true, Vec::new()));
                let shortlist: Vec<AwsProfile> = profiles
                    .iter()
                    .filter(|profile| {
                        known_profiles
                            .iter()
                            .any(|known| known == profile.name.as_ref())
                            || active.as_deref() == Some(profile.name.as_ref())
                    })
                    .cloned()
                    .collect();
                let hidden_count = profiles.len() - shortlist.len();
                let (visible_profiles, hidden_count) =
                    if project_mode || show_all || shortlist.is_empty() {
                        (profiles, 0)
                    } else {
                        (shortlist, hidden_count)
                    };
                let has_shortlist = !project_mode && !known_profiles.is_empty();
                Some(ContextMenu::build(
                    window,
                    cx,
                    move |mut menu, _window, _cx| {
                        menu = menu.header("AWS Profile");
                        if project_mode {
                            menu = menu.label("From the project's .aws/config");
                        }
                        if visible_profiles.is_empty() {
                            menu = menu.label(if project_mode {
                                "No profiles in the project's .aws/config"
                            } else {
                                "No profiles in ~/.aws/config"
                            });
                        }
                        for profile in visible_profiles {
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
                        if hidden_count > 0 {
                            let selector = selector.clone();
                            let menu_handle = menu_handle.clone();
                            menu = menu.entry(
                                format!("Show All Profiles ({hidden_count} more)"),
                                None,
                                move |window, cx| {
                                    selector
                                        .update(cx, |selector, cx| {
                                            selector.show_all_profiles = true;
                                            cx.notify();
                                        })
                                        .ok();
                                    // Reopen after the click's dismiss settles
                                    // so the rebuilt menu shows the full list.
                                    let menu_handle = menu_handle.clone();
                                    window.defer(cx, move |window, cx| {
                                        menu_handle.show(window, cx);
                                    });
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
                        {
                            let selector = selector.clone();
                            menu = menu.entry("New SSO Profile (wizard)", None, move |window, cx| {
                                selector
                                    .update(cx, |selector, cx| {
                                        selector.open_sso_wizard(window, cx);
                                    })
                                    .ok();
                            });
                        }
                        {
                            let selector = selector.clone();
                            let label = if project_mode {
                                "Edit Project AWS Config"
                            } else {
                                "Edit AWS Config File"
                            };
                            menu = menu.entry(label, None, move |window, cx| {
                                selector
                                    .update(cx, |selector, cx| {
                                        selector.open_aws_config(window, cx);
                                    })
                                    .ok();
                            });
                        }
                        if has_shortlist {
                            let selector = selector.clone();
                            menu = menu.entry(
                                "Reset This Window's Profile List",
                                None,
                                move |_window, cx| {
                                    selector
                                        .update(cx, |selector, cx| {
                                            selector.reset_known_profiles(cx);
                                        })
                                        .ok();
                                },
                            );
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
