use std::sync::Arc;

use collections::HashMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};
use util::serde::default_true;

#[with_fallible_options]
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ExtensionSettingsContent {
    /// The extensions that should be automatically installed by Zed.
    ///
    /// This is used to make functionality provided by extensions (e.g., language support)
    /// available out-of-the-box.
    ///
    /// Default: { "html": true }
    #[serde(default)]
    pub auto_install_extensions: HashMap<Arc<str>, bool>,
    #[serde(default)]
    pub auto_update_extensions: HashMap<Arc<str>, bool>,
    /// Whether installed extensions update automatically. An extension's
    /// entry in `auto_update_extensions` overrides this.
    ///
    /// Default: true
    pub extension_auto_update: Option<bool>,
    /// Extensions that stay installed but are not loaded, keyed by extension
    /// id. Set an extension to `true` to turn it off and `false` (or remove
    /// it) to turn it back on.
    ///
    /// Default: {}
    #[serde(default)]
    pub disabled_extensions: HashMap<Arc<str>, bool>,
    /// Whether to suggest installing extensions based on the files you open.
    ///
    /// Default: true
    #[serde(default = "default_true")]
    pub suggest_extensions: bool,
    /// Additional extension registries, given as URLs of static `index.json` files.
    ///
    /// Default: ["https://paterschris.github.io/lathe-extensions/index.json"]
    pub extension_registries: Option<Vec<String>>,
    /// The capabilities granted to extensions.
    pub granted_extension_capabilities: Option<Vec<ExtensionCapabilityContent>>,
}

impl Default for ExtensionSettingsContent {
    fn default() -> Self {
        Self {
            auto_install_extensions: HashMap::default(),
            auto_update_extensions: HashMap::default(),
            disabled_extensions: HashMap::default(),
            extension_auto_update: None,
            suggest_extensions: true,
            extension_registries: None,
            granted_extension_capabilities: None,
        }
    }
}

/// A capability for an extension.
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExtensionCapabilityContent {
    #[serde(rename = "process:exec")]
    ProcessExec {
        /// The command to execute.
        command: String,
        /// The arguments to pass to the command. Use `*` for a single wildcard argument.
        /// If the last element is `**`, then any trailing arguments are allowed.
        args: Vec<String>,
    },
    DownloadFile {
        host: String,
        path: Vec<String>,
    },
    #[serde(rename = "npm:install")]
    NpmInstallPackage {
        package: String,
    },
}
