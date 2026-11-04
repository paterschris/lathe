use crate::{Companion, CompanionSettings, ConnectionStatus, PairingState};
use gpui::{
    Bounds, ClipboardItem, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Hsla, Subscription, Task,
    canvas,
    fill, point, px, size,
};
use qrcodegen::{QrCode, QrCodeEcc};
use settings::Settings as _;
use std::time::{Duration, SystemTime};
use ui::{Indicator, Switch, Tooltip, prelude::*};
use workspace::{ModalView, Workspace};

gpui::actions!(
    lathe_companion,
    [
        /// Open Lathe Companion to pair a phone and manage paired phones.
        OpenCompanion
    ]
);

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &OpenCompanion, window, cx| {
        let Some(companion) = Companion::global(cx) else {
            return;
        };
        workspace.toggle_modal(window, cx, |_, cx| CompanionModal::new(companion, cx));
    });
}

/// Phone icon in the title bar while the companion is on. Its color shows the
/// connection, and clicking it opens the companion dialog.
struct TitleBarIndicator {
    companion: Entity<Companion>,
    _subscription: Subscription,
}

pub(crate) fn register_title_bar_indicator(cx: &mut App) {
    let Some(companion) = Companion::global(cx) else {
        return;
    };
    let indicator = cx.new(|cx| TitleBarIndicator {
        _subscription: cx.observe(&companion, |_, _, cx| cx.notify()),
        companion,
    });
    lathe_hooks::LatheHooks::update(cx, |hooks| {
        hooks
            .title_bar_status_items
            .push(std::rc::Rc::new(move |_, cx| {
                let visible = CompanionSettings::get_global(cx).enabled;
                visible.then(|| indicator.clone().into_any_element())
            }));
    });
}

impl Render for TitleBarIndicator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let companion = self.companion.read(cx);
        let online = companion
            .devices()
            .iter()
            .filter(|device| companion.is_online(&device.device_id))
            .map(|device| device.name.clone())
            .collect::<Vec<_>>();
        let (color, tooltip): (Color, SharedString) = match companion.status() {
            ConnectionStatus::Connected if !online.is_empty() => {
                (Color::Accent, format!("Lathe Companion: {} connected", online.join(", ")).into())
            }
            ConnectionStatus::Connected if companion.devices().is_empty() => {
                (Color::Muted, "Lathe Companion: no phone paired".into())
            }
            ConnectionStatus::Connected => (Color::Muted, "Lathe Companion: no phone connected".into()),
            ConnectionStatus::Off | ConnectionStatus::Connecting => {
                (Color::Muted, "Lathe Companion: connecting…".into())
            }
            ConnectionStatus::Reconnecting { .. } => {
                (Color::Warning, "Lathe Companion: reconnecting to the relay…".into())
            }
            ConnectionStatus::Stopped { .. } | ConnectionStatus::Failed { .. } => {
                (Color::Error, "Lathe Companion: not connected".into())
            }
        };
        IconButton::new("lathe-companion-indicator", IconName::Phone)
            .icon_size(IconSize::Small)
            .icon_color(color)
            .tooltip(Tooltip::text(tooltip))
            .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenCompanion), cx))
    }
}

pub struct CompanionModal {
    companion: Entity<Companion>,
    focus_handle: FocusHandle,
    error: Option<SharedString>,
    _subscription: Subscription,
    _countdown: Task<()>,
}

