//! Mobile development panel for Lathe.
//!
//! Surfaces a bottom-dock panel with device picker, log tailing, and build
//! controls when a React Native / Expo project is detected in the active
//! workspace. The ADB integration lives in [`adb`], the iOS/Xcode
//! integration in [`apple`]; this module is the GPUI surface that consumes
//! them.

pub mod adb;
pub mod apple;
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
    App, AsyncWindowContext, Context, Div, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Pixels, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, WeakEntity, Window, actions, div, px,
};
use project::Project;
use serde::{Deserialize, Serialize};
use settings::{RegisterSetting, Settings};
use ui::prelude::*;
use ui::{Color, Headline, HeadlineSize, IconName, Label, LabelSize, Tooltip, h_flex, v_flex};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

use crate::adb::{AdbDevice, AdbDeviceState, AdbTransport};
use crate::apple::{AppleDevice, AppleDeviceKind, AppleDeviceState};
use crate::build::{BuildEvent, BuildKind, BuildOutcome, BuildSession, MobilePlatform};
use crate::expo_project::ExpoProject;

const PANEL_KEY: &str = "MobileDevPanel";
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Slower than the ADB cadence: `devicectl` takes noticeably longer per
/// invocation than `adb devices`.
const APPLE_DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(5);
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
        /// Build the active mobile project as a debug variant and run it on the picked device
        /// (Android or iOS, following the device selection).
        BuildAndRun,
        /// Build a preview APK for Android in the cloud via EAS.
        BuildEasPreview,
        /// Build a preview app for iOS in the cloud via EAS.
        BuildEasPreviewIos,
        /// Install the most recent local APK on the picked device.
        InstallApk,
        /// Stream the picked device's log in the Mobile panel.
        OpenLogcat,
        /// Cycle through connected devices.
        PickDevice,
        /// Pair a wireless ADB device.
        PairWirelessDevice,
        /// Download JDK 17 and the Android SDK into a Lathe-managed directory.
        InstallAndroidToolchain,
        /// Download the iOS simulator runtime via xcodebuild.
        InstallIosRuntime,
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
                    let platform = panel.selected_platform();
                    panel.start_build(BuildKind::LocalDebugRun, platform, cx);
                });
            }
        })
        .register_action(|workspace, _: &BuildEasPreview, _, cx| {
            if let Some(panel) = workspace.panel::<MobileDevPanel>(cx) {
                panel.update(cx, |panel, cx| {
                    panel.start_build(BuildKind::EasPreview, MobilePlatform::Android, cx);
                });
            }
        })
        .register_action(|workspace, _: &BuildEasPreviewIos, _, cx| {
            if let Some(panel) = workspace.panel::<MobileDevPanel>(cx) {
                panel.update(cx, |panel, cx| {
                    panel.start_build(BuildKind::EasPreview, MobilePlatform::Ios, cx);
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
        })
        .register_action(|workspace, _: &InstallIosRuntime, window, cx| {
            if let Some(panel) = workspace.panel::<MobileDevPanel>(cx) {
                workspace.focus_panel::<MobileDevPanel>(window, cx);
                panel.update(cx, |panel, cx| {
                    panel.start_ios_runtime_install(cx);
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

/// What the Apple device-tracking task most recently reported.
#[derive(Clone, Default)]
struct AppleDeviceListState {
    devices: Vec<AppleDevice>,
    error: Option<SharedString>,
    loaded: bool,
}

/// Identity of the device the user picked, across both platforms. `id` is
/// an ADB serial for Android and a CoreSimulator/CoreDevice UDID for iOS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SelectedDevice {
    pub(crate) platform: MobilePlatform,
    pub(crate) id: SharedString,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum BuildStatus {
    Running,
    Success,
    Failure(SharedString),
}

/// Live state for an active log tail (Android logcat or an iOS simulator's
/// unified log).
struct LogcatUiState {
    platform: MobilePlatform,
    /// Serial for Android, device name for iOS.
    device_label: SharedString,
    /// Android package id, or the iOS process-name filter.
    target: SharedString,
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
struct BuildUiState {
    kind: BuildKind,
    platform: MobilePlatform,
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
    apple_device_state: AppleDeviceListState,
    /// The currently selected device, if any.
    selected_device: Option<SelectedDevice>,
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
    apple_toolchain_status: Option<apple::AppleToolchainStatus>,
    /// Reuses the toolchain-install UI shape for the iOS simulator runtime
    /// download driven by `xcodebuild -downloadPlatform`.
    apple_install: Option<ToolchainInstallUiState>,
    /// Whether this panel already offered the toolchain install via a
    /// workspace notification (once per workspace session, so a dismissal
    /// isn't nagged).
    toolchain_offer_made: bool,
    _device_tracker: Task<()>,
    _apple_device_tracker: Task<()>,
    _project_detector: Task<()>,
    _toolchain_detector: Task<()>,
    _apple_toolchain_detector: Task<()>,
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

        let apple_device_tracker = if cfg!(target_os = "macos") {
            cx.spawn(async move |this, cx| {
                let stream = apple::track_devices(
                    APPLE_DEVICE_POLL_INTERVAL,
                    cx.background_executor().clone(),
                );
                pin_mut!(stream);
                while let Some(result) = stream.next().await {
                    let Ok(_) = this.update(cx, |panel, cx| {
                        panel.apply_apple_device_poll(result);
                        cx.notify();
                    }) else {
                        break;
                    };
                }
            })
        } else {
            Task::ready(())
        };

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
            apple_device_state: AppleDeviceListState::default(),
            selected_device: None,
            expo_project: None,
            project_scanned: false,
            build_state: None,
            logcat_state: None,
            toolchain_status: None,
            toolchain_install: None,
            apple_toolchain_status: None,
            apple_install: None,
            toolchain_offer_made: false,
            _device_tracker: device_tracker,
            _apple_device_tracker: apple_device_tracker,
            _project_detector: project_detector,
            _toolchain_detector: Self::spawn_toolchain_detection(cx),
            _apple_toolchain_detector: Self::spawn_apple_toolchain_detection(cx),
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

    fn spawn_apple_toolchain_detection(cx: &mut Context<Self>) -> Task<()> {
        if !cfg!(target_os = "macos") {
            return Task::ready(());
        }
        cx.spawn(async move |this, cx| {
            let status = cx.background_spawn(apple::detect()).await;
            this.update(cx, |panel, cx| {
                panel.apple_toolchain_status = Some(status);
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

    fn refresh_apple_toolchain(&mut self, cx: &mut Context<Self>) {
        self._apple_toolchain_detector = Self::spawn_apple_toolchain_detection(cx);
    }

    /// Forward install progress events into one of the panel's install-state
    /// fields (`target`), re-running the matching toolchain detection when
    /// the install finishes.
    fn spawn_install_forwarder(
        events: smol::channel::Receiver<toolchain::InstallEvent>,
        target: fn(&mut Self) -> &mut Option<ToolchainInstallUiState>,
        refresh: fn(&mut Self, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            pin_mut!(events);
            while let Some(event) = events.next().await {
                let Ok(_) = this.update(cx, |panel, cx| {
                    match event {
                        toolchain::InstallEvent::Line(line) => {
                            if let Some(state) = target(panel).as_mut() {
                                state.lines.push(line);
                                if state.lines.len() > BUILD_OUTPUT_LINE_CAP {
                                    let overflow = state.lines.len() - BUILD_OUTPUT_LINE_CAP;
                                    state.lines.drain(..overflow);
                                }
                            }
                        }
                        toolchain::InstallEvent::Finished(outcome) => {
                            if let Some(state) = target(panel).as_mut() {
                                state.status = match outcome {
                                    toolchain::InstallOutcome::Success => BuildStatus::Success,
                                    toolchain::InstallOutcome::Failure(reason) => {
                                        BuildStatus::Failure(reason)
                                    }
                                };
                            }
                            refresh(panel, cx);
                        }
                    }
                    cx.notify();
                }) else {
                    break;
                };
            }
        })
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
        let forwarder = Self::spawn_install_forwarder(
            session.events(),
            |panel| &mut panel.toolchain_install,
            |panel, cx| panel.refresh_toolchain(cx),
            cx,
        );

        self.toolchain_install = Some(ToolchainInstallUiState {
            status: BuildStatus::Running,
            lines: Vec::new(),
            _forwarder: forwarder,
        });
        cx.notify();
    }

    /// Kick off (or ignore, if one is already running) the iOS simulator
    /// runtime download, streaming its progress into the Apple toolchain
    /// section.
    pub fn start_ios_runtime_install(&mut self, cx: &mut Context<Self>) {
        if !cfg!(target_os = "macos") {
            log::warn!("mobile_dev: the iOS simulator runtime requires macOS");
            return;
        }
        if self
            .apple_install
            .as_ref()
            .is_some_and(|state| state.status == BuildStatus::Running)
        {
            return;
        }

        let session = apple::RuntimeInstallSession::spawn();
        let forwarder = Self::spawn_install_forwarder(
            session.events(),
            |panel| &mut panel.apple_install,
            |panel, cx| panel.refresh_apple_toolchain(cx),
            cx,
        );

        self.apple_install = Some(ToolchainInstallUiState {
            status: BuildStatus::Running,
            lines: Vec::new(),
            _forwarder: forwarder,
        });
        cx.notify();
    }

    /// Start streaming the selected device's log. Dispatches on the selected
    /// device's platform; resets any previous tail.
    pub fn start_logcat(&mut self, cx: &mut Context<Self>) {
        match self.selected_platform() {
            MobilePlatform::Android => self.start_android_logcat(cx),
            MobilePlatform::Ios => self.start_ios_log_stream(cx),
        }
    }

    /// Start streaming logcat for the active project's package on the
    /// selected device. Bails (with a warning) if the project, device, or
    /// package isn't known yet.
    fn start_android_logcat(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.expo_project.clone() else {
            log::warn!("mobile_dev: cannot start logcat without a detected Expo project");
            return;
        };
        let Some(package) = project.android_package else {
            log::warn!("mobile_dev: cannot start logcat; app.json has no expo.android.package");
            return;
        };
        let Some(device) = self
            .selected_android_device()
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
                    panel.push_log_item(item);
                    cx.notify();
                }) else {
                    break;
                };
            }
        });

        self.logcat_state = Some(LogcatUiState {
            platform: MobilePlatform::Android,
            device_label: device.serial,
            target: SharedString::from(package),
            pid: None,
            lines: Vec::new(),
            error: None,
            _forwarder: forwarder,
        });
        cx.notify();
    }

    /// Start streaming the unified log of the selected iOS simulator,
    /// filtered to the project's process. Physical iOS devices are not
    /// supported (CoreDevice has no stable log-streaming CLI).
    fn start_ios_log_stream(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.expo_project.clone() else {
            log::warn!("mobile_dev: cannot stream logs without a detected Expo project");
            return;
        };
        let Some(device) = self.selected_apple_device().cloned() else {
            log::warn!("mobile_dev: cannot stream logs; no iOS device selected");
            return;
        };
        if device.kind != AppleDeviceKind::Simulator {
            log::warn!("mobile_dev: log streaming is only supported for iOS simulators");
            return;
        }
        if device.state != AppleDeviceState::Booted {
            log::warn!("mobile_dev: cannot stream logs; the selected simulator is not booted");
            return;
        }

        let udid = device.udid.clone();
        let process_hint = project.display_name.clone();

        let forwarder = cx.spawn(async move |this, cx| {
            let stream = apple::log_stream(&udid, Some(&process_hint));
            pin_mut!(stream);
            while let Some(item) = stream.next().await {
                let Ok(_) = this.update(cx, |panel, cx| {
                    panel.push_log_item(item);
                    cx.notify();
                }) else {
                    break;
                };
            }
        });

        self.logcat_state = Some(LogcatUiState {
            platform: MobilePlatform::Ios,
            device_label: device.name.clone(),
            target: SharedString::from(project.display_name),
            pid: None,
            lines: Vec::new(),
            error: None,
            _forwarder: forwarder,
        });
        cx.notify();
    }

    fn push_log_item(&mut self, item: Result<String>) {
        let Some(state) = self.logcat_state.as_mut() else {
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
    }

    /// Spawn the requested build and start streaming output into the panel.
    /// Bails (with a `log::warn!`) if no Expo project has been detected.
    pub fn start_build(&mut self, kind: BuildKind, platform: MobilePlatform, cx: &mut Context<Self>) {
        let Some(project) = self.expo_project.clone() else {
            log::warn!("mobile_dev: cannot start build without a detected Expo project");
            return;
        };
        if platform == MobilePlatform::Ios && !cfg!(target_os = "macos") {
            log::warn!("mobile_dev: iOS builds require macOS");
            return;
        }

        let device_id = match kind {
            BuildKind::LocalDebugRun => match platform {
                MobilePlatform::Android => self
                    .selected_android_device()
                    .filter(|d| d.is_usable())
                    .map(|d| d.serial.clone()),
                MobilePlatform::Ios => self
                    .selected_apple_device()
                    .filter(|d| d.is_usable())
                    .map(|d| d.udid.clone()),
            },
            BuildKind::EasPreview | BuildKind::EasProduction => None,
        };

        let toolchain_env = match platform {
            MobilePlatform::Android => toolchain::build_env(self.toolchain_status.as_ref()),
            MobilePlatform::Ios => Vec::new(),
        };
        let session =
            match BuildSession::spawn(kind, platform, project.root, device_id, toolchain_env) {
                Ok(session) => session,
                Err(err) => {
                    log::error!("mobile_dev: failed to spawn build: {err:#}");
                    self.build_state = Some(BuildUiState {
                        kind,
                        platform,
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
            platform,
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

    fn apply_apple_device_poll(&mut self, result: Result<Vec<AppleDevice>>) {
        match result {
            Ok(devices) => {
                self.apple_device_state.devices = devices;
                self.apple_device_state.error = None;
                self.apple_device_state.loaded = true;
                self.reconcile_selection();
            }
            Err(err) => {
                self.apple_device_state.error = Some(SharedString::from(format!("{err:#}")));
                self.apple_device_state.loaded = true;
            }
        }
    }

    fn selected_platform(&self) -> MobilePlatform {
        self.selected_device
            .as_ref()
            .map(|device| device.platform)
            .unwrap_or(MobilePlatform::Android)
    }

    fn selected_android_device(&self) -> Option<&AdbDevice> {
        let selected = self.selected_device.as_ref()?;
        if selected.platform != MobilePlatform::Android {
            return None;
        }
        self.device_state
            .devices
            .iter()
            .find(|d| d.serial == selected.id)
    }

    fn selected_apple_device(&self) -> Option<&AppleDevice> {
        let selected = self.selected_device.as_ref()?;
        if selected.platform != MobilePlatform::Ios {
            return None;
        }
        self.apple_device_state
            .devices
            .iter()
            .find(|d| d.udid == selected.id)
    }

    fn reconcile_selection(&mut self) {
        if let Some(selected) = self.selected_device.as_ref()
            && !self.device_exists(selected)
        {
            self.selected_device = None;
        }
        if self.selected_device.is_none() {
            self.selected_device = self.default_selection();
        }
    }

    fn device_exists(&self, selected: &SelectedDevice) -> bool {
        match selected.platform {
            MobilePlatform::Android => self
                .device_state
                .devices
                .iter()
                .any(|d| d.serial == selected.id),
            MobilePlatform::Ios => self
                .apple_device_state
                .devices
                .iter()
                .any(|d| d.udid == selected.id),
        }
    }

    /// Prefer targets that are running right now (online Android device,
    /// booted simulator, connected iPhone) over ones that merely exist.
    fn default_selection(&self) -> Option<SelectedDevice> {
        let android = |device: &AdbDevice| SelectedDevice {
            platform: MobilePlatform::Android,
            id: device.serial.clone(),
        };
        let apple = |device: &AppleDevice| SelectedDevice {
            platform: MobilePlatform::Ios,
            id: device.udid.clone(),
        };
        self.device_state
            .devices
            .iter()
            .find(|d| d.is_usable())
            .map(android)
            .or_else(|| {
                self.apple_device_state
                    .devices
                    .iter()
                    .find(|d| {
                        matches!(
                            d.state,
                            AppleDeviceState::Booted | AppleDeviceState::Connected
                        )
                    })
                    .map(apple)
            })
            .or_else(|| self.device_state.devices.first().map(android))
            .or_else(|| self.apple_device_state.devices.first().map(apple))
    }

    fn select_device(&mut self, device: SelectedDevice, cx: &mut Context<Self>) {
        self.selected_device = Some(device);
        cx.notify();
    }

    fn cycle_selected_device(&mut self, cx: &mut Context<Self>) {
        let all: Vec<SelectedDevice> = self
            .device_state
            .devices
            .iter()
            .map(|d| SelectedDevice {
                platform: MobilePlatform::Android,
                id: d.serial.clone(),
            })
            .chain(self.apple_device_state.devices.iter().map(|d| SelectedDevice {
                platform: MobilePlatform::Ios,
                id: d.udid.clone(),
            }))
            .collect();
        if all.is_empty() {
            return;
        }
        let next_index = match self
            .selected_device
            .as_ref()
            .and_then(|selected| all.iter().position(|d| d == selected))
        {
            Some(index) => (index + 1) % all.len(),
            None => 0,
        };
        self.selected_device = all.into_iter().nth(next_index);
        cx.notify();
    }

    fn render_devices_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut section = v_flex().gap_1().w_full().p_3().child(
            Label::new("Devices")
                .size(LabelSize::Small)
                .color(Color::Muted),
        );
        section = section.child(self.render_android_device_group(cx));
        if cfg!(target_os = "macos") {
            section = section.child(self.render_apple_device_group(cx));
        }
        section
    }

    fn render_android_device_group(&self, cx: &mut Context<Self>) -> Div {
        let mut group = v_flex().gap_1().w_full();
        // The platform subheader is only useful next to the iOS group, which
        // exists on macOS hosts only.
        if cfg!(target_os = "macos") {
            group = group.child(
                Label::new("Android")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );
        }

        if !self.device_state.loaded {
            return group.child(
                Label::new("Looking for devices...")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }

        if let Some(err) = self.device_state.error.as_ref() {
            return group.child(
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
        }

        if self.device_state.devices.is_empty() {
            return group.child(
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
        }

        for (index, device) in self.device_state.devices.iter().enumerate() {
            group = group.child(self.render_device_row(index, device, cx));
        }
        group
    }

    fn render_apple_device_group(&self, cx: &mut Context<Self>) -> Div {
        let mut group = v_flex().gap_1().w_full().mt_1().child(
            Label::new("iOS")
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        );

        let xcode_missing = self
            .apple_toolchain_status
            .as_ref()
            .is_some_and(|status| !status.xcode.is_present());
        if xcode_missing {
            return group.child(
                Label::new("Install Xcode to see iOS simulators and devices.")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );
        }

        if !self.apple_device_state.loaded {
            return group.child(
                Label::new("Looking for devices...")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }

        if let Some(err) = self.apple_device_state.error.as_ref() {
            return group.child(
                v_flex()
                    .gap_1()
                    .child(
                        Label::new("simctl error")
                            .size(LabelSize::Small)
                            .color(Color::Error),
                    )
                    .child(
                        Label::new(err.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            );
        }

        if self.apple_device_state.devices.is_empty() {
            return group.child(
                Label::new("No simulators or devices found")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }

        for (index, device) in self.apple_device_state.devices.iter().enumerate() {
            group = group.child(self.render_apple_device_row(index, device, cx));
        }
        group
    }

    fn render_device_row(
        &self,
        index: usize,
        device: &AdbDevice,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selection = SelectedDevice {
            platform: MobilePlatform::Android,
            id: device.serial.clone(),
        };
        let selected = self.selected_device.as_ref() == Some(&selection);
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

        self.device_row(
            ("device-row", index),
            transport_icon,
            label,
            state_label,
            state_color,
            selected,
            selection,
            cx,
        )
    }

    fn render_apple_device_row(
        &self,
        index: usize,
        device: &AppleDevice,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selection = SelectedDevice {
            platform: MobilePlatform::Ios,
            id: device.udid.clone(),
        };
        let selected = self.selected_device.as_ref() == Some(&selection);
        let label = device.label();
        let state_label: SharedString = match device.state {
            AppleDeviceState::Booted => "booted".into(),
            AppleDeviceState::Shutdown => "shutdown".into(),
            AppleDeviceState::Connected => "connected".into(),
            AppleDeviceState::Unavailable => "unavailable".into(),
            AppleDeviceState::Other => "other".into(),
        };
        let state_color = match device.state {
            AppleDeviceState::Booted | AppleDeviceState::Connected => Color::Success,
            AppleDeviceState::Shutdown
            | AppleDeviceState::Unavailable
            | AppleDeviceState::Other => Color::Muted,
        };
        let kind_icon = match device.kind {
            AppleDeviceKind::Simulator => IconName::Screen,
            AppleDeviceKind::Physical => IconName::ArrowDownRight,
        };

        self.device_row(
            ("apple-device-row", index),
            kind_icon,
            label,
            state_label,
            state_color,
            selected,
            selection,
            cx,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn device_row(
        &self,
        id: (&'static str, usize),
        icon: IconName,
        label: SharedString,
        state_label: SharedString,
        state_color: Color,
        selected: bool,
        selection: SelectedDevice,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .id(id)
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
                ui::Icon::new(icon)
                    .size(ui::IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(Label::new(label).size(LabelSize::Small))
            .child(div().flex_1())
            .child(
                Label::new(state_label)
                    .size(LabelSize::XSmall)
                    .color(state_color),
            )
            .on_click(cx.listener(move |panel, _, _, cx| {
                panel.select_device(selection.clone(), cx);
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

        // The pid annotation only means something for logcat; the iOS log
        // stream is filtered by process name instead.
        let pid_label = match (state.platform, state.pid) {
            (MobilePlatform::Android, Some(pid)) => Some(SharedString::from(format!("pid {pid}"))),
            (MobilePlatform::Android, None) => Some(SharedString::from("app not running")),
            (MobilePlatform::Ios, _) => None,
        };
        let title = match state.platform {
            MobilePlatform::Android => "Logcat",
            MobilePlatform::Ios => "Device log",
        };

        let header = h_flex()
            .gap_2()
            .child(
                ui::Icon::new(IconName::Terminal)
                    .size(ui::IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(Label::new(title).size(LabelSize::Small))
            .child(
                Label::new(state.target.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .child(
                Label::new(state.device_label.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .when_some(pid_label, |this, pid_label| {
                this.child(
                    Label::new(pid_label)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            });

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
                        .h(px(240.))
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
            .child(Label::new(state.kind.label(state.platform)).size(LabelSize::Small))
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
                        .h(px(240.))
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
            section = section.child(Self::render_install_log("mobile-toolchain-output", install));
        }

        section
    }

    fn render_install_log(id: &'static str, install: &ToolchainInstallUiState) -> impl IntoElement {
        let mut log = v_flex().w_full();
        for line in install.lines.iter().rev().take(30).rev() {
            log = log.child(
                Label::new(line.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );
        }
        div()
            .id(id)
            .h(px(120.))
            .w_full()
            .overflow_y_scroll()
            .child(log)
    }

    /// Xcode and friends. macOS only; Xcode itself can't be auto-installed,
    /// so the header action is a link when it's missing and a runtime
    /// download when only the simulator runtime is.
    fn render_apple_toolchain_section(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let border_color = cx.theme().colors().border;
        let section = v_flex()
            .w_full()
            .gap_1()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(border_color);

        let header = h_flex()
            .gap_2()
            .child(
                Label::new("Apple toolchain")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .when_some(
                self.apple_toolchain_status
                    .as_ref()
                    .and_then(|status| status.xcode_version.clone()),
                |this, version| {
                    this.child(
                        Label::new(version)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                },
            )
            .child(div().flex_1());

        let Some(status) = self.apple_toolchain_status.as_ref() else {
            return Some(section.child(header).child(
                Label::new("Checking toolchain...")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            ));
        };

        let installing = self
            .apple_install
            .as_ref()
            .is_some_and(|state| state.status == BuildStatus::Running);

        let header = if installing {
            header.child(
                Label::new("downloading...")
                    .size(LabelSize::XSmall)
                    .color(Color::Info),
            )
        } else if status.all_present() {
            header.child(
                Label::new("ready")
                    .size(LabelSize::XSmall)
                    .color(Color::Success),
            )
        } else if !status.xcode.is_present() {
            header.child(
                Button::new("mobile-get-xcode", "Get Xcode")
                    .label_size(LabelSize::Small)
                    .on_click(|_, _, cx| cx.open_url("https://developer.apple.com/xcode/")),
            )
        } else if !status.ios_runtime.is_present() {
            header.child(
                Button::new("mobile-ios-runtime-install", "Download iOS runtime")
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _, _, cx| this.start_ios_runtime_install(cx))),
            )
        } else {
            // Only CocoaPods is missing; there's no safe automated install,
            // so the hint below carries the action.
            header
        };
        let mut section = section.child(header);

        for (name, component) in [
            ("Xcode", &status.xcode),
            ("iOS simulator runtime", &status.ios_runtime),
            ("CocoaPods", &status.cocoapods),
        ] {
            let (state_label, color) = match component {
                toolchain::ComponentStatus::Managed(_) | toolchain::ComponentStatus::System(_) => {
                    ("installed", Color::Success)
                }
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

        if status.xcode.is_present() && !status.cocoapods.is_present() {
            section = section.child(
                Label::new(
                    "Install CocoaPods (e.g. `brew install cocoapods`) to enable local iOS builds.",
                )
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            );
        }

        if let Some(install) = self.apple_install.as_ref() {
            if let BuildStatus::Failure(reason) = &install.status {
                section = section.child(
                    Label::new(reason.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                );
            }
            section = section.child(Self::render_install_log(
                "mobile-apple-toolchain-output",
                install,
            ));
        }

        Some(section)
    }

    /// One-click access to the operations that are otherwise only reachable
    /// through the command palette. Hidden until an Expo project is detected,
    /// mirroring when the actions themselves can do anything.
    fn render_actions_section(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        self.expo_project.as_ref()?;
        let build_label = match self.selected_platform() {
            MobilePlatform::Android => "Build & run (Android)",
            MobilePlatform::Ios => "Build & run (iOS)",
        };

        Some(
            h_flex()
                .w_full()
                .flex_wrap()
                .gap_1()
                .px_3()
                .py_2()
                .border_t_1()
                .border_color(cx.theme().colors().border)
                .child(
                    // Dispatches the action rather than calling into the
                    // panel so the button is the palette entry by
                    // construction. A re-click during a run intentionally
                    // restarts the build (and its Metro server), same as the
                    // palette.
                    Button::new("mobile-action-build-run", build_label)
                        .style(ui::ButtonStyle::Tinted(ui::TintColor::Success))
                        .label_size(LabelSize::Small)
                        .tooltip(|_window, cx| {
                            Tooltip::for_action(
                                "Debug build on the selected device",
                                &BuildAndRun,
                                cx,
                            )
                        })
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(BuildAndRun), cx);
                        }),
                )
                .child(
                    Button::new("mobile-action-eas-android", "EAS preview (Android)")
                        .style(ui::ButtonStyle::Tinted(ui::TintColor::Accent))
                        .label_size(LabelSize::Small)
                        .tooltip(|_window, cx| {
                            Tooltip::for_action("Cloud preview build", &BuildEasPreview, cx)
                        })
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(BuildEasPreview), cx);
                        }),
                )
                .when(cfg!(target_os = "macos"), |this| {
                    this.child(
                        Button::new("mobile-action-eas-ios", "EAS preview (iOS)")
                            .style(ui::ButtonStyle::Tinted(ui::TintColor::Accent))
                            .label_size(LabelSize::Small)
                            .tooltip(|_window, cx| {
                                Tooltip::for_action("Cloud preview build", &BuildEasPreviewIos, cx)
                            })
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(BuildEasPreviewIos), cx);
                            }),
                    )
                })
                .child(
                    // Direct call instead of dispatching OpenLogcat: the
                    // action also toggles panel focus, which is meaningless
                    // (and surprising) when clicked from inside the panel.
                    Button::new("mobile-action-logs", "Logs")
                        .style(ui::ButtonStyle::Filled)
                        .label_size(LabelSize::Small)
                        .tooltip(|_window, cx| {
                            Tooltip::for_action(
                                "Stream the selected device's log",
                                &OpenLogcat,
                                cx,
                            )
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.start_logcat(cx))),
                ),
        )
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
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Label::new("iOS bundle id").size(LabelSize::XSmall).color(Color::Muted),
                        )
                        .child(
                            Label::new(SharedString::from(
                                project
                                    .ios_bundle_identifier
                                    .clone()
                                    .unwrap_or_else(|| "(not set)".to_string()),
                            ))
                            .size(LabelSize::XSmall)
                            .color(if project.ios_bundle_identifier.is_some() {
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
                        Label::new(format!(
                            "{} device(s)",
                            self.device_state.devices.len()
                                + self.apple_device_state.devices.len()
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            )
            .child(
                // The section stack easily outgrows the dock, especially with
                // both toolchains and a simulator roster; everything below
                // the header scrolls as one body.
                v_flex()
                    .id("mobile-panel-body")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scroll()
                    .child(self.render_project_section())
                    .when_some(self.render_actions_section(cx), |this, section| {
                        this.child(section)
                    })
                    .child(self.render_toolchain_section(cx))
                    .when_some(self.render_apple_toolchain_section(cx), |this, section| {
                        this.child(section)
                    })
                    .child(self.render_devices_section(cx))
                    .when_some(self.render_build_section(cx), |this, section| {
                        this.child(section)
                    })
                    .when_some(self.render_logcat_section(cx), |this, section| {
                        this.child(section)
                    }),
            )
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
