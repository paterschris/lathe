use collections::HashMap;

use editor::EditorSettings;
use gpui::App;
use settings::{RegisterSetting, Settings};

#[derive(Debug, Default, RegisterSetting)]
pub struct JupyterSettings {
    pub kernel_selections: HashMap<String, String>,
    pub clear_outputs_on_save: bool,
    pub language_server_sidecar: bool,
}

impl JupyterSettings {
    pub fn enabled(cx: &App) -> bool {
        // In order to avoid a circular dependency between `editor` and `repl` crates,
        // we put the `enable` flag on its settings.
        // This allows the editor to set up context for key bindings/actions.
        EditorSettings::jupyter_enabled(cx)
    }

    pub fn notebook_enabled(cx: &App) -> bool {
        EditorSettings::notebook_enabled(cx)
    }
}

impl Settings for JupyterSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let jupyter = content.editor.jupyter.clone().unwrap();
        Self {
            kernel_selections: jupyter.kernel_selections.unwrap_or_default(),
            clear_outputs_on_save: jupyter.clear_outputs_on_save.unwrap_or(false),
            language_server_sidecar: jupyter.language_server_sidecar.unwrap_or(false),
        }
    }
}
