//! Offers to install the extensions a project recommends in its
//! `.zed/settings.json` (`"recommended_extensions": ["id", ...]`).

use std::sync::Arc;

use db::kvp::KeyValueStore;
use extension_host::ExtensionStore;
use extension_host::extension_settings::ExtensionSettings;
use gpui::{App, AppContext as _, Context, SharedString};
use settings::{RegisterSetting, Settings, SettingsLocation, SettingsStore};
use util::ResultExt as _;
use util::rel_path::RelPath;
use workspace::Workspace;
use workspace::notifications::{NotificationId, simple_message_notification::MessageNotification};

#[derive(Clone, Debug, Default, RegisterSetting)]
pub struct RecommendedExtensionsSettings {
    pub recommended_extensions: Vec<Arc<str>>,
}

impl Settings for RecommendedExtensionsSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            recommended_extensions: content
                .project
                .recommended_extensions
                .clone()
                .unwrap_or_default(),
        }
    }
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        if window.is_none() {
            return;
        }
        // Project settings arrive after the workspace opens, and for an
        // untrusted project only once it's trusted, so check whenever any
        // settings change rather than once.
        cx.observe_global::<SettingsStore>(|workspace, cx| {
            offer_recommended_extensions(workspace, cx)
        })
        .detach();
        offer_recommended_extensions(workspace, cx);
    })
    .detach();
}

struct RecommendedExtensionsNotification;

pub(crate) fn dismissed_key(worktree_root: &str) -> String {
    format!("{worktree_root}_recommended_extensions_dismissed")
}

pub(crate) fn notification_id(worktree_root: &str) -> NotificationId {
    NotificationId::composite::<RecommendedExtensionsNotification>(SharedString::from(
        worktree_root.to_string(),
    ))
}

/// Remembers that the user declined exactly this list for this project.
pub(crate) fn dismiss_recommendations(key: String, fingerprint: String, cx: &mut App) {
    let kvp = KeyValueStore::global(cx);
    db::write_and_log(
        cx,
        move || async move { kvp.write_kvp(key, fingerprint).await },
    );
}

/// The recommended extensions a project lists that aren't installed (or being
/// installed). Disabled extensions count as installed.
pub(crate) fn missing_recommendations(
    recommended: &[Arc<str>],
    store: &ExtensionStore,
) -> Vec<Arc<str>> {
    let mut missing = recommended
        .iter()
        .map(|id| Arc::<str>::from(id.trim()))
        .filter(|id| !id.is_empty())
        .filter(|id| {
            !store.installed_extensions().contains_key(id)
                && !store.outstanding_operations().contains_key(id)
        })
        .collect::<Vec<_>>();
    missing.sort();
    missing.dedup();
    missing
}

fn offer_recommended_extensions(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    if !ExtensionSettings::get_global(cx).suggest_extensions {
        return;
    }
    let Some(store) = ExtensionStore::try_global(cx) else {
        return;
    };
    let worktrees = workspace
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| {
            let worktree = worktree.read(cx);
            (worktree.id(), worktree.abs_path())
        })
        .collect::<Vec<_>>();

    for (worktree_id, root) in worktrees {
        let recommended = RecommendedExtensionsSettings::get(
            Some(SettingsLocation {
                worktree_id,
                path: RelPath::empty(),
            }),
            cx,
        )
        .recommended_extensions
        .clone();
        let missing = missing_recommendations(&recommended, store.read(cx));
        if missing.is_empty() {
            continue;
        }

        let root = root.to_string_lossy().into_owned();
        let notification_id = notification_id(&root);
        if workspace.has_notification(&notification_id) {
            continue;
        }
        // Dismissing remembers the exact list, so a changed list asks again.
        let fingerprint = missing.join(",");
        let key = dismissed_key(&root);
        if KeyValueStore::global(cx).read_kvp(&key).log_err().flatten() == Some(fingerprint.clone())
        {
            continue;
        }

        let project_name = std::path::Path::new(&root)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.clone());
        let listed = missing
            .iter()
            .map(|id| format!("`{id}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let message = if missing.len() == 1 {
            format!("{project_name} recommends an extension you don't have: {listed}")
        } else {
            format!(
                "{project_name} recommends {} extensions you don't have: {listed}",
                missing.len()
            )
        };

        workspace.show_notification(notification_id, cx, move |cx| {
            cx.new(move |cx| {
                MessageNotification::new(message, cx)
                    .primary_message(if missing.len() == 1 {
                        "Install"
                    } else {
                        "Install All"
                    })
                    .primary_on_click({
                        let missing = missing.clone();
                        move |_window, cx| {
                            ExtensionStore::global(cx).update(cx, |store, cx| {
                                for extension_id in &missing {
                                    store.install_latest_extension(extension_id.clone(), cx);
                                }
                            });
                        }
                    })
                    .secondary_message("Don't Ask Again")
                    .secondary_on_click(move |_window, cx| {
                        dismiss_recommendations(key.clone(), fingerprint.clone(), cx)
                    })
            })
        });
    }
}
