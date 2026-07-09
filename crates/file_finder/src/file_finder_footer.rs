use crate::{
    FileFinderDelegate, OpenWithoutDismiss, ToggleFilterMenu, ToggleSplitMenu,
};
use gpui::{Action, AnyElement, Context, ParentElement, px};
use picker::Picker;
use ui::{
    ButtonLike, ContextMenu, Indicator, KeyBinding, PopoverMenu, TintColor, Tooltip, prelude::*,
};
use workspace::pane;
use zed_actions::search::ToggleIncludeIgnored;

pub(super) fn render(
    delegate: &FileFinderDelegate,
    cx: &mut Context<Picker<FileFinderDelegate>>,
) -> Option<AnyElement> {
    let focus_handle = delegate.focus_handle.clone();

    Some(
        h_flex()
            .w_full()
            .p_1p5()
            .justify_between()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                PopoverMenu::new("filter-menu-popover")
                    .with_handle(delegate.filter_popover_menu_handle.clone())
                    .attach(gpui::Anchor::BottomRight)
                    .anchor(gpui::Anchor::BottomLeft)
                    .offset(gpui::Point {
                        x: px(1.0),
                        y: px(1.0),
                    })
                    .trigger_with_tooltip(
                        IconButton::new("filter-trigger", IconName::Sliders)
                            .icon_size(IconSize::Small)
                            .icon_size(IconSize::Small)
                            .toggle_state(delegate.include_ignored.unwrap_or(false))
                            .when(delegate.include_ignored.is_some(), |this| {
                                this.indicator(Indicator::dot().color(Color::Info))
                            }),
                        {
                            let focus_handle = focus_handle.clone();
                            move |_window, cx| {
                                Tooltip::for_action_in(
                                    "Filter Options",
                                    &ToggleFilterMenu,
                                    &focus_handle,
                                    cx,
                                )
                            }
                        },
                    )
                    .menu({
                        let focus_handle = focus_handle.clone();
                        let include_ignored = delegate.include_ignored;

                        move |window, cx| {
                            Some(ContextMenu::build(window, cx, {
                                let focus_handle = focus_handle.clone();
                                move |menu, _, _| {
                                    menu.context(focus_handle.clone())
                                        .header("Filter Options")
                                        .toggleable_entry(
                                            "Include Ignored Files",
                                            include_ignored.unwrap_or(false),
                                            ui::IconPosition::End,
                                            Some(ToggleIncludeIgnored.boxed_clone()),
                                            move |window, cx| {
                                                window.focus(&focus_handle, cx);
                                                window.dispatch_action(
                                                    ToggleIncludeIgnored.boxed_clone(),
                                                    cx,
                                                );
                                            },
                                        )
                                }
                            }))
                        }
                    }),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        PopoverMenu::new("split-menu-popover")
                            .with_handle(delegate.split_popover_menu_handle.clone())
                            .attach(gpui::Anchor::BottomRight)
                            .anchor(gpui::Anchor::BottomLeft)
                            .offset(gpui::Point {
                                x: px(1.0),
                                y: px(1.0),
                            })
                            .trigger(
                                ButtonLike::new("split-trigger")
                                    .child(Label::new("Split…"))
                                    .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                                    .child(
                                        KeyBinding::for_action_in(
                                            &ToggleSplitMenu,
                                            &focus_handle,
                                            cx,
                                        )
                                        .size(rems_from_px(12.)),
                                    ),
                            )
                            .menu({
                                let focus_handle = focus_handle.clone();

                                move |window, cx| {
                                    Some(ContextMenu::build(window, cx, {
                                        let focus_handle = focus_handle.clone();
                                        move |menu, _, _| {
                                            menu.context(focus_handle)
                                                .action(
                                                    "Split Left",
                                                    pane::SplitLeft::default().boxed_clone(),
                                                )
                                                .action(
                                                    "Split Right",
                                                    pane::SplitRight::default().boxed_clone(),
                                                )
                                                .action(
                                                    "Split Up",
                                                    pane::SplitUp::default().boxed_clone(),
                                                )
                                                .action(
                                                    "Split Down",
                                                    pane::SplitDown::default().boxed_clone(),
                                                )
                                        }
                                    }))
                                }
                            }),
                    )
                    .child(
                        Button::new("open-without-dismiss", "Keep Open")
                            .key_binding(
                                KeyBinding::for_action_in(&OpenWithoutDismiss, &focus_handle, cx)
                                    .map(|kb| kb.size(rems_from_px(12.))),
                            )
                            .on_click(|_, window, cx| {
                                window.dispatch_action(OpenWithoutDismiss.boxed_clone(), cx)
                            }),
                    )
                    .child(
                        Button::new("open-selection", "Open")
                            .key_binding(
                                KeyBinding::for_action_in(&menu::Confirm, &focus_handle, cx)
                                    .map(|kb| kb.size(rems_from_px(12.))),
                            )
                            .on_click(|_, window, cx| {
                                window.dispatch_action(menu::Confirm.boxed_clone(), cx)
                            }),
                    ),
            )
            .into_any(),
    )
}
