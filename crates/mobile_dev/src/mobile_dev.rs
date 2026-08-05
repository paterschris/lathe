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
pub mod commands;
mod device_picker;
pub mod emulator;
pub mod mobile_project;
pub mod toolchain;

pub use device_picker::MobileDeviceSelector;

use std::path::PathBuf;
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
use task::{HideStrategy, RevealStrategy, RevealTarget, SaveStrategy, Shell, SpawnInTerminal, TaskId};
use terminal_view::TerminalView;
use settings::{RegisterSetting, Settings};
use ui::prelude::*;
use ui::{
    Color, ContextMenu, CopyButton, Headline, HeadlineSize, IconName, IconPosition, Label,
    LabelSize, PopoverMenu, Tooltip, h_flex, v_flex,
};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

use crate::adb::AdbDevice;
use crate::apple::{AppleDevice, AppleDeviceKind, AppleDeviceState};
use crate::build::{BuildKind, MobilePlatform};
use crate::commands::ResolvedCommand;
use crate::mobile_project::{MobileProject, ProjectKind};

const PANEL_KEY: &str = "MobileDevPanel";
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Slower than the ADB cadence: `devicectl` takes noticeably longer per
/// invocation than `adb devices`.
const APPLE_DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// AVD listing barely changes, so poll it lazily.
const AVD_POLL_INTERVAL: Duration = Duration::from_secs(15);
const BUILD_OUTPUT_LINE_CAP: usize = 500;
const LOGCAT_LINE_CAP: usize = 1000;
/// Up to this many README run-hints render inline (flowing with the panel
/// scroll); beyond it the section becomes its own fixed-height scroll pane.
const README_INLINE_HINT_LIMIT: usize = 8;

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
        /// Start the Metro bundler for the active project.
        StartMetro,
        /// Stop the running Metro bundler.
        StopMetro,
        /// Boot the selected iOS simulator.
        BootSimulator,
        /// Shut down the selected iOS simulator.
        ShutdownSimulator,
        /// Install iOS CocoaPods for the active project.
        PodInstall,
        /// Run the project's lint script.
        RunLint,
        /// Run the project's test script.
        RunTests,
        /// Run the project's iOS end-to-end (Appium) test script.
        RunE2eIos,
        /// Run the project's Android end-to-end (Appium) test script.
        RunE2eAndroid,
        /// Forward the Metro dev-server port to a USB-attached Android device.
        AdbReverse,
        /// Create a default Android emulator (AVD) within Lathe.
        CreateAvd,
        /// Launch the Spotlight sidecar for live Sentry monitoring.
        StartSpotlight,
        /// Stop the running Spotlight sidecar.
        StopSpotlight,
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
        .register_action(|workspace, _: &BuildAndRun, window, cx| {
            with_panel_in(workspace, window, cx, |panel, window, cx| {
                panel.build_and_run(window, cx)
            });
        })
        .register_action(|workspace, _: &BuildEasPreview, window, cx| {
            with_panel_in(workspace, window, cx, |panel, window, cx| {
                panel.start_build(BuildKind::EasPreview, MobilePlatform::Android, window, cx)
            });
        })
        .register_action(|workspace, _: &BuildEasPreviewIos, window, cx| {
            with_panel_in(workspace, window, cx, |panel, window, cx| {
                panel.start_build(BuildKind::EasPreview, MobilePlatform::Ios, window, cx)
            });
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
        })
        .register_action(|workspace, _: &StartMetro, window, cx| {
            if let Some(panel) = workspace.panel::<MobileDevPanel>(cx) {
                panel.update(cx, |panel, cx| panel.start_metro(false, window, cx));
            }
        })
        .register_action(|workspace, _: &BootSimulator, _, cx| {
            with_panel(workspace, cx, |panel, cx| panel.boot_selected_simulator(cx));
        })
        .register_action(|workspace, _: &ShutdownSimulator, _, cx| {
            with_panel(workspace, cx, |panel, cx| panel.shutdown_selected_simulator(cx));
        })
        .register_action(|workspace, _: &PodInstall, window, cx| {
            with_panel_in(workspace, window, cx, |panel, window, cx| {
                panel.pod_install(window, cx)
            });
        })
        .register_action(|workspace, _: &RunLint, window, cx| {
            with_panel_in(workspace, window, cx, |panel, window, cx| {
                panel.run_named_script("lint", window, cx)
            });
        })
        .register_action(|workspace, _: &RunTests, window, cx| {
            with_panel_in(workspace, window, cx, |panel, window, cx| {
                panel.run_named_script("test", window, cx)
            });
        })
        .register_action(|workspace, _: &RunE2eIos, window, cx| {
            with_panel_in(workspace, window, cx, |panel, window, cx| {
                panel.run_named_script("test:e2e:ios", window, cx)
            });
        })
        .register_action(|workspace, _: &RunE2eAndroid, window, cx| {
            with_panel_in(workspace, window, cx, |panel, window, cx| {
                panel.run_named_script("test:e2e:android", window, cx)
            });
        })
        .register_action(|workspace, _: &AdbReverse, window, cx| {
            with_panel_in(workspace, window, cx, |panel, window, cx| {
                panel.adb_reverse(window, cx)
            });
        })
        .register_action(|workspace, _: &CreateAvd, window, cx| {
            with_panel_in(workspace, window, cx, |panel, window, cx| {
                panel.create_avd(window, cx)
            });
        })
        .register_action(|workspace, _: &StartSpotlight, window, cx| {
            with_panel_in(workspace, window, cx, |panel, window, cx| {
                panel.start_spotlight(window, cx)
            });
        });
}

