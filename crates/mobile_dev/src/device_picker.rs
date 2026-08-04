//! Status-bar mobile device selector (Android devices plus iOS simulators
//! and devices on macOS). Hidden unless the workspace contains a detected
//! Expo project, so non-mobile projects never see it.

use gpui::{
    Action as _, App, Context, Empty, Entity, IntoElement, Render, SharedString, Subscription,
    Window,
};
use settings::Settings as _;
use ui::prelude::*;
use ui::{Button, ContextMenu, IconPosition, PopoverMenu, Tooltip};
use workspace::{HideStatusItem, StatusBarSettings, StatusItemView, Workspace, item::ItemHandle};

use crate::build::MobilePlatform;
use crate::{MobileDevPanel, PickDevice, SelectedDevice, ToggleFocus};

pub struct MobileDeviceSelector {
    panel: Option<Entity<MobileDevPanel>>,
    _workspace_subscription: Option<Subscription>,
    _panel_observation: Option<Subscription>,
}

impl MobileDeviceSelector {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        // The panel is registered asynchronously (`add_panel_when_ready`), so
        // it usually doesn't exist yet when status items are built; watch for
        // it instead of looking it up once.
        let workspace_subscription = workspace.weak_handle().upgrade().map(|workspace| {
            cx.subscribe(&workspace, |this, workspace, event, cx| {
                if this.panel.is_none()
                    && let workspace::Event::PanelAdded(_) = event
                    && let Some(panel) = workspace.read(cx).panel::<MobileDevPanel>(cx)
                {
                    this.attach_panel(panel, cx);
                }
            })
        });

        let mut this = Self {
            panel: None,
            _workspace_subscription: workspace_subscription,
            _panel_observation: None,
        };
        if let Some(panel) = workspace.panel::<MobileDevPanel>(cx) {
            this.attach_panel(panel, cx);
        }
        this
    }

    fn attach_panel(&mut self, panel: Entity<MobileDevPanel>, cx: &mut Context<Self>) {
        self._panel_observation = Some(cx.observe(&panel, |_, _, cx| cx.notify()));
        self.panel = Some(panel);
        cx.notify();
    }
}

impl Render for MobileDeviceSelector {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !StatusBarSettings::get_global(cx).mobile_device_selector_button {
            return Empty.into_any_element();
        }
        let Some(panel) = self.panel.clone() else {
            return Empty.into_any_element();
        };
        let (devices, apple_devices, selected_device, project_detected) = {
            let panel = panel.read(cx);
            (
                panel.device_state.devices.clone(),
                panel.apple_device_state.devices.clone(),
                panel.selected_device.clone(),
                panel.mobile_project.is_some(),
            )
        };
        if !project_detected {
            return Empty.into_any_element();
        }

        let selected_label = selected_device
            .as_ref()
            .and_then(|selected| match selected.platform {
                MobilePlatform::Android => devices
                    .iter()
                    .find(|device| device.serial == selected.id)
                    .map(|device| device.label()),
                MobilePlatform::Ios => apple_devices
                    .iter()
                    .find(|device| device.udid == selected.id)
                    .map(|device| device.label()),
            })
            .unwrap_or_else(|| SharedString::from("No device"));

        PopoverMenu::new("mobile-device-selector")
            .trigger(
                Button::new("mobile-device-selector-trigger", selected_label)
                    .label_size(LabelSize::Small)
                    .tooltip(|_window, cx| Tooltip::for_action("Select Device", &PickDevice, cx)),
            )
            .menu(move |window, cx| {
                let panel = panel.clone();
                Some(ContextMenu::build(
                    window,
                    cx,
                    move |mut menu, _window, cx| {
                        let devices = panel.read(cx).device_state.devices.clone();
                        let apple_devices = panel.read(cx).apple_device_state.devices.clone();
                        let selected = panel.read(cx).selected_device.clone();
                        menu = menu.header("Android");
                        if devices.is_empty() {
                            menu = menu.label("No Android devices");
                        }
                        for device in devices {
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
                                        panel.select_device(selection.clone(), cx);
                                    });
                                },
                            );
                        }
                        if cfg!(target_os = "macos") {
                            menu = menu.header("iOS");
                            if apple_devices.is_empty() {
                                menu = menu.label("No iOS devices");
                            }
                            for device in apple_devices {
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
                                            panel.select_device(selection.clone(), cx);
                                        });
                                    },
                                );
                            }
                        }
                        menu.separator()
                            .action("Open Mobile Panel", ToggleFocus.boxed_clone())
                    },
                ))
            })
            .into_any_element()
    }
}

impl StatusItemView for MobileDeviceSelector {
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
                .mobile_device_selector_button = Some(false);
        }))
    }
}
