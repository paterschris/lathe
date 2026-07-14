use crate::TitleBar;
use gpui::{
    Animation, AnimationExt, AnyElement, Context, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, div, pulsating_between,
};
use std::time::Duration;
use ui::{Color, CountBadge, FluentBuilder, Icon, IconName, IconSize, Tooltip};

impl TitleBar {
    pub(super) fn render_awaiting_input_indicator(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let workspace = self.workspace.upgrade()?;
        let count = workspace.read(cx).awaiting_input_count(cx);
        if count == 0 {
            return None;
        }
        let workspace_handle = self.workspace.clone();
        let tooltip = workspace.read(cx).first_awaiting_input_tooltip(cx);
        let tooltip_text = if count > 1 {
            format!("{count} terminals awaiting input — click to focus")
        } else {
            format!("{tooltip} — click to focus")
        };
        Some(
            div()
                .id("terminal-awaiting-input")
                .cursor_pointer()
                .relative()
                .on_click(move |_, window, cx| {
                    if let Some(workspace) = workspace_handle.upgrade() {
                        workspace.update(cx, |workspace, cx| {
                            workspace.focus_first_awaiting_input(window, cx);
                        });
                    }
                })
                .tooltip(Tooltip::text(tooltip_text))
                .child(
                    Icon::new(IconName::Return)
                        .size(IconSize::Small)
                        .color(Color::Accent),
                )
                .when(count > 1, |this| this.child(CountBadge::new(count)))
                .with_animation(
                    "titlebar-awaiting-pulse",
                    Animation::new(Duration::from_secs(2))
                        .repeat()
                        .with_easing(pulsating_between(0.4, 1.0)),
                    |element, delta| element.opacity(delta),
                )
                .into_any_element(),
        )
    }
}