impl CompanionModal {
    fn new(companion: Entity<Companion>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&companion, |_, _, cx| cx.notify());
        cx.on_release(|this: &mut Self, cx| {
            // Closing the dialog abandons a pairing in progress, so a QR code
            // left on screen elsewhere can't be used later.
            this.companion
                .update(cx, |companion, cx| companion.cancel_pairing(cx));
        })
        .detach();
        // Keeps the pairing code's expiry countdown current.
        let countdown = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(15)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
        });
        Self {
            companion,
            focus_handle: cx.focus_handle(),
            error: None,
            _subscription: subscription,
            _countdown: countdown,
        }
    }

    fn set_terminal_input(&mut self, allowed: bool, cx: &mut Context<Self>) {
        settings::update_settings_file(<dyn fs::Fs>::global(cx), cx, move |settings, _| {
            settings.companion.get_or_insert_default().terminal_input = Some(allowed);
        });
    }

    fn render_terminal_input(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let allowed = CompanionSettings::get_global(cx).terminal_input;
        v_flex()
            .gap_1()
            .child(
                Switch::new(
                    "companion-terminal-input",
                    if allowed { ToggleState::Selected } else { ToggleState::Unselected },
                )
                .label("Allow typing into terminals from the phone")
                .on_click(cx.listener(|this, state: &ToggleState, _, cx| {
                    this.set_terminal_input(matches!(state, ToggleState::Selected), cx)
                })),
            )
            .child(
                Label::new(
                    "A paired phone can always see terminals. With this on, it can also type \
                     into them, which can run any command.",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
    }

    fn set_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        settings::update_settings_file(<dyn fs::Fs>::global(cx), cx, move |settings, _| {
            settings.companion.get_or_insert_default().enabled = Some(enabled);
        });
    }

    fn render_status(&self, cx: &App) -> impl IntoElement {
        let (color, text): (Color, SharedString) = match self.companion.read(cx).status() {
            ConnectionStatus::Off => (Color::Muted, "Off".into()),
            ConnectionStatus::Connecting => (Color::Muted, "Connecting…".into()),
            ConnectionStatus::Connected => (Color::Success, "Connected to the relay".into()),
            ConnectionStatus::Reconnecting { .. } => (Color::Warning, "Reconnecting…".into()),
            ConnectionStatus::Stopped { reason } => (
                Color::Error,
                format!("Stopped: {reason}. Is another Lathe using this identity?").into(),
            ),
            ConnectionStatus::Failed { error } => (Color::Error, error.clone().into()),
        };
        h_flex()
            .gap_2()
            .child(Indicator::dot().color(color))
            .child(Label::new(text).size(LabelSize::Small).color(Color::Muted))
    }

    fn start_pairing(&mut self, cx: &mut Context<Self>) {
        let result = self
            .companion
            .update(cx, |companion, cx| companion.start_pairing(cx));
        self.error = result.err().map(|error| error.to_string().into());
        cx.notify();
    }

    fn render_pairing(&self, cx: &mut Context<Self>) -> AnyElement {
        let companion = self.companion.read(cx);
        let connected = *companion.status() == ConnectionStatus::Connected;
        match companion.pairing().cloned() {
            None => Button::new("pair-phone", "Pair a phone")
                .style(ButtonStyle::Filled)
                .disabled(!connected)
                .on_click(cx.listener(|this, _, _, cx| this.start_pairing(cx)))
                .into_any_element(),
            Some(PairingState::WaitingForScan {
                qr_payload,
                web_link,
                expires_at,
            }) => {
                let minutes_left = expires_at
                    .duration_since(SystemTime::now())
                    .map(|left| left.as_secs().div_ceil(60))
                    .unwrap_or(0);
                if minutes_left == 0 {
                    // An expired code can't pair, so it isn't shown at all.
                    return v_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            Label::new("This code has expired.")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            Button::new("regenerate-pairing-code", "New code")
                                .style(ButtonStyle::Filled)
                                .disabled(!connected)
                                .on_click(cx.listener(|this, _, _, cx| this.start_pairing(cx))),
                        )
                        .into_any_element();
                }
                // An https link opens from a message or a browser on any phone.
                // Simulators and emulators open it too: `xcrun simctl openurl
                // booted <link>` or `adb shell am start -d <link>`.
                let link = web_link.unwrap_or_else(|| qr_payload.clone());
                v_flex()
                    .gap_2()
                    .items_center()
                    .child(render_qr_code(&qr_payload))
                    .child(
                        Button::new("copy-pairing-link", "Copy link")
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(link.clone()))
                            }),
                    )
                    .child(
                        Label::new(format!(
                            "Scan with the Lathe Companion app. The code expires in {minutes_left} min."
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .into_any_element()
            }
            Some(PairingState::AwaitingApproval(request)) => v_flex()
                .gap_2()
                .child(Label::new(format!(
                    "Allow \"{}\" to follow and control agents in Lathe?",
                    request.device_name
                )))
                .child(
                    Label::new("Only allow a phone you just scanned this code with.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("accept-pairing", "Allow")
                                .style(ButtonStyle::Filled)
                                .on_click(cx.listener(|this, _, _, cx| this.respond(true, cx))),
                        )
                        .child(
                            Button::new("reject-pairing", "Deny")
                                .on_click(cx.listener(|this, _, _, cx| this.respond(false, cx))),
                        ),
                )
                .into_any_element(),
            Some(PairingState::Paired { device_name }) => {
                Label::new(format!("Paired with \"{device_name}\"."))
                    .color(Color::Success)
                    .into_any_element()
            }
        }
    }

    fn respond(&mut self, accept: bool, cx: &mut Context<Self>) {
        let result = self
            .companion
            .update(cx, |companion, cx| companion.respond_to_pairing(accept, cx));
        self.error = result.err().map(|error| error.to_string().into());
        cx.notify();
    }

    fn render_devices(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let companion = self.companion.read(cx);
        let rows = companion
            .devices()
            .iter()
            .map(|device| {
                let online = companion.is_online(&device.device_id);
                let device_id = device.device_id.clone();
                h_flex()
                    .id(SharedString::from(format!("device-{}", device.device_id)))
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Indicator::dot().color(if online {
                                Color::Success
                            } else {
                                Color::Muted
                            }))
                            .child(Label::new(device.name.clone())),
                    )
                    .child(
                        Button::new(
                            SharedString::from(format!("remove-{}", device.device_id)),
                            "Remove",
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.companion
                                .update(cx, |companion, cx| companion.revoke(&device_id, cx));
                        })),
                    )
            })
            .collect::<Vec<_>>();
        v_flex()
            .gap_1()
            .child(Label::new("Paired phones").size(LabelSize::Small).color(Color::Muted))
            .when(rows.is_empty(), |this| {
                this.child(Label::new("None yet").size(LabelSize::Small).color(Color::Muted))
            })
            .children(rows)
    }
}

