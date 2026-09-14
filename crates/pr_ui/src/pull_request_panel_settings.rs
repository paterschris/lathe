use std::time::Duration;

use gpui::Pixels;
use settings::{IntoGpui, RegisterSetting, Settings};
use workspace::dock::DockPosition;

/// Floor for `auto_refresh_interval`. The panel issues one list request per
/// repository section plus a reviewer request per PR, so a small interval
/// multiplies quickly into the host's rate limit.
const MIN_AUTO_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, RegisterSetting)]
pub struct PullRequestPanelSettings {
    pub button: bool,
    pub dock: DockPosition,
    pub default_width: Pixels,
    pub auto_refresh: bool,
    pub auto_refresh_interval: Duration,
}

impl Settings for PullRequestPanelSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let panel = content.pull_request_panel.as_ref().unwrap();
        Self {
            button: panel.button.unwrap(),
            dock: panel.dock.unwrap().into(),
            default_width: panel.default_width.unwrap().into_gpui(),
            auto_refresh: panel.auto_refresh.unwrap(),
            auto_refresh_interval: Duration::from_secs(
                panel.auto_refresh_interval_seconds.unwrap(),
            )
            .max(MIN_AUTO_REFRESH_INTERVAL),
        }
    }
}
