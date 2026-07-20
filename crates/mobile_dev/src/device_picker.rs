//! Status-bar Android device selector. Hidden unless the workspace contains
//! a detected Expo project, so non-mobile projects never see it.

use gpui::{
    Action as _, App, Context, Empty, Entity, IntoElement, Render, SharedString, Subscription,
    Window,
};
use settings::Settings as _;
use ui::prelude::*;
use ui::{Button, ContextMenu, IconPosition, PopoverMenu, Tooltip};
use workspace::{HideStatusItem, StatusBarSettings, StatusItemView, Workspace, item::ItemHandle};

use crate::{MobileDevPanel, PickDevice, ToggleFocus};

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
        let (devices, selected_serial, expo_detected) = {
            let panel = panel.read(cx);
            (
                panel.device_state.devices.clone(),
                panel.selected_serial.clone(),
                panel.expo_project.is_some(),
            )
        };
        if !expo_detected {
            return Empty.into_any_element();
        }

        let selected_label = selected_serial
            .as_ref()
            .and_then(|serial| devices.iter().find(|device| &device.serial == serial))
            .map(|device| device.label())
            .unwrap_or_else(|| SharedString::from("No device"));

        PopoverMenu::new("mobile-device-selector")
            .trigger(
                Button::new("mobile-device-selector-trigger", selected_label)
                    .label_size(LabelSize::Small)
                    .tooltip(|_window, cx| {
                        Tooltip::for_action("Select Android Device", &PickDevice, cx)
                    }),
            )
            .menu(move |window, cx| {
                let panel = panel.clone();
                Some(ContextMenu::build(
                    window,
                    cx,
                    move |mut menu, _window, cx| {
                        let devices = panel.read(cx).device_state.devices.clone();
                        let selected_serial = panel.read(cx).selected_serial.clone();
                        menu = menu.header("Android Devices");
                        if devices.is_empty() {
                            menu = menu.label("No devices detected");
                        }
                        for device in devices {
                            let toggled = selected_serial.as_ref() == Some(&device.serial);
                            let serial = device.serial.clone();
                            let panel = panel.clone();
                            menu = menu.toggleable_entry(
                                device.label(),
                                toggled,
                                IconPosition::Start,
                                None,
                                move |_window, cx| {
                                    panel.update(cx, |panel, cx| {
                                        panel.select_device(serial.clone(), cx);
                                    });
                                },
                            );
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
