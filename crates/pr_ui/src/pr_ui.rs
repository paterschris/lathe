use workspace::Workspace;

pub mod pull_request_panel;
pub mod pull_request_panel_settings;
pub mod pull_request_picker;
pub mod pull_request_view;

pub use pull_request_panel::PullRequestPanel;
pub use pull_request_view::PullRequestView;

pub fn init(cx: &mut gpui::App) {
    cx.observe_new(|workspace: &mut Workspace, _, _cx| {
        register(workspace);
    })
    .detach();
}

pub fn register(workspace: &mut Workspace) {
    pull_request_panel::register(workspace);
    pull_request_picker::register(workspace);
}
