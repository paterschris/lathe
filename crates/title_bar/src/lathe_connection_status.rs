use crate::TitleBar;
use auto_update::AutoUpdateStatus;
use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, div,
};
use ui::{Button, Clickable, Icon, IconName, IconSize, LabelSize, Tooltip};

pub fn render(status: &client::Status, cx: &mut Context<TitleBar>) -> Option<AnyElement> {
    match status {
        client::Status::ConnectionError
        | client::Status::ConnectionLost
        | client::Status::Reauthenticating
        | client::Status::Reconnecting
        | client::Status::ReconnectionError { .. } => Some(
            div()
                .id("disconnected")
                .child(Icon::new(IconName::Disconnected).size(IconSize::Small))
                .tooltip(Tooltip::text("Disconnected"))
                .into_any_element(),
        ),
        client::Status::UpgradeRequired => Some(
            Button::new("connection-status", update_required_label(cx))
                .label_size(LabelSize::Small)
                .on_click(|_, window, cx| {
                    if let Some(auto_updater) = auto_update::AutoUpdater::get(cx)
                        && auto_updater.read(cx).status().is_updated()
                    {
                        workspace::reload(cx);
                        return;
                    }
                    auto_update::check(&Default::default(), window, cx);
                })
                .into_any_element(),
        ),
        _ => None,
    }
}

fn update_required_label(cx: &mut Context<TitleBar>) -> &'static str {
    let auto_updater = auto_update::AutoUpdater::get(cx);
    match auto_updater.map(|auto_update| auto_update.read(cx).status()) {
        Some(AutoUpdateStatus::Updated { .. }) => "Please restart Lathe to Collaborate",
        Some(AutoUpdateStatus::Installing { .. })
        | Some(AutoUpdateStatus::Downloading { .. })
        | Some(AutoUpdateStatus::Checking) => "Updating...",
        Some(AutoUpdateStatus::Idle) | Some(AutoUpdateStatus::Errored { .. }) | None => {
            "Please update Lathe to Collaborate"
        }
    }
}
