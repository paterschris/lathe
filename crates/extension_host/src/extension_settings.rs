use collections::HashMap;
use extension::{
    DownloadFileCapability, ExtensionCapability, NpmInstallPackageCapability, ProcessExecCapability,
};
use settings::{RegisterSetting, Settings};
use std::sync::Arc;

#[derive(Debug, Default, Clone, RegisterSetting)]
pub struct ExtensionSettings {
    /// The extensions that should be automatically installed by Zed.
    ///
    /// This is used to make functionality provided by extensions (e.g., language support)
    /// available out-of-the-box.
    ///
    /// Default: { "html": true }
    pub auto_install_extensions: HashMap<Arc<str>, bool>,
    pub auto_update_extensions: HashMap<Arc<str>, bool>,
    pub disabled_extensions: HashMap<Arc<str>, bool>,
    pub extension_auto_update: bool,
    pub suggest_extensions: bool,
    pub extension_registries: Vec<Arc<str>>,
    pub extension_settings: HashMap<Arc<str>, serde_json::Value>,
    pub granted_capabilities: Vec<ExtensionCapability>,
}

impl ExtensionSettings {
    /// Returns whether the given extension should be auto-installed.
    pub fn should_auto_install(&self, extension_id: &str) -> bool {
        self.auto_install_extensions
            .get(extension_id)
            .copied()
            .unwrap_or(true)
    }

    /// Whether the user turned the extension off. Disabled extensions stay
    /// installed but are not loaded.
    pub fn is_disabled(&self, extension_id: &str) -> bool {
        self.disabled_extensions
            .get(extension_id)
            .copied()
            .unwrap_or(false)
    }

    pub fn should_auto_update(&self, extension_id: &str) -> bool {
        self.auto_update_extensions
            .get(extension_id)
            .copied()
            .unwrap_or(self.extension_auto_update)
    }
}

impl Settings for ExtensionSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            auto_install_extensions: content.extension.auto_install_extensions.clone(),
            auto_update_extensions: content.extension.auto_update_extensions.clone(),
            disabled_extensions: content.extension.disabled_extensions.clone(),
            extension_auto_update: content.extension.extension_auto_update.unwrap_or(true),
            suggest_extensions: content.extension.suggest_extensions,
            extension_settings: content.project.extension_settings.clone(),
            extension_registries: content
                .extension
                .extension_registries
                .iter()
                .flatten()
                .map(|url| Arc::from(url.trim()))
                .filter(|url: &Arc<str>| !url.is_empty())
                .collect(),
            granted_capabilities: content
                .extension
                .granted_extension_capabilities
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(|capability| match capability {
                    settings::ExtensionCapabilityContent::ProcessExec { command, args } => {
                        ExtensionCapability::ProcessExec(ProcessExecCapability { command, args })
                    }
                    settings::ExtensionCapabilityContent::DownloadFile { host, path } => {
                        ExtensionCapability::DownloadFile(DownloadFileCapability { host, path })
                    }
                    settings::ExtensionCapabilityContent::NpmInstallPackage { package } => {
                        ExtensionCapability::NpmInstallPackage(NpmInstallPackageCapability {
                            package,
                        })
                    }
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auto_update_override() {
        let mut settings = ExtensionSettings {
            extension_auto_update: false,
            ..Default::default()
        };
        settings
            .auto_update_extensions
            .insert("pinned".into(), false);
        settings
            .auto_update_extensions
            .insert("wanted".into(), true);
        assert!(
            !settings.should_auto_update("other"),
            "the global switch applies"
        );
        assert!(
            settings.should_auto_update("wanted"),
            "a per-extension entry wins"
        );

        settings.extension_auto_update = true;
        assert!(settings.should_auto_update("other"));
        assert!(!settings.should_auto_update("pinned"));
    }

    #[test]
    fn test_is_disabled() {
        let mut settings = ExtensionSettings::default();
        settings.disabled_extensions.insert("off".into(), true);
        settings.disabled_extensions.insert("on".into(), false);
        assert!(settings.is_disabled("off"));
        assert!(!settings.is_disabled("on"));
        assert!(!settings.is_disabled("missing"));
    }
}
