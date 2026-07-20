//! Mobile development panel for Lathe (Phase 2).
//!
//! Surfaces a bottom-dock panel with device picker, logcat, and build
//! controls when a React Native / Expo project is detected in the active
//! workspace. The ADB integration lives in [`adb`]; this module is the GPUI
//! surface that consumes it.

pub mod adb;
pub mod build;
mod device_picker;
pub mod expo_project;
pub mod toolchain;

pub use device_picker::MobileDeviceSelector;

use std::time::Duration;

use anyhow::Result;
use futures::StreamExt as _;
use futures::pin_mut;
use gpui::{
    App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Pixels, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, WeakEntity, Window, actions, div, px,
};
use project::Project;
use serde::{Deserialize, Serialize};
use settings::{RegisterSetting, Settings};
use ui::prelude::*;
use ui::{Color, Headline, HeadlineSize, IconName, Label, LabelSize, h_flex, v_flex};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

use crate::adb::{AdbDevice, AdbDeviceState, AdbTransport};
use crate::build::{BuildEvent, BuildKind, BuildOutcome, BuildSession};
use crate::expo_project::ExpoProject;

const PANEL_KEY: &str = "MobileDevPanel";
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(2);
const BUILD_OUTPUT_LINE_CAP: usize = 500;
const LOGCAT_LINE_CAP: usize = 1000;

#[derive(Debug, RegisterSetting)]
pub struct MobileDevPanelSettings {
    pub button: bool,
    pub dock: DockPosition,
    pub default_width: Pixels,
}

impl Settings for MobileDevPanelSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let panel = content.mobile_dev_panel.as_ref().unwrap();
        Self {
            button: panel.button.unwrap(),
            dock: panel.dock.unwrap().into(),
            default_width: panel.default_width.map(px).unwrap(),
        }
    }
}

actions!(
    mobile_dev,
    [
        /// Build the active mobile project as a debug variant and run it on the picked device.
        BuildAndRun,
        /// Build a preview APK in the cloud via EAS.
        BuildEasPreview,
        /// Install the most recent local APK on the picked device.
        InstallApk,
        /// Open the logcat tab in the Mobile panel.
        OpenLogcat,
        /// Cycle through connected devices.
        PickDevice,
        /// Pair a wireless ADB device.
        PairWirelessDevice,
        /// Download JDK 17 and the Android SDK into a Lathe-managed directory.
        InstallAndroidToolchain,
        /// Toggle the Mobile panel's focus.
        ToggleFocus,
    ]
);

/// One-time initialization. Currently wires action handlers; richer
/// project-detection and event subscription will land in later commits.
pub fn init(cx: &mut App) {
    cx.observe_new(register_workspace_actions).detach();
}

fn register_workspace_actions(
    workspace: &mut Workspace,
    _: Option<&mut Window>,
    _: &mut Context<Workspace>,
) {
    workspace
        .register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<MobileDevPanel>(window, cx);
        })
        .register_action(|workspace, _: &BuildAndRun, _, cx| {
            if let Some(panel) = workspace.panel::<MobileDevPanel>(cx) {
                panel.update(cx, |panel, cx| {
                    panel.start_build(BuildKind::LocalDebugRun, cx);
                });
            }
        })
        .register_action(|workspace, _: &BuildEasPreview, _, cx| {
            if let Some(panel) = workspace.panel::<MobileDevPanel>(cx) {
                panel.update(cx, |panel, cx| {
                    panel.start_build(BuildKind::EasPreview, cx);
                });
            }
        })
        .register_action(|_, _: &InstallApk, _, _| {
            log::warn!("mobile_dev: InstallApk is not yet implemented");
        })
        .register_action(|workspace, _: &OpenLogcat, window, cx| {
            if let Some(panel) = workspace.panel::<MobileDevPanel>(cx) {
                panel.update(cx, |panel, cx| {
                    panel.start_logcat(cx);
                });
            }
            workspace.toggle_panel_focus::<MobileDevPanel>(window, cx);
        })
        .register_action(|workspace, _: &PickDevice, window, cx| {
            if let Some(panel) = workspace.panel::<MobileDevPanel>(cx) {
                panel.update(cx, |panel, cx| {
                    panel.cycle_selected_device(cx);
                });
            }
            workspace.toggle_panel_focus::<MobileDevPanel>(window, cx);
        })
        .register_action(|_, _: &PairWirelessDevice, _, _| {
            log::warn!("mobile_dev: PairWirelessDevice is not yet implemented");
        })
        .register_action(|workspace, _: &InstallAndroidToolchain, window, cx| {
            if let Some(panel) = workspace.panel::<MobileDevPanel>(cx) {
                workspace.focus_panel::<MobileDevPanel>(window, cx);
                panel.update(cx, |panel, cx| {
                    panel.start_toolchain_install(cx);
                });
            }
        });
}