/// Dark modules on white with a four-module quiet zone, whatever the theme:
/// phone scanners expect that contrast.
fn render_qr_code(payload: &str) -> AnyElement {
    let Ok(code) = QrCode::encode_text(payload, QrCodeEcc::Medium) else {
        return Label::new("The pairing link is too long for a QR code.")
            .color(Color::Error)
            .into_any_element();
    };
    const MODULE: f32 = 4.;
    const QUIET_ZONE: i32 = 4;
    let modules = code.size() + QUIET_ZONE * 2;
    let side = px(MODULE * modules as f32);
    canvas(
        |_, _, _| {},
        move |bounds: Bounds<gpui::Pixels>, _, window, _| {
            let white: Hsla = gpui::white();
            let black: Hsla = gpui::black();
            window.paint_quad(fill(bounds, white));
            for y in 0..code.size() {
                for x in 0..code.size() {
                    if code.get_module(x, y) {
                        let origin = bounds.origin
                            + point(
                                px(MODULE * (x + QUIET_ZONE) as f32),
                                px(MODULE * (y + QUIET_ZONE) as f32),
                            );
                        window.paint_quad(fill(
                            Bounds::new(origin, size(px(MODULE), px(MODULE))),
                            black,
                        ));
                    }
                }
            }
        },
    )
    .size(side)
    .into_any_element()
}

impl EventEmitter<DismissEvent> for CompanionModal {}

impl Focusable for CompanionModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for CompanionModal {}

impl Render for CompanionModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let enabled = CompanionSettings::get_global(cx).enabled;
        v_flex()
            .key_context("CompanionModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .elevation_3(cx)
            .w(rems(26.))
            .p_4()
            .gap_3()
            .child(Label::new("Lathe Companion").size(LabelSize::Large))
            .child(
                Label::new(
                    "Follow agent threads, approve tool calls, and reply from your phone. \
                     Messages are end-to-end encrypted through the relay.",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .map(|this| {
                if enabled {
                    this.child(self.render_status(cx))
                        .child(self.render_pairing(cx))
                        .child(self.render_devices(cx))
                        .child(self.render_terminal_input(cx))
                } else {
                    this.child(
                        Button::new("enable-companion", "Turn on")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, _, cx| this.set_enabled(true, cx))),
                    )
                }
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).color(Color::Error).size(LabelSize::Small))
            })
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        Button::new("disable-companion", "Turn off")
                            .when(!enabled, |this| this.disabled(true))
                            .on_click(cx.listener(|this, _, _, cx| this.set_enabled(false, cx))),
                    )
                    .child(
                        Button::new("close-companion", "Close")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    ),
            )
    }
}
