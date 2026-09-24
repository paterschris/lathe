//! Prompt shown when Lathe wants to download and execute something and the user has asked to be
//! consulted first. See [`project::binary_download_consent`] for the decision model.

use collections::HashMap;
use gpui::{DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, ScrollHandle};
use project::binary_download_consent::{
    BinaryDownloadConsent, BinaryDownloadRequest, ConsentDecision,
};
use theme::ActiveTheme;
use ui::{AlertModal, Checkbox, ToggleState, WithScrollbar, prelude::*};

use crate::{DismissDecision, ModalView};

pub struct BinaryDownloadModal {
    consent: Entity<BinaryDownloadConsent>,
    /// Every request pending when the modal was opened or last refreshed, paired with whether the
    /// user currently has it ticked. Approving is opt-in, so everything starts unchecked.
    items: Vec<(BinaryDownloadRequest, bool)>,
    /// Items the user marked as "always allow", which skips the version check on future updates.
    always: HashMap<SharedString, bool>,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    /// Set once a decision has been submitted, so dismissing afterwards does not also deny.
    resolved: bool,
}

impl BinaryDownloadModal {
    pub fn new(
        consent: Entity<BinaryDownloadConsent>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            consent,
            items: Vec::new(),
            always: HashMap::default(),
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
            resolved: false,
        };
        this.refresh(cx);
        this
    }

    /// Pull the current pending set in, keeping any ticks the user already made.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let previous = self
            .items
            .iter()
            .map(|(request, approved)| (request.name.clone(), *approved))
            .collect::<HashMap<_, _>>();

        self.items = self
            .consent
            .read(cx)
            .pending()
            .cloned()
            .map(|request| {
                let approved = previous.get(&request.name).copied().unwrap_or(false);
                (request, approved)
            })
            .collect();
        cx.notify();
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn toggle(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some((_, approved)) = self.items.get_mut(index) {
            *approved = !*approved;
            cx.notify();
        }
    }

    fn toggle_always(&mut self, name: SharedString, cx: &mut Context<Self>) {
        let entry = self.always.entry(name).or_insert(false);
        *entry = !*entry;
        cx.notify();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let decisions = self
            .items
            .iter()
            .map(|(request, approved)| {
                let decision = if !*approved {
                    ConsentDecision::Deny
                } else if self.always.get(&request.name).copied().unwrap_or(false) {
                    ConsentDecision::ApproveAlways
                } else {
                    ConsentDecision::ApproveVersion
                };
                (request.clone(), decision)
            })
            .collect::<Vec<_>>();

        self.consent.update(cx, |consent, cx| {
            consent.resolve(&decisions, cx);
        });
        self.resolved = true;
        cx.emit(DismissEvent);
    }

    fn deny_all(&mut self, cx: &mut Context<Self>) {
        self.consent.update(cx, |consent, cx| consent.deny_all(cx));
        self.resolved = true;
        cx.emit(DismissEvent);
    }
}

impl Focusable for BinaryDownloadModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for BinaryDownloadModal {}

impl ModalView for BinaryDownloadModal {
    fn fade_out_background(&self) -> bool {
        true
    }

    fn on_before_dismiss(&mut self, _: &mut Window, cx: &mut Context<Self>) -> DismissDecision {
        // Closing the prompt without deciding must not leave the download paths waiting forever.
        if !self.resolved {
            self.consent.update(cx, |consent, cx| consent.deny_all(cx));
        }
        DismissDecision::Dismiss(true)
    }
}

impl Render for BinaryDownloadModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.items.len();
        let header_label: SharedString = if count == 1 {
            "Lathe wants to download and run 1 item".into()
        } else {
            format!("Lathe wants to download and run {count} items").into()
        };

        let approved_count = self
            .items
            .iter()
            .filter(|(_, approved)| *approved)
            .count();

        let rows = self
            .items
            .iter()
            .enumerate()
            .map(|(index, (request, approved))| {
                let name = request.name.clone();
                let always = self.always.get(&name).copied().unwrap_or(false);
                let detail: SharedString = match &request.version {
                    Some(version) => format!("{} {}", request.kind.label(), version).into(),
                    None => format!("{} (version unknown)", request.kind.label()).into(),
                };

                v_flex()
                    .p_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Checkbox::new(("approve", index), ToggleState::from(*approved))
                                    .on_click(cx.listener(move |this, _, _window, cx| {
                                        this.toggle(index, cx);
                                    })),
                            )
                            .child(Label::new(name))
                            .child(Label::new(detail).color(Color::Muted).size(LabelSize::Small)),
                    )
                    .when(!request.url.is_empty(), |this| {
                        this.child(
                            Label::new(request.url.clone())
                                .color(Color::Muted)
                                .size(LabelSize::XSmall),
                        )
                    })
                    .when(!request.verified, |this| {
                        // The complaint this feature answers is largely about running code that
                        // was never checked, so say so plainly at the point of decision.
                        this.child(
                            h_flex()
                                .gap_1()
                                .child(Icon::new(IconName::Warning).color(Color::Warning))
                                .child(
                                    Label::new("No checksum published; integrity cannot be verified")
                                        .color(Color::Warning)
                                        .size(LabelSize::XSmall),
                                ),
                        )
                    })
                    .child(
                        h_flex().pl_5().child(
                            Checkbox::new(("always", index), ToggleState::from(always))
                                .label("Always allow, including future updates")
                                .on_click(cx.listener(move |this, _, _window, cx| {
                                    let name = this
                                        .items
                                        .get(index)
                                        .map(|(request, _)| request.name.clone());
                                    if let Some(name) = name {
                                        this.toggle_always(name, cx);
                                    }
                                })),
                        ),
                    )
            })
            .collect::<Vec<_>>();

        AlertModal::new("binary-download-modal")
            .width(rems(34.))
            .key_context("BinaryDownloadModal")
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(|this, _: &menu::Confirm, _window, cx| {
                this.submit(cx);
            }))
            .on_action(cx.listener(|this, _: &menu::Cancel, _window, cx| {
                this.deny_all(cx);
            }))
            .title(header_label)
            .header(
                v_flex()
                    .p_3()
                    .gap_1()
                    .bg(cx.theme().colors().editor_background.opacity(0.5))
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        Label::new(
                            "These will be downloaded from the internet and executed on your \
                            machine. Approvals are remembered for the version shown.",
                        )
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                    ),
            )
            .child(
                div()
                    .max_h(rems(24.))
                    .overflow_hidden()
                    .vertical_scrollbar_for(&self.scroll_handle, window, cx)
                    .child(v_flex().children(rows)),
            )
            .primary_action(if approved_count == 0 {
                "Deny All"
            } else if approved_count == count {
                "Allow All"
            } else {
                "Allow Selected"
            })
            .dismiss_label("Deny All")
    }
}