#[derive(Default, Serialize, Deserialize)]
#[allow(dead_code)] // wired up by the panel-serialization commit in Phase 2.
struct SerializedMobileDevPanel {
    width: Option<Pixels>,
}

/// What the device-tracking task most recently reported.
#[derive(Clone, Default)]
struct DeviceListState {
    devices: Vec<AdbDevice>,
    error: Option<SharedString>,
    /// `false` until the first successful poll, used to show a loading state.
    loaded: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum BuildStatus {
    Running,
    Success,
    Failure(SharedString),
}

/// Live state for an active logcat tail.
struct LogcatUiState {
    serial: SharedString,
    package: SharedString,
    pid: Option<u32>,
    lines: Vec<SharedString>,
    error: Option<SharedString>,
    _forwarder: Task<()>,
}

/// Live state for a running (or most-recent) managed toolchain install.
struct ToolchainInstallUiState {
    status: BuildStatus,
    lines: Vec<SharedString>,
    _forwarder: Task<()>,
}

/// Live state for the currently-running (or most-recent) build.
#[allow(dead_code)] // `kind` used by status-label rendering in a follow-up commit.
struct BuildUiState {
    kind: BuildKind,
    status: BuildStatus,
    lines: Vec<SharedString>,
    /// Detached forwarder task that pulls events off the BuildSession's
    /// channel and pushes into [`lines`]. Holding it keeps the task alive;
    /// dropping it (e.g. starting a new build) cancels the forwarder, which
    /// drops the BuildSession, which kills the underlying subprocess via
    /// `kill_on_drop`.
    _forwarder: Task<()>,
}

/// Bottom-dock panel for mobile development.
pub struct MobileDevPanel {
    workspace: WeakEntity<Workspace>,
    #[allow(dead_code)] // used by upcoming worktree-change subscription that re-runs detection.
    project: Entity<Project>,
    focus_handle: FocusHandle,
    width: Option<Pixels>,
    device_state: DeviceListState,
    /// Serial of the currently selected device, if any.
    selected_serial: Option<SharedString>,
    /// Detected Expo project metadata, if any worktree root contains an
    /// app.json with an `expo` key. `None` while detection is in flight or
    /// when the project is not a mobile project.
    expo_project: Option<ExpoProject>,
    /// `true` once the first detection pass has completed (so we can show
    /// the "not a mobile project" empty state instead of a perpetual
    /// loading spinner).
    project_scanned: bool,
    build_state: Option<BuildUiState>,
    logcat_state: Option<LogcatUiState>,
    toolchain_status: Option<toolchain::ToolchainStatus>,
    toolchain_install: Option<ToolchainInstallUiState>,
    /// Whether this panel already offered the toolchain install via a
    /// workspace notification (once per workspace session, so a dismissal
    /// isn't nagged).
    toolchain_offer_made: bool,
    _device_tracker: Task<()>,
    _project_detector: Task<()>,
    _toolchain_detector: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl MobileDevPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            cx.new(|cx| Self::new(workspace, window, cx))
        })?;
        Ok(panel)
    }

    fn new(workspace: &Workspace, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let project = workspace.project().clone();

        let device_tracker = cx.spawn(async move |this, cx| {
            let stream = adb::track_devices(DEVICE_POLL_INTERVAL, cx.background_executor().clone());
            pin_mut!(stream);
            while let Some(result) = stream.next().await {
                let Ok(_) = this.update(cx, |panel, cx| {
                    panel.apply_device_poll(result);
                    cx.notify();
                }) else {
                    break;
                };
            }
        });

        let project_detector = cx.spawn({
            let project = project.clone();
            async move |this, cx| {
                let worktree_roots: Vec<std::path::PathBuf> = this
                    .read_with(cx, |_, cx| {
                        project
                            .read(cx)
                            .visible_worktrees(cx)
                            .map(|wt| wt.read(cx).abs_path().to_path_buf())
                            .collect()
                    })
                    .unwrap_or_default();

                let detected = cx
                    .background_spawn(async move {
                        for root in worktree_roots {
                            if let Some(project) = expo_project::detect_at(&root) {
                                return Some(project);
                            }
                        }
                        None
                    })
                    .await;

                this.update(cx, |panel, cx| {
                    panel.expo_project = detected;
                    panel.project_scanned = true;
                    panel.maybe_offer_toolchain_install(cx);
                    cx.notify();
                })
                .ok();
            }
        });

        Self {
            workspace: workspace.weak_handle(),
            project,
            focus_handle,
            width: None,
            device_state: DeviceListState::default(),
            selected_serial: None,
            expo_project: None,
            project_scanned: false,
            build_state: None,
            logcat_state: None,
            toolchain_status: None,
            toolchain_install: None,
            toolchain_offer_made: false,
            _device_tracker: device_tracker,
            _project_detector: project_detector,
            _toolchain_detector: Self::spawn_toolchain_detection(cx),
            _subscriptions: Vec::new(),
        }
    }

    fn spawn_toolchain_detection(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let status = cx.background_spawn(async { toolchain::detect() }).await;
            this.update(cx, |panel, cx| {
                panel.toolchain_status = Some(status);
                panel.maybe_offer_toolchain_install(cx);
                cx.notify();
            })
            .ok();
        })
    }

    /// Once both detections have finished and this turns out to be an Expo
    /// project with toolchain components missing, offer the managed install
    /// via a workspace notification. Fires at most once per panel instance.
    fn maybe_offer_toolchain_install(&mut self, cx: &mut Context<Self>) {
        struct ToolchainInstallOffer;

        if self.toolchain_offer_made
            || self.expo_project.is_none()
            || self.toolchain_install.is_some()
        {
            return;
        }
        let Some(status) = self.toolchain_status.as_ref() else {
            return;
        };
        if status.all_present() {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };

        self.toolchain_offer_made = true;
        workspace.update(cx, |workspace, cx| {
            workspace.show_notification(
                NotificationId::unique::<ToolchainInstallOffer>(),
                cx,
                |cx| {
                    cx.new(|cx| {
                        MessageNotification::new(
                            "This is an Expo project, but some of the Android build tools it \
                             needs are missing. Lathe can download JDK 17 and the Android SDK \
                             into a managed directory and accept the SDK licenses for you.",
                            cx,
                        )
                        .with_title("Install Android toolchain?")
                        .primary_message("Install")
                        .primary_icon(IconName::Download)
                        .primary_on_click(|window, cx| {
                            window.dispatch_action(Box::new(InstallAndroidToolchain), cx);
                        })
                    })
                },
            );
        });
    }

    fn refresh_toolchain(&mut self, cx: &mut Context<Self>) {
        self._toolchain_detector = Self::spawn_toolchain_detection(cx);
    }

    /// Kick off (or ignore, if one is already running) a managed toolchain
    /// install, streaming its progress into the panel's toolchain section.
    pub fn start_toolchain_install(&mut self, cx: &mut Context<Self>) {
        if self
            .toolchain_install
            .as_ref()
            .is_some_and(|state| state.status == BuildStatus::Running)
        {
            return;
        }

        let session = toolchain::InstallSession::spawn(cx.http_client());
        let forwarder = cx.spawn(async move |this, cx| {
            let events = session.events();
            pin_mut!(events);
            while let Some(event) = events.next().await {
                let Ok(_) = this.update(cx, |panel, cx| {
                    match event {
                        toolchain::InstallEvent::Line(line) => {
                            if let Some(state) = panel.toolchain_install.as_mut() {
                                state.lines.push(line);
                                if state.lines.len() > BUILD_OUTPUT_LINE_CAP {
                                    let overflow = state.lines.len() - BUILD_OUTPUT_LINE_CAP;
                                    state.lines.drain(..overflow);
                                }
                            }
                        }
                        toolchain::InstallEvent::Finished(outcome) => {
                            if let Some(state) = panel.toolchain_install.as_mut() {
                                state.status = match outcome {
                                    toolchain::InstallOutcome::Success => BuildStatus::Success,
                                    toolchain::InstallOutcome::Failure(reason) => {
                                        BuildStatus::Failure(reason)
                                    }
                                };
                            }
                            panel.refresh_toolchain(cx);
                        }
                    }
                    cx.notify();
                }) else {
                    break;
                };
            }
        });

        self.toolchain_install = Some(ToolchainInstallUiState {
            status: BuildStatus::Running,
            lines: Vec::new(),
            _forwarder: forwarder,
        });
        cx.notify();
    }

    /// Start streaming logcat for the active project's package on the
    /// selected device. Resets any previous tail. Bails (with a warning) if
    /// the project, device, or package isn't known yet.
    pub fn start_logcat(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.expo_project.clone() else {
            log::warn!("mobile_dev: cannot start logcat without a detected Expo project");
            return;
        };
        let Some(package) = project.android_package else {
            log::warn!("mobile_dev: cannot start logcat; app.json has no expo.android.package");
            return;
        };
        let Some(device) = self
            .device_state
            .devices
            .iter()
            .find(|d| Some(&d.serial) == self.selected_serial.as_ref())
            .filter(|d| d.is_usable())
            .cloned()
        else {
            log::warn!("mobile_dev: cannot start logcat; no online device selected");
            return;
        };

        let serial = device.serial.clone();
        let package_owned = package.clone();

        let forwarder = cx.spawn(async move |this, cx| {
            let pid = adb::pid_of(&serial, &package_owned).await.ok().flatten();
            let stream = adb::logcat(&serial, pid);
            this.update(cx, |panel, cx| {
                if let Some(state) = panel.logcat_state.as_mut() {
                    state.pid = pid;
                    state.error = None;
                }
                cx.notify();
            })
            .ok();

            pin_mut!(stream);
            while let Some(item) = stream.next().await {
                let Ok(_) = this.update(cx, |panel, cx| {
                    let Some(state) = panel.logcat_state.as_mut() else {
                        return;
                    };
                    match item {
                        Ok(line) => {
                            state.lines.push(SharedString::from(line));
                            if state.lines.len() > LOGCAT_LINE_CAP {
                                let overflow = state.lines.len() - LOGCAT_LINE_CAP;
                                state.lines.drain(..overflow);
                            }
                        }
                        Err(err) => {
                            state.error = Some(SharedString::from(format!("{err:#}")));
                        }
                    }
                    cx.notify();
                }) else {
                    break;
                };
            }
        });

        self.logcat_state = Some(LogcatUiState {
            serial: device.serial,
            package: SharedString::from(package),
            pid: None,
            lines: Vec::new(),
            error: None,
            _forwarder: forwarder,
        });
        cx.notify();
    }

    /// Spawn the requested build and start streaming output into the panel.
    /// Bails (with a `log::warn!`) if no Expo project has been detected.
    pub fn start_build(&mut self, kind: BuildKind, cx: &mut Context<Self>) {
        let Some(project) = self.expo_project.clone() else {
            log::warn!("mobile_dev: cannot start build without a detected Expo project");
            return;
        };

        let device_serial = match kind {
            BuildKind::LocalDebugRun => self
                .device_state
                .devices
                .iter()
                .find(|d| Some(&d.serial) == self.selected_serial.as_ref())
                .filter(|d| d.is_usable())
                .map(|d| d.serial.clone()),
            BuildKind::EasPreview | BuildKind::EasProduction => None,
        };

        let toolchain_env = toolchain::build_env(self.toolchain_status.as_ref());
        let session = match BuildSession::spawn(kind, project.root, device_serial, toolchain_env) {
            Ok(session) => session,
            Err(err) => {
                log::error!("mobile_dev: failed to spawn build: {err:#}");
                self.build_state = Some(BuildUiState {
                    kind,
                    status: BuildStatus::Failure(SharedString::from(format!("{err:#}"))),
                    lines: Vec::new(),
                    _forwarder: cx.background_spawn(async {}),
                });
                cx.notify();
                return;
            }
        };

        let forwarder = cx.spawn(async move |this, cx| {
            let events = session.events();
            pin_mut!(events);
            while let Some(event) = events.next().await {
                let Ok(_) = this.update(cx, |panel, cx| {
                    if let Some(state) = panel.build_state.as_mut() {
                        match event {
                            BuildEvent::Line(line) => {
                                state.lines.push(line);
                                if state.lines.len() > BUILD_OUTPUT_LINE_CAP {
                                    let overflow = state.lines.len() - BUILD_OUTPUT_LINE_CAP;
                                    state.lines.drain(..overflow);
                                }
                            }
                            BuildEvent::Finished(outcome) => {
                                state.status = match outcome {
                                    BuildOutcome::Success => BuildStatus::Success,
                                    BuildOutcome::Failure(reason) => BuildStatus::Failure(reason),
                                };
                            }
                        }
                        cx.notify();
                    }
                }) else {
                    break;
                };
            }
            // Keep the session alive for the duration of this task so that
            // dropping the panel mid-build also kills the child.
            drop(session);
        });

        self.build_state = Some(BuildUiState {
            kind,
            status: BuildStatus::Running,
            lines: Vec::new(),
            _forwarder: forwarder,
        });
        cx.notify();
    }

    fn apply_device_poll(&mut self, result: Result<Vec<AdbDevice>>) {
        match result {
            Ok(devices) => {
                self.device_state.devices = devices;
                self.device_state.error = None;
                self.device_state.loaded = true;
                self.reconcile_selection();
            }
            Err(err) => {
                self.device_state.error = Some(SharedString::from(format!("{err:#}")));
                self.device_state.loaded = true;
            }
        }
    }

    fn reconcile_selection(&mut self) {
        if let Some(serial) = self.selected_serial.as_ref() {
            if !self
                .device_state
                .devices
                .iter()
                .any(|d| &d.serial == serial)
            {
                self.selected_serial = None;
            }
        }
        if self.selected_serial.is_none()
            && let Some(first) = self
                .device_state
                .devices
                .iter()
                .find(|d| d.is_usable())
                .or_else(|| self.device_state.devices.first())
        {
            self.selected_serial = Some(first.serial.clone());
        }
    }

    fn select_device(&mut self, serial: SharedString, cx: &mut Context<Self>) {
        self.selected_serial = Some(serial);
        cx.notify();
    }

    fn cycle_selected_device(&mut self, cx: &mut Context<Self>) {
        if self.device_state.devices.is_empty() {
            return;
        }
        let next_index = match self.selected_serial.as_ref().and_then(|serial| {
            self.device_state
                .devices
                .iter()
                .position(|d| &d.serial == serial)
        }) {
            Some(index) => (index + 1) % self.device_state.devices.len(),
            None => 0,
        };
        self.selected_serial = Some(self.device_state.devices[next_index].serial.clone());
        cx.notify();
    }

    fn render_devices_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut section = v_flex().gap_1().w_full().p_3();

        if !self.device_state.loaded {
            section = section.child(
                Label::new("Looking for devices...")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
            return section;
        }

        if let Some(err) = self.device_state.error.as_ref() {
            section = section.child(
                v_flex()
                    .gap_1()
                    .child(
                        Label::new("adb error")
                            .size(LabelSize::Small)
                            .color(Color::Error),
                    )
                    .child(
                        Label::new(err.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            );
            return section;
        }

        if self.device_state.devices.is_empty() {
            section = section.child(
                v_flex()
                    .gap_1()
                    .child(
                        Label::new("No devices connected")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(
                            "Plug a phone in over USB, or run the 'adb: pair wireless' task.",
                        )
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    ),
            );
            return section;
        }

        section = section.child(
            Label::new("Devices")
                .size(LabelSize::Small)
                .color(Color::Muted),
        );

        for (index, device) in self.device_state.devices.iter().enumerate() {
            section = section.child(self.render_device_row(index, device, cx));
        }

        section
    }

    fn render_device_row(
        &self,
        index: usize,
        device: &AdbDevice,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let serial_for_click = device.serial.clone();
        let selected = self.selected_serial.as_ref() == Some(&device.serial);
        let label = device.label();
        let state_label: SharedString = match device.state {
            AdbDeviceState::Online => "online".into(),
            AdbDeviceState::Offline => "offline".into(),
            AdbDeviceState::Unauthorized => "unauthorized".into(),
            AdbDeviceState::Other => "other".into(),
        };
        let state_color = match device.state {
            AdbDeviceState::Online => Color::Success,
            AdbDeviceState::Unauthorized => Color::Warning,
            AdbDeviceState::Offline | AdbDeviceState::Other => Color::Muted,
        };
        let transport_icon = match device.transport {
            AdbTransport::Usb => IconName::ArrowDownRight,
            AdbTransport::Wireless => IconName::SignalHigh,
        };

        h_flex()
            .id(("device-row", index))
            .w_full()
            .gap_2()
            .px_2()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .when(selected, |this| {
                this.bg(cx.theme().colors().element_selected)
            })
            .when(!selected, |this| {
                this.hover(|s| s.bg(cx.theme().colors().element_hover))
            })
            .child(
                ui::Icon::new(transport_icon)
                    .size(ui::IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(Label::new(label).size(LabelSize::Small).color(if selected {
                Color::Default
            } else {
                Color::Default
            }))
            .child(div().flex_1())
            .child(
                Label::new(state_label)
                    .size(LabelSize::XSmall)
                    .color(state_color),
            )
            .on_click(cx.listener(move |panel, _, _, cx| {
                panel.select_device(serial_for_click.clone(), cx);
            }))
    }
}

impl Focusable for MobileDevPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for MobileDevPanel {}

impl MobileDevPanel {
    fn render_logcat_section(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let state = self.logcat_state.as_ref()?;
        let border_color = cx.theme().colors().border;

        let visible_lines: Vec<SharedString> =
            state.lines.iter().rev().take(80).rev().cloned().collect();

        let pid_label = match state.pid {
            Some(pid) => SharedString::from(format!("pid {pid}")),
            None => SharedString::from("app not running"),
        };

        let header = h_flex()
            .gap_2()
            .child(
                ui::Icon::new(IconName::Terminal)
                    .size(ui::IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(Label::new("Logcat").size(LabelSize::Small))
            .child(
                Label::new(state.package.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .child(
                Label::new(state.serial.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Label::new(pid_label)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );

        let error_line = state.error.as_ref().map(|err| {
            Label::new(err.clone())
                .size(LabelSize::XSmall)
                .color(Color::Error)
        });

        let mut log = v_flex().w_full().gap_0p5();
        for line in visible_lines {
            log = log.child(Label::new(line).size(LabelSize::XSmall).color(Color::Muted));
        }

        Some(
            v_flex()
                .w_full()
                .gap_1()
                .px_3()
                .py_2()
                .border_t_1()
                .border_color(border_color)
                .child(header)
                .when_some(error_line, |this, line| this.child(line))
                .child(
                    div()
                        .id("mobile-logcat-output")
                        .h(px(160.))
                        .w_full()
                        .overflow_y_scroll()
                        .child(log),
                ),
        )
    }

    fn render_build_section(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let state = self.build_state.as_ref()?;
        let border_color = cx.theme().colors().border;
        let (status_label, status_color) = match &state.status {
            BuildStatus::Running => ("running...", Color::Info),
            BuildStatus::Success => ("succeeded", Color::Success),
            BuildStatus::Failure(_) => ("failed", Color::Error),
        };

        let visible_lines: Vec<SharedString> =
            state.lines.iter().rev().take(50).rev().cloned().collect();

        let header = h_flex()
            .gap_2()
            .child(Label::new(state.kind.label()).size(LabelSize::Small))
            .child(div().flex_1())
            .child(
                Label::new(status_label)
                    .size(LabelSize::XSmall)
                    .color(status_color),
            );

        let failure_line = if let BuildStatus::Failure(reason) = &state.status {
            Some(
                Label::new(reason.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Error),
            )
        } else {
            None
        };

        let mut log = v_flex().w_full().gap_0p5();
        for line in visible_lines {
            log = log.child(Label::new(line).size(LabelSize::XSmall).color(Color::Muted));
        }

        Some(
            v_flex()
                .w_full()
                .gap_1()
                .px_3()
                .py_2()
                .border_t_1()
                .border_color(border_color)
                .child(header)
                .when_some(failure_line, |this, line| this.child(line))
                .child(
                    div()
                        .id("mobile-build-output")
                        .h(px(160.))
                        .w_full()
                        .overflow_y_scroll()
                        .child(log),
                ),
        )
    }

    fn render_toolchain_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let border_color = cx.theme().colors().border;
        let mut section = v_flex()
            .w_full()
            .gap_1()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(border_color);

        let header = h_flex()
            .gap_2()
            .child(
                Label::new("Android toolchain")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(div().flex_1());

        let Some(status) = self.toolchain_status.as_ref() else {
            return section.child(header).child(
                Label::new("Checking toolchain...")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );
        };

        let installing = self
            .toolchain_install
            .as_ref()
            .is_some_and(|state| state.status == BuildStatus::Running);

        let header = if installing {
            header.child(
                Label::new("installing...")
                    .size(LabelSize::XSmall)
                    .color(Color::Info),
            )
        } else if status.all_present() {
            header.child(
                Label::new("ready")
                    .size(LabelSize::XSmall)
                    .color(Color::Success),
            )
        } else {
            header.child(
                Button::new("mobile-toolchain-install", "Install missing")
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _, _, cx| this.start_toolchain_install(cx))),
            )
        };
        section = section.child(header);

        for (name, component) in [
            ("JDK 17", &status.jdk),
            ("SDK command-line tools", &status.sdk),
            ("Platform tools (adb)", &status.platform_tools),
            ("SDK licenses", &status.licenses),
        ] {
            let (state_label, color) = match component {
                toolchain::ComponentStatus::Managed(_) => ("managed", Color::Success),
                toolchain::ComponentStatus::System(_) => ("system", Color::Default),
                toolchain::ComponentStatus::Missing => ("missing", Color::Warning),
            };
            section = section.child(
                h_flex()
                    .gap_2()
                    .child(Label::new(name).size(LabelSize::XSmall))
                    .child(div().flex_1())
                    .child(Label::new(state_label).size(LabelSize::XSmall).color(color)),
            );
        }

        if let Some(install) = self.toolchain_install.as_ref() {
            if let BuildStatus::Failure(reason) = &install.status {
                section = section.child(
                    Label::new(reason.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                );
            }
            let mut log = v_flex().w_full();
            for line in install.lines.iter().rev().take(30).rev() {
                log = log.child(
                    Label::new(line.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                );
            }
            section = section.child(
                div()
                    .id("mobile-toolchain-output")
                    .h(px(120.))
                    .w_full()
                    .overflow_y_scroll()
                    .child(log),
            );
        }

        section
    }

    fn render_project_section(&self) -> impl IntoElement {
        let v = v_flex().w_full().gap_1().px_3().py_2();
        if !self.project_scanned {
            return v.child(
                Label::new("Scanning workspace for an Expo project...")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }
        match &self.expo_project {
            Some(project) => v
                .child(
                    Label::new("Project")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            ui::Icon::new(IconName::Folder)
                                .size(ui::IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(SharedString::from(project.display_name.clone()))
                                .size(LabelSize::Small),
                        ),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Label::new("Android package").size(LabelSize::XSmall).color(Color::Muted),
                        )
                        .child(
                            Label::new(SharedString::from(
                                project
                                    .android_package
                                    .clone()
                                    .unwrap_or_else(|| "(not set)".to_string()),
                            ))
                            .size(LabelSize::XSmall)
                            .color(if project.android_package.is_some() {
                                Color::Default
                            } else {
                                Color::Warning
                            }),
                        ),
                ),
            None => v
                .child(
                    Label::new("No Expo project detected")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(
                        "Open a worktree whose root contains an app.json with an `expo` key to enable build and install actions.",
                    )
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                ),
        }
    }
}

impl Render for MobileDevPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border_color = cx.theme().colors().border;
        v_flex()
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .w_full()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(border_color)
                    .child(
                        ui::Icon::new(IconName::ToolHammer)
                            .size(ui::IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Headline::new("Mobile development").size(HeadlineSize::XSmall))
                    .child(div().flex_1())
                    .child(
                        Label::new(format!("{} device(s)", self.device_state.devices.len()))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(self.render_project_section())
            .child(self.render_toolchain_section(cx))
            .child(self.render_devices_section(cx))
            .when_some(self.render_build_section(cx), |this, section| {
                this.child(section)
            })
            .when_some(self.render_logcat_section(cx), |this, section| {
                this.child(section)
            })
    }
}

impl Panel for MobileDevPanel {
    fn persistent_name() -> &'static str {
        "MobileDevPanel"
    }

    fn panel_key() -> &'static str {
        PANEL_KEY
    }

    fn position(&self, _: &Window, cx: &App) -> DockPosition {
        MobileDevPanelSettings::get_global(cx).dock
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Bottom | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        settings::update_settings_file(<dyn fs::Fs>::global(cx), cx, move |settings, _| {
            settings.mobile_dev_panel.get_or_insert_default().dock = Some(position.into())
        });
    }

    fn default_size(&self, _: &Window, cx: &App) -> Pixels {
        self.width
            .unwrap_or_else(|| MobileDevPanelSettings::get_global(cx).default_width)
    }

    fn icon(&self, _: &Window, cx: &App) -> Option<IconName> {
        Some(IconName::ToolHammer).filter(|_| MobileDevPanelSettings::get_global(cx).button)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Mobile development")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        // Must be unique across all registered panels (dock.rs asserts this on
        // startup). 6 collides with GitActivityPanel; 8 is the next free slot
        // after the debugger panel (7).
        8
    }
}