/// Like [`with_panel`] but threads the window through, for actions that spawn
/// terminals (which need a `Window` to build the [`TerminalView`]).
fn with_panel_in(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    f: impl FnOnce(&mut MobileDevPanel, &mut Window, &mut Context<MobileDevPanel>),
) {
    if let Some(panel) = workspace.panel::<MobileDevPanel>(cx) {
        panel.update(cx, |panel, cx| f(panel, window, cx));
    }
}

/// Run `f` against the workspace's mobile panel if it exists. Keeps the action
/// handlers above to a single line each.
fn with_panel(
    workspace: &mut Workspace,
    cx: &mut Context<Workspace>,
    f: impl FnOnce(&mut MobileDevPanel, &mut Context<MobileDevPanel>),
) {
    if let Some(panel) = workspace.panel::<MobileDevPanel>(cx) {
        panel.update(cx, |panel, cx| f(panel, cx));
    }
}

/// POSIX single-quote escaping for embedding a path/arg in a shell command.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Join `commands` into one `&&`-chained shell command, each step `cd`-ing into
/// its own working directory first. When any step needs the managed Android
/// toolchain, its env is exported up front (prepended to `$PATH` so the user's
/// shell PATH still wins for everything else).
fn build_compound_command(
    commands: &[ResolvedCommand],
    toolchain: Option<&toolchain::ToolchainStatus>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if commands.iter().any(|command| command.wants_android_env) {
        let exports = android_export_prefix(toolchain);
        if !exports.is_empty() {
            parts.push(exports);
        }
    }
    for command in commands {
        let mut step = format!("cd {} && {}", shell_quote(&command.cwd.to_string_lossy()), shell_quote(&command.program));
        for arg in &command.args {
            step.push(' ');
            step.push_str(&shell_quote(arg));
        }
        parts.push(step);
    }
    parts.join(" && ")
}

