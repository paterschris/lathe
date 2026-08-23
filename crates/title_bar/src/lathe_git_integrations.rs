use gpui::Action;
use ui::ContextMenu;

/// One connectable git host and the account currently connected to it, resolved
/// by the caller from the hosting-provider registry.
pub struct GitHostMenuEntry {
    pub host: String,
    /// Name to show in the menu, e.g. "GitHub" or "BigCorp GitHub (git.corp.com)".
    pub display_name: String,
    /// The connected account's login, or `None` when nothing is connected.
    pub connected_login: Option<String>,
}

/// Appends a connect/disconnect entry for every host Lathe can authenticate
/// against. The set is dynamic rather than a fixed GitHub/Bitbucket pair, so
/// enterprise and self-hosted instances appear here as soon as their provider
/// is registered.
pub fn append_git_integrations(menu: ContextMenu, entries: Vec<GitHostMenuEntry>) -> ContextMenu {
    entries.into_iter().fold(menu, append_host_action)
}

fn append_host_action(menu: ContextMenu, entry: GitHostMenuEntry) -> ContextMenu {
    let GitHostMenuEntry {
        host,
        display_name,
        connected_login,
    } = entry;
    match connected_login {
        Some(login) => menu.action(
            format!("Disconnect {display_name} ({login})"),
            zed_actions::DisconnectGitHost { host }.boxed_clone(),
        ),
        None => menu.action(
            format!("Connect {display_name}…"),
            zed_actions::ConnectGitHost { host }.boxed_clone(),
        ),
    }
}
