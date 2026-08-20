use gpui::Pixels;
use settings::{IntoGpui, RegisterSetting, Settings};
use workspace::dock::DockPosition;

#[derive(Debug, RegisterSetting)]
pub struct PullRequestPanelSettings {
    pub button: bool,
    pub dock: DockPosition,
    pub default_width: Pixels,
}

impl Settings for PullRequestPanelSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let panel = content.pull_request_panel.as_ref().unwrap();
        Self {
            button: panel.button.unwrap(),
            dock: panel.dock.unwrap().into(),
            default_width: panel.default_width.unwrap().into_gpui(),
        }
    }
}
