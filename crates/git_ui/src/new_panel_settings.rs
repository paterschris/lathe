use gpui::Pixels;
use settings::{RegisterSetting, Settings};
use ui::px;
use workspace::dock::DockPosition;

/// Settings that used to live on a standalone Repository Dashboard panel.
/// The panel was folded into the git panel's repos strip; what remains is the
/// pinned-external-repos list, which the strip surfaces below the workspace
/// repos.
#[derive(Debug, RegisterSetting)]
pub struct RepositoryDashboardPanelSettings {
    pub pinned_repos: Vec<String>,
}

impl Settings for RepositoryDashboardPanelSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            pinned_repos: content
                .repository_dashboard_pinned_repos
                .clone()
                .unwrap_or_default(),
        }
    }
}

// `PullRequestPanelSettings` moved to the `pr_ui` crate (see
// `pr_ui::pull_request_panel_settings::PullRequestPanelSettings`).

#[derive(Debug, RegisterSetting)]
pub struct GitActivityPanelSettings {
    pub button: bool,
    pub dock: DockPosition,
    pub default_width: Pixels,
}

impl Settings for GitActivityPanelSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let panel = content.git_activity_panel.as_ref().unwrap();
        Self {
            button: panel.button.unwrap(),
            dock: panel.dock.unwrap().into(),
            default_width: panel.default_width.map(px).unwrap(),
        }
    }
}