/// `export`s for the Lathe-managed Android toolchain, or an empty string when
/// nothing is managed (the user's own env is assumed set up).
fn android_export_prefix(toolchain: Option<&toolchain::ToolchainStatus>) -> String {
    let Some(status) = toolchain else {
        return String::new();
    };
    let mut exports: Vec<String> = Vec::new();
    if let toolchain::ComponentStatus::Managed(home) = &status.jdk {
        exports.push(format!("export JAVA_HOME={}", shell_quote(&home.to_string_lossy())));
    }
    if let toolchain::ComponentStatus::Managed(sdk) = &status.sdk {
        let sdk = sdk.to_string_lossy();
        exports.push(format!("export ANDROID_HOME={}", shell_quote(&sdk)));
        exports.push(format!("export ANDROID_SDK_ROOT={}", shell_quote(&sdk)));
        // Double-quoted so $PATH expands; managed SDK paths never contain
        // spaces (see toolchain::managed_root).
        exports.push(format!(
            "export PATH=\"{sdk}/platform-tools:{sdk}/cmdline-tools/latest/bin:$PATH\""
        ));
    }
    exports.join(" && ")
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









/// One interactive terminal tab: a PTY-backed process (Metro, a build, a
/// script) rendered as a real terminal with input, scrollback, and colors.
/// Dropping the [`TerminalView`] tears down the terminal and kills its child.
struct TerminalTab {
    id: usize,
    title: SharedString,
    view: Entity<TerminalView>,
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
    /// Detected React Native / Expo project metadata for the active workspace.
    /// `None` while detection is in flight or when the project is not a mobile
    /// project.
    mobile_project: Option<MobileProject>,
    /// `true` once the first detection pass has completed (so we can show
    /// the "not a mobile project" empty state instead of a perpetual
    /// loading spinner).
    project_scanned: bool,
    /// Installed Android emulator (AVD) names, from `emulator -list-avds`.
    avds: Vec<SharedString>,
    /// The iOS scheme chosen for the native Build & run, if the project has
    /// shared schemes. Defaults to the first detected scheme.
    selected_scheme: Option<SharedString>,
    /// The Android gradle variant chosen for the native Build & run.
    selected_variant: Option<SharedString>,
    /// Interactive terminal tabs, one per started process.
    terminals: Vec<TerminalTab>,
    active_terminal: usize,
    next_terminal_id: usize,
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
    _avd_tracker: Task<()>,
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
                            if let Some(project) = mobile_project::detect_at(&root) {
                                return Some(project);
                            }
                        }
                        None
                    })
                    .await;

                this.update(cx, |panel, cx| {
                    panel.selected_scheme = detected
                        .as_ref()
                        .and_then(|project| project.ios_schemes.first().cloned())
                        .map(SharedString::from);
                    panel.selected_variant = detected
                        .as_ref()
                        .and_then(|project| project.android_variants.first().cloned())
                        .map(SharedString::from);
                    panel.mobile_project = detected;
                    panel.project_scanned = true;
                    panel.maybe_offer_toolchain_install(cx);
                    cx.notify();
                })
                .ok();
            }
        });

        let avd_tracker = cx.spawn(async move |this, cx| {
            loop {
                let env = this
                    .read_with(cx, |panel, _| {
                        toolchain::build_env(panel.toolchain_status.as_ref())
                    })
                    .unwrap_or_default();
                let avds = cx
                    .background_spawn(async move { emulator::list_avds(&env).await })
                    .await;
                let Ok(_) = this.update(cx, |panel, cx| {
                    if let Ok(avds) = avds {
                        panel.avds = avds.into_iter().map(SharedString::from).collect();
                        cx.notify();
                    }
                }) else {
                    break;
                };
                cx.background_executor().timer(AVD_POLL_INTERVAL).await;
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
            mobile_project: None,
            project_scanned: false,
            avds: Vec::new(),
            selected_scheme: None,
            selected_variant: None,
            terminals: Vec::new(),
            active_terminal: 0,
            next_terminal_id: 0,
            logcat_state: None,
            toolchain_status: None,
            toolchain_install: None,
            apple_toolchain_status: None,
            apple_install: None,
            toolchain_offer_made: false,
            _device_tracker: device_tracker,
            _apple_device_tracker: apple_device_tracker,
            _avd_tracker: avd_tracker,
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
            || self.mobile_project.is_none()
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
                            "This is a mobile project, but some of the Android build tools it \
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
        let Some(project) = self.mobile_project.clone() else {
            log::warn!("mobile_dev: cannot start logcat without a detected mobile project");
            return;
        };
        let Some(package) = project.android_package else {
            log::warn!("mobile_dev: cannot start logcat; no Android application id detected");
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
        let Some(project) = self.mobile_project.clone() else {
            log::warn!("mobile_dev: cannot stream logs without a detected mobile project");
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
            device_label: device.name,
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

    /// Route the primary "Build & run" affordance to the right command for the
    /// project kind, run in an interactive terminal tab.
    pub fn build_and_run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.mobile_project.clone() else {
            log::warn!("mobile_dev: cannot build without a detected mobile project");
            return;
        };
        let platform = self.selected_platform();
        let run_commands: Vec<ResolvedCommand> = match platform {
            MobilePlatform::Ios => {
                let udid = self.selected_apple_device().map(|device| device.udid.clone());
                let scheme = match project.kind {
                    ProjectKind::BareReactNative => self.selected_scheme.as_deref(),
                    ProjectKind::Expo => None,
                };
                vec![commands::run_ios(&project, scheme, udid.as_deref())]
            }
            MobilePlatform::Android => {
                let device = self.selected_android_device();
                let serial = device.map(|device| device.serial.clone());
                let expo_name = device.map(|device| device.expo_device_name());
                self.android_run_commands(&project, serial.as_deref(), expo_name.as_deref())
            }
        };
        let steps = self.with_prereqs(&project, run_commands, platform == MobilePlatform::Ios);
        self.run_in_terminal(format!("Run {}", platform.label()), steps, window, cx);
    }

    /// Android run commands. For bare React Native with a known variant and
    /// applicationId, use gradle install + adb launch (robust against the RN
    /// CLI's flavored-APK bug); otherwise fall back to `react-native
    /// run-android` / `expo run:android`.
    fn android_run_commands(
        &self,
        project: &MobileProject,
        serial: Option<&str>,
        expo_device_name: Option<&str>,
    ) -> Vec<ResolvedCommand> {
        if project.kind == ProjectKind::BareReactNative {
            let variant = self
                .selected_variant
                .clone()
                .or_else(|| project.android_variants.first().cloned().map(SharedString::from));
            if let Some(variant) = variant
                && let Some(application_id) = project.variant_application_id(&variant)
            {
                return commands::run_android_gradle(project, &variant, serial, &application_id);
            }
        }
        vec![commands::run_android(
            project,
            None,
            serial,
            expo_device_name,
        )]
    }

    /// Start an EAS cloud build in a terminal tab.
    pub fn start_build(
        &mut self,
        kind: BuildKind,
        platform: MobilePlatform,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.mobile_project.clone() else {
            log::warn!("mobile_dev: cannot start build without a detected mobile project");
            return;
        };
        let (program, args) = crate::build::build_command(kind, platform, None);
        let command = ResolvedCommand {
            label: kind.label(platform),
            program,
            args,
            cwd: project.root,
            wants_android_env: false,
        };
        self.run_in_terminal(kind.label(platform), vec![command], window, cx);
    }

    /// Prepend prerequisite installs (JS deps, and CocoaPods setup for an iOS
    /// run) to `run_commands` so a terminal never fails just because setup
    /// hasn't been done.
    fn with_prereqs(
        &self,
        project: &MobileProject,
        run_commands: Vec<ResolvedCommand>,
        needs_pods: bool,
    ) -> Vec<ResolvedCommand> {
        let mut steps = Vec::new();
        if !project.root.join("node_modules").is_dir() {
            steps.push(commands::install_deps(project));
        }
        if needs_pods && project.has_podfile && !project.root.join("ios/Pods").is_dir() {
            steps.extend(Self::ios_pod_commands(project));
        }
        steps.extend(run_commands);
        steps
    }

    /// Project-specific iOS pod setup: Bundler under the project's pinned Ruby
    /// (provisioned via rbenv) when it has a Gemfile, else a direct pod install
    /// (installing CocoaPods via Homebrew first when missing).
    fn ios_pod_commands(project: &MobileProject) -> Vec<ResolvedCommand> {
        let mut steps = Vec::new();
        if project.has_gemfile {
            if let Some(version) = project.ruby_version.as_deref() {
                if let Some(rbenv) = commands::install_rbenv(project) {
                    steps.push(rbenv);
                }
                if !commands::ruby_installed(version) {
                    steps.push(commands::install_ruby(project, version));
                }
            }
            steps.push(commands::bundle_install(project));
            steps.push(commands::pod_install(project));
        } else {
            if let Some(cocoapods) = commands::install_cocoapods(project) {
                steps.push(cocoapods);
            }
            steps.push(commands::pod_install(project));
        }
        steps
    }

    fn run_named_script(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.mobile_project.clone() else {
            return;
        };
        if !project.scripts.iter().any(|script| script.name == name) {
            log::warn!("mobile_dev: project has no `{name}` script");
            return;
        }
        self.run_script(&project, name, window, cx);
    }

    fn run_project_script(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.mobile_project.clone() else {
            return;
        };
        self.run_script(&project, name, window, cx);
    }

    /// Run a package.json script, appending the selected scheme (iOS-named
    /// scripts) or gradle variant (Android-named scripts) as positional
    /// `variant config` arguments when the project defines them.
    fn run_script(
        &mut self,
        project: &MobileProject,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let extra = self.script_extra_args(name);
        let command = commands::run_script(project, name, &extra);
        let title = command.label.clone();
        let steps = self.with_prereqs(project, vec![command], false);
        self.run_in_terminal(title, steps, window, cx);
    }

    /// The scheme/variant arguments to append to a script, based on its name:
    /// an `ios`-named script gets the selected iOS scheme, an `android`-named
    /// one gets the selected gradle variant, both split into `variant config`.
    fn script_extra_args(&self, script_name: &str) -> Vec<String> {
        let lower = script_name.to_lowercase();
        if lower.contains("ios") {
            if let Some(scheme) = self.selected_scheme.as_deref() {
                return mobile_project::split_variant_config(scheme);
            }
        } else if lower.contains("android")
            && let Some(variant) = self.selected_variant.as_deref()
        {
            return mobile_project::split_variant_config(variant);
        }
        Vec::new()
    }

    fn pod_install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.mobile_project.clone() else {
            return;
        };
        let mut steps = Vec::new();
        if !project.root.join("node_modules").is_dir() {
            steps.push(commands::install_deps(&project));
        }
        steps.extend(Self::ios_pod_commands(&project));
        self.run_in_terminal("Install pods", steps, window, cx);
    }

    fn adb_reverse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.mobile_project.clone() else {
            return;
        };
        let serial = self
            .selected_android_device()
            .map(|device| device.serial.to_string());
        self.run_in_terminal(
            "adb reverse",
            vec![commands::adb_reverse(&project, serial.as_deref())],
            window,
            cx,
        );
    }

    /// Start the Metro dev server. When `localhost` (Expo only), serves the
    /// dev-server URL on localhost so an Android emulator can reach it.
    pub fn start_metro(&mut self, localhost: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.mobile_project.clone() else {
            return;
        };
        let mut commands = Vec::new();
        if !project.root.join("node_modules").is_dir() {
            commands.push(commands::install_deps(&project));
        }
        commands.push(commands::metro(&project, localhost));
        let title = if localhost { "Metro (localhost)" } else { "Metro" };
        self.run_in_terminal(title, commands, window, cx);
    }

    /// Run `commands` (chained with `&&`) in a new interactive terminal tab.
    /// The compound is executed by the user's login+interactive shell so it
    /// inherits their real PATH (nvm, rbenv, Homebrew); managed Android tools
    /// are exported inline when a step needs them.
    fn run_in_terminal(
        &mut self,
        title: impl Into<SharedString>,
        commands: Vec<ResolvedCommand>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(first) = commands.first() else {
            return;
        };
        let cwd = first.cwd.clone();
        let compound = build_compound_command(&commands, self.toolchain_status.as_ref());
        self.spawn_terminal(title, compound, cwd, window, cx);
    }

    /// Run a raw shell command string in a terminal tab. When `android_env`,
    /// the managed Android toolchain is exported first (so `sdkmanager` /
    /// `avdmanager` find Java and the SDK).
    fn run_shell_in_terminal(
        &mut self,
        title: impl Into<SharedString>,
        shell_command: String,
        cwd: PathBuf,
        android_env: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let compound = if android_env {
            let exports = android_export_prefix(self.toolchain_status.as_ref());
            if exports.is_empty() {
                shell_command
            } else {
                format!("{exports} && {shell_command}")
            }
        } else {
            shell_command
        };
        self.spawn_terminal(title, compound, cwd, window, cx);
    }

    /// Open a new interactive terminal tab running `compound` in the user's
    /// login+interactive shell.
    fn spawn_terminal(
        &mut self,
        title: impl Into<SharedString>,
        compound: String,
        cwd: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = title.into();
        let login_shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        let id = self.next_terminal_id;
        self.next_terminal_id += 1;

        let spawn = SpawnInTerminal {
            id: TaskId(format!("mobile-dev-{id}")),
            full_label: title.to_string(),
            label: title.to_string(),
            command: Some(login_shell),
            args: vec!["-lic".to_string(), compound],
            command_label: title.to_string(),
            cwd: Some(cwd),
            env: Default::default(),
            use_new_terminal: true,
            allow_concurrent_runs: true,
            reveal: RevealStrategy::NoFocus,
            reveal_target: RevealTarget::Dock,
            hide: HideStrategy::Never,
            shell: Shell::System,
            show_summary: false,
            show_command: false,
            show_rerun: false,
            save: SaveStrategy::default(),
        };

        let project = self.project.clone();
        let workspace = self.workspace.clone();
        let terminal_task =
            project.update(cx, |project, cx| project.create_terminal_task(spawn, cx));
        cx.spawn_in(window, async move |this, cx| {
            let terminal = terminal_task.await?;
            let view = cx.new_window_entity(|window, cx| {
                TerminalView::new(terminal, workspace, None, project.downgrade(), window, cx)
            })?;
            this.update_in(cx, |this, _window, cx| {
                this.terminals.push(TerminalTab { id, title, view });
                this.active_terminal = this.terminals.len() - 1;
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn close_terminal(&mut self, id: usize, cx: &mut Context<Self>) {
        if let Some(index) = self.terminals.iter().position(|tab| tab.id == id) {
            self.terminals.remove(index);
            if self.active_terminal >= self.terminals.len() {
                self.active_terminal = self.terminals.len().saturating_sub(1);
            }
            cx.notify();
        }
    }

    pub fn start_spotlight(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.mobile_project.clone() else {
            return;
        };
        self.run_in_terminal("Spotlight", vec![commands::spotlight(&project)], window, cx);
    }

    fn boot_selected_simulator(&mut self, cx: &mut Context<Self>) {
        let Some(device) = self.selected_apple_device().cloned() else {
            log::warn!("mobile_dev: no iOS simulator selected to boot");
            return;
        };
        if device.kind != AppleDeviceKind::Simulator {
            log::warn!("mobile_dev: the selected iOS device is not a simulator");
            return;
        }
        let udid = device.udid.to_string();
        cx.background_spawn(async move { apple::boot_simulator(&udid).await })
            .detach_and_log_err(cx);
    }

    fn shutdown_selected_simulator(&mut self, cx: &mut Context<Self>) {
        let Some(device) = self.selected_apple_device().cloned() else {
            return;
        };
        if device.kind != AppleDeviceKind::Simulator {
            return;
        }
        let udid = device.udid.to_string();
        cx.background_spawn(async move { apple::shutdown_simulator(&udid).await })
            .detach_and_log_err(cx);
    }

    fn start_emulator(&mut self, name: SharedString, cx: &mut Context<Self>) {
        let env = toolchain::build_env(self.toolchain_status.as_ref());
        let name = name.to_string();
        cx.background_spawn(async move { emulator::launch_avd(&name, &env) })
            .detach_and_log_err(cx);
    }

    /// Create a default Pixel AVD entirely within Lathe (no Android Studio):
    /// installs the emulator package and a system image via the SDK's
    /// `sdkmanager`, then creates the AVD with `avdmanager`, all in a terminal
    /// tab so the (large) download is visible. The new AVD appears in the
    /// Android dropdown once the tracker next polls.
    fn create_avd(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(sdkmanager), Some(avdmanager), Some(sdk)) = (
            emulator::sdkmanager_path(),
            emulator::avdmanager_path(),
            toolchain::sdk_dir(),
        ) else {
            log::warn!(
                "mobile_dev: Android SDK command-line tools not found; install the Android \
                 toolchain first"
            );
            return;
        };
        let image = emulator::default_system_image();
        let name = "Lathe_Pixel_API35";
        let sdkmanager = shell_quote(&sdkmanager.to_string_lossy());
        let avdmanager = shell_quote(&avdmanager.to_string_lossy());
        // Accept licenses, install the emulator + system image, then create the
        // AVD, answering avdmanager's hardware-profile prompt with the default.
        let command = format!(
            "yes | {sdkmanager} --licenses >/dev/null 2>&1; \
             {sdkmanager} 'emulator' '{image}' && \
             echo no | {avdmanager} create avd -n '{name}' -k '{image}' -d pixel_7 --force"
        );
        self.run_shell_in_terminal("Create AVD", command, sdk, true, window, cx);
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
        let mut row = h_flex()
            .w_full()
            .gap_4()
            .items_start()
            .child(self.render_android_dropdown(cx));
        if cfg!(target_os = "macos") {
            row = row.child(self.render_apple_dropdown(cx));
        }
        v_flex()
            .w_full()
            .gap_1()
            .px_3()
            .py_2()
            .child(
                Label::new("Devices")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(row)
    }

    /// Android device + emulator picker as a compact dropdown.
    fn render_android_dropdown(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let devices = self.device_state.devices.clone();
        let avds = self.avds.clone();
        let selected = self.selected_device.clone();
        let trigger_label = selected
            .as_ref()
            .filter(|selection| selection.platform == MobilePlatform::Android)
            .and_then(|selection| {
                devices
                    .iter()
                    .find(|device| device.serial == selection.id)
                    .map(|device| device.label())
            })
            .unwrap_or_else(|| SharedString::from("Select device"));
        let panel = cx.entity();
        let menu = PopoverMenu::new("mobile-android-devices")
            .trigger(
                Button::new("mobile-android-devices-trigger", trigger_label)
                    .style(ui::ButtonStyle::Filled)
                    .label_size(LabelSize::Small)
                    .end_icon(ui::Icon::new(IconName::ChevronDown).size(ui::IconSize::XSmall).color(Color::Muted)),
            )
            .menu(move |window, cx| {
                let panel = panel.clone();
                let devices = devices.clone();
                let avds = avds.clone();
                let selected = selected.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
                    if devices.is_empty() {
                        menu = menu.label("No devices connected");
                    }
                    for device in &devices {
                        let selection = SelectedDevice {
                            platform: MobilePlatform::Android,
                            id: device.serial.clone(),
                        };
                        let toggled = selected.as_ref() == Some(&selection);
                        let panel = panel.clone();
                        menu = menu.toggleable_entry(
                            device.label(),
                            toggled,
                            IconPosition::Start,
                            None,
                            move |_window, cx| {
                                panel.update(cx, |panel, cx| {
                                    panel.select_device(selection.clone(), cx)
                                });
                            },
                        );
                    }
                    if !avds.is_empty() {
                        menu = menu.separator().header("Emulators");
                        for avd in &avds {
                            let name = avd.clone();
                            let panel = panel.clone();
                            menu = menu.entry(format!("Start {avd}"), None, move |_window, cx| {
                                panel.update(cx, |panel, cx| panel.start_emulator(name.clone(), cx));
                            });
                        }
                    }
                    menu = menu.separator().entry(
                        "Create AVD (Pixel, API 35)",
                        None,
                        move |window, cx| {
                            panel.update(cx, |panel, cx| panel.create_avd(window, cx));
                        },
                    );
                    menu
                }))
            });
        let mut column = v_flex().flex_1().gap_1().child(
            Label::new("Android")
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        );
        if let Some(project) = self.mobile_project.as_ref()
            && project.kind == ProjectKind::BareReactNative
            && !project.android_variants.is_empty()
        {
            let options: Vec<SharedString> = project
                .android_variants
                .iter()
                .cloned()
                .map(SharedString::from)
                .collect();
            let selected_variant = self
                .selected_variant
                .clone()
                .or_else(|| options.first().cloned());
            column = column.child(self.render_selection_dropdown(
                "Variant",
                "mobile-variant",
                options,
                selected_variant,
                false,
                cx,
            ));
        }
        column.child(menu)
    }

    /// iOS simulator + device picker as a compact dropdown.
    fn render_apple_dropdown(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let devices = self.apple_device_state.devices.clone();
        let selected = self.selected_device.clone();
        let trigger_label = selected
            .as_ref()
            .filter(|selection| selection.platform == MobilePlatform::Ios)
            .and_then(|selection| {
                devices
                    .iter()
                    .find(|device| device.udid == selection.id)
                    .map(|device| device.label())
            })
            .unwrap_or_else(|| SharedString::from("Select device"));
        let panel = cx.entity();
        let menu = PopoverMenu::new("mobile-apple-devices")
            .trigger(
                Button::new("mobile-apple-devices-trigger", trigger_label)
                    .style(ui::ButtonStyle::Filled)
                    .label_size(LabelSize::Small)
                    .end_icon(ui::Icon::new(IconName::ChevronDown).size(ui::IconSize::XSmall).color(Color::Muted)),
            )
            .menu(move |window, cx| {
                let panel = panel.clone();
                let devices = devices.clone();
                let selected = selected.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
                    if devices.is_empty() {
                        menu = menu.label("No simulators or devices");
                    }
                    for device in &devices {
                        let selection = SelectedDevice {
                            platform: MobilePlatform::Ios,
                            id: device.udid.clone(),
                        };
                        let toggled = selected.as_ref() == Some(&selection);
                        let panel = panel.clone();
                        menu = menu.toggleable_entry(
                            device.label(),
                            toggled,
                            IconPosition::Start,
                            None,
                            move |_window, cx| {
                                panel.update(cx, |panel, cx| {
                                    panel.select_device(selection.clone(), cx)
                                });
                            },
                        );
                    }
                    menu
                }))
            });
        let mut column = v_flex()
            .flex_1()
            .gap_1()
            .child(Label::new("iOS").size(LabelSize::XSmall).color(Color::Muted));
        if let Some(project) = self.mobile_project.as_ref()
            && project.kind == ProjectKind::BareReactNative
            && !project.ios_schemes.is_empty()
        {
            let options: Vec<SharedString> = project
                .ios_schemes
                .iter()
                .cloned()
                .map(SharedString::from)
                .collect();
            let selected_scheme = self
                .selected_scheme
                .clone()
                .or_else(|| options.first().cloned());
            column = column.child(self.render_selection_dropdown(
                "Scheme",
                "mobile-scheme",
                options,
                selected_scheme,
                true,
                cx,
            ));
        }
        column.child(menu)
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
        let copy_text = state.lines.join("\n");

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
            })
            .child(CopyButton::new("mobile-logcat-copy", copy_text).tooltip_label("Copy log"));

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
                        .occlude()
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
            .occlude()
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
    /// through the command palette. Hidden until a mobile project is detected,
    /// and each button is shown only when the project supports it.
    fn render_actions_section(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let project = self.mobile_project.clone()?;
        let build_label = match self.selected_platform() {
            MobilePlatform::Android => "Build & run (Android)",
            MobilePlatform::Ios => "Build & run (iOS)",
        };
        // Whether the selected device is a simulator, and if so whether it is
        // already booted (drives the boot/shutdown button).
        let simulator_booted = self
            .selected_apple_device()
            .filter(|device| device.kind == AppleDeviceKind::Simulator)
            .map(|device| device.state == AppleDeviceState::Booted);
        let has_android_device = self.selected_android_device().is_some();

        let mut row = h_flex()
            .w_full()
            .flex_wrap()
            .gap_1()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(
                Button::new("mobile-action-build-run", build_label)
                    .style(ui::ButtonStyle::Tinted(ui::TintColor::Success))
                    .label_size(LabelSize::Small)
                    .tooltip(|_window, cx| {
                        Tooltip::for_action("Debug build on the selected device", &BuildAndRun, cx)
                    })
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(BuildAndRun), cx);
                    }),
            )
            .child(
                Button::new("mobile-action-metro", "Start Metro")
                    .style(ui::ButtonStyle::Filled)
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::text("Start the Metro dev server in a terminal"))
                    .on_click(cx.listener(|this, _, window, cx| this.start_metro(false, window, cx))),
            )
            .child(
                Button::new("mobile-action-logs", "Logs")
                    .style(ui::ButtonStyle::Filled)
                    .label_size(LabelSize::Small)
                    .tooltip(|_window, cx| {
                        Tooltip::for_action("Stream the selected device's log", &OpenLogcat, cx)
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.start_logcat(cx))),
            );

        // Expo only: the LAN dev-server URL an emulator can't reach is the
        // usual Android-emulator failure, so offer a localhost variant.
        if project.kind == ProjectKind::Expo {
            row = row.child(
                Button::new("mobile-action-metro-localhost", "Metro (localhost)")
                    .style(ui::ButtonStyle::Filled)
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::text(
                        "Start Metro on localhost so an Android emulator can reach it (via adb reverse)",
                    ))
                    .on_click(cx.listener(|this, _, window, cx| this.start_metro(true, window, cx))),
            );
        }

        if let Some(booted) = simulator_booted {
            row = row.child(
                Button::new(
                    "mobile-action-simulator",
                    if booted { "Shut down simulator" } else { "Boot simulator" },
                )
                .style(ui::ButtonStyle::Filled)
                .label_size(LabelSize::Small)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if booted {
                        this.shutdown_selected_simulator(cx);
                    } else {
                        this.boot_selected_simulator(cx);
                    }
                })),
            );
        }

        if project.has_podfile {
            row = row.child(
                Button::new("mobile-action-pods", "Install pods")
                    .style(ui::ButtonStyle::Filled)
                    .label_size(LabelSize::Small)
                    .tooltip(|_window, cx| Tooltip::for_action("Run pod install", &PodInstall, cx))
                    .on_click(cx.listener(|this, _, window, cx| this.pod_install(window, cx))),
            );
        }

        if has_android_device {
            row = row.child(
                Button::new("mobile-action-adb-reverse", "adb reverse")
                    .style(ui::ButtonStyle::Filled)
                    .label_size(LabelSize::Small)
                    .tooltip(|_window, cx| {
                        Tooltip::for_action("Forward Metro to the device", &AdbReverse, cx)
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.adb_reverse(window, cx))),
            );
        }

        row = row.child(
            Button::new("mobile-action-spotlight", "Spotlight")
                .style(ui::ButtonStyle::Filled)
                .label_size(LabelSize::Small)
                .on_click(cx.listener(|this, _, window, cx| this.start_spotlight(window, cx))),
        );

        if project.uses_eas {
            row = row.child(
                Button::new("mobile-action-eas-android", "EAS preview (Android)")
                    .style(ui::ButtonStyle::Tinted(ui::TintColor::Accent))
                    .label_size(LabelSize::Small)
                    .tooltip(|_window, cx| {
                        Tooltip::for_action("Cloud preview build", &BuildEasPreview, cx)
                    })
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(BuildEasPreview), cx);
                    }),
            );
            if cfg!(target_os = "macos") {
                row = row.child(
                    Button::new("mobile-action-eas-ios", "EAS preview (iOS)")
                        .style(ui::ButtonStyle::Tinted(ui::TintColor::Accent))
                        .label_size(LabelSize::Small)
                        .tooltip(|_window, cx| {
                            Tooltip::for_action("Cloud preview build", &BuildEasPreviewIos, cx)
                        })
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(BuildEasPreviewIos), cx);
                        }),
                );
            }
        }

        Some(row)
    }

    fn render_project_section(&self) -> impl IntoElement {
        let v = v_flex().w_full().gap_1().px_3().py_2();
        if !self.project_scanned {
            return v.child(
                Label::new("Scanning workspace for a mobile project...")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }
        match &self.mobile_project {
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
                        )
                        .child(div().flex_1())
                        .child(
                            Label::new(project.kind.label())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(project.package_manager.label())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Label::new("Android package")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
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
                            Label::new("iOS bundle id")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
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
                    Label::new("No mobile project detected")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(
                        "Open a React Native or Expo project (a package.json with a react-native \
                         or expo dependency, or ios/ and android/ folders) to enable build and run \
                         actions.",
                    )
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                ),
        }
    }

    /// A labeled dropdown of `options`; picking one sets the iOS scheme (when
    /// `is_scheme`) or the Android variant.
    fn render_selection_dropdown(
        &self,
        label: &'static str,
        id: &'static str,
        options: Vec<SharedString>,
        selected: Option<SharedString>,
        is_scheme: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let trigger_label = selected.clone().unwrap_or_else(|| SharedString::from("Select"));
        let panel = cx.entity();
        let menu = PopoverMenu::new(id)
            .trigger(
                Button::new(SharedString::from(format!("{id}-trigger")), trigger_label)
                    .style(ui::ButtonStyle::Filled)
                    .label_size(LabelSize::Small)
                    .end_icon(ui::Icon::new(IconName::ChevronDown).size(ui::IconSize::XSmall).color(Color::Muted)),
            )
            .menu(move |window, cx| {
                let panel = panel.clone();
                let options = options.clone();
                let selected = selected.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
                    for option in &options {
                        let toggled = selected.as_ref() == Some(option);
                        let value = option.clone();
                        let panel = panel.clone();
                        menu = menu.toggleable_entry(
                            option.clone(),
                            toggled,
                            IconPosition::Start,
                            None,
                            move |_window, cx| {
                                let value = value.clone();
                                panel.update(cx, |panel, cx| {
                                    if is_scheme {
                                        panel.selected_scheme = Some(value);
                                    } else {
                                        panel.selected_variant = Some(value);
                                    }
                                    cx.notify();
                                });
                            },
                        );
                    }
                    menu
                }))
            });
        h_flex()
            .w_full()
            .gap_2()
            .child(
                Label::new(label)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .child(menu)
    }

    /// A button per `package.json` script, so every command the project
    /// defines is one click away. Scripts that need positional arguments print
    /// their own usage into the output pane, which is itself useful feedback.
    fn render_scripts_section(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let project = self.mobile_project.as_ref()?;
        if project.scripts.is_empty() {
            return None;
        }
        let mut row = h_flex().w_full().flex_wrap().gap_1();
        for (index, script) in project.scripts.iter().enumerate() {
            let name = script.name.clone();
            let command = script.command.clone();
            row = row.child(
                Button::new(("mobile-script", index), script.name.clone())
                    .style(ui::ButtonStyle::Filled)
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::text(command))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.run_project_script(&name, window, cx);
                    })),
            );
        }
        Some(
            v_flex()
                .w_full()
                .gap_1()
                .px_3()
                .py_2()
                .border_t_1()
                .border_color(cx.theme().colors().border)
                .child(
                    Label::new("Scripts")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(row),
        )
    }

    /// Interactive terminal tabs (Metro, builds, scripts): a tab strip plus
    /// the active terminal, fully interactive with its own scrollback.
    fn render_terminals_section(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.terminals.is_empty() {
            return None;
        }
        let active = self.active_terminal.min(self.terminals.len() - 1);
        let border = cx.theme().colors().border;

        let mut tabs = h_flex().w_full().gap_1().flex_wrap();
        for (index, tab) in self.terminals.iter().enumerate() {
            let is_active = index == active;
            let id = tab.id;
            tabs = tabs.child(
                h_flex()
                    .id(("mobile-term-tab", tab.id))
                    .gap_1()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .when(is_active, |this| {
                        this.bg(cx.theme().colors().element_selected)
                    })
                    .when(!is_active, |this| {
                        this.hover(|style| style.bg(cx.theme().colors().element_hover))
                    })
                    .child(Label::new(tab.title.clone()).size(LabelSize::Small))
                    .child(
                        ui::IconButton::new(("mobile-term-close", tab.id), IconName::Close)
                            .icon_size(ui::IconSize::XSmall)
                            .on_click(cx.listener(move |this, _, _, cx| this.close_terminal(id, cx))),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.active_terminal = index;
                        cx.notify();
                    })),
            );
        }

        let view = self.terminals[active].view.clone();
        Some(
            v_flex()
                .w_full()
                .border_t_1()
                .border_color(border)
                .child(h_flex().w_full().px_3().py_1().child(tabs))
                .child(div().w_full().h(px(320.)).occlude().child(view)),
        )
    }



    /// Run/start commands scraped from the project's README, so the panel
    /// suggests how to run this specific app. Hidden when the README yielded
    /// no recognizable commands.
    fn render_readme_section(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let project = self.mobile_project.as_ref()?;
        if project.readme_run_hints.is_empty() {
            return None;
        }
        let mut rows = v_flex().w_full().gap_1();
        for (index, hint) in project.readme_run_hints.iter().enumerate() {
            rows = rows.child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        ui::Icon::new(IconName::Terminal)
                            .size(ui::IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(Label::new(hint.clone()).size(LabelSize::XSmall))
                    .child(div().flex_1())
                    .child(
                        CopyButton::new(("mobile-readme-hint", index), hint.clone())
                            .tooltip_label("Copy command"),
                    ),
            );
        }
        Some(
            v_flex()
                .w_full()
                .gap_1()
                .px_3()
                .py_2()
                .border_t_1()
                .border_color(cx.theme().colors().border)
                .child(
                    Label::new("How to run (from README)")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(if project.readme_run_hints.len() > README_INLINE_HINT_LIMIT {
                    // Long list: its own scrollable, occluded pane.
                    div()
                        .id("mobile-readme-scroll")
                        .max_h(px(160.))
                        .w_full()
                        .overflow_y_scroll()
                        .occlude()
                        .child(rows)
                        .into_any_element()
                } else {
                    // Short list: flow with the panel so hovering it doesn't
                    // lock the panel's scroll (occlude would block a pane that
                    // has nothing to scroll).
                    rows.into_any_element()
                }),
        )
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
                    .when_some(self.render_readme_section(cx), |this, section| {
                        this.child(section)
                    })
                    .when_some(self.render_scripts_section(cx), |this, section| {
                        this.child(section)
                    })
                    .when_some(self.render_actions_section(cx), |this, section| {
                        this.child(section)
                    })
                    .child(self.render_devices_section(cx))
                    .child(self.render_toolchain_section(cx))
                    .when_some(self.render_apple_toolchain_section(cx), |this, section| {
                        this.child(section)
                    })
                    .when_some(self.render_terminals_section(cx), |this, section| {
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
