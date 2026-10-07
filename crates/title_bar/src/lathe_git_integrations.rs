use gpui::Action;
use ui::{ContextMenu, IconPosition};

/// One connectable git host and its connected accounts, resolved by the caller
/// from the hosting-provider registry and the account store.
pub struct GitHostMenuEntry {
    pub host: String,
    /// Name to show in the menu, e.g. "GitHub" or "BigCorp GitHub (git.corp.com)".
    pub display_name: String,
    /// Every connected account, as `(id, login)`.
    pub accounts: Vec<(String, String)>,
    /// The account this workspace uses, when any is connected.
    pub active_account_id: Option<String>,
}

/// Appends an entry for every host Lathe can authenticate against. The set is
/// dynamic rather than a fixed GitHub/Bitbucket pair, so enterprise and
/// self-hosted instances appear here as soon as their provider is registered.
pub fn append_git_integrations(menu: ContextMenu, entries: Vec<GitHostMenuEntry>) -> ContextMenu {
    entries.into_iter().fold(menu, append_host)
}

fn account_label(login: &str) -> String {
    if login.is_empty() {
        "Unnamed account".to_string()
    } else {
        login.to_string()
    }
}

/// A host with nothing connected gets a single connect entry. A connected host
/// gets a submenu listing its accounts, where picking one switches only this
/// workspace.
fn append_host(menu: ContextMenu, entry: GitHostMenuEntry) -> ContextMenu {
    let GitHostMenuEntry {
        host,
        display_name,
        accounts,
        active_account_id,
    } = entry;
    if accounts.is_empty() {
        return menu.action(
            format!("Connect {display_name}…"),
            zed_actions::ConnectGitHost { host }.boxed_clone(),
        );
    }

    let active_login = active_account_id
        .as_ref()
        .and_then(|active| accounts.iter().find(|(id, _)| id == active))
        .map(|(_, login)| account_label(login));
    let label = match active_login {
        Some(login) => format!("{display_name}: {login}"),
        None => display_name,
    };
    menu.submenu(label, move |mut submenu, _window, _cx| {
        for (account_id, login) in &accounts {
            let is_active = active_account_id.as_deref() == Some(account_id.as_str());
            let action = zed_actions::SwitchGitHostAccount {
                host: host.clone(),
                account_id: account_id.clone(),
            };
            submenu = submenu.toggleable_entry(
                account_label(login),
                is_active,
                IconPosition::Start,
                None,
                move |window, cx| window.dispatch_action(action.boxed_clone(), cx),
            );
        }
        submenu = submenu.separator().action(
            "Add Account…",
            zed_actions::ConnectGitHost { host: host.clone() }.boxed_clone(),
        );
        if let Some(active_id) = active_account_id.clone() {
            let active_login = accounts
                .iter()
                .find(|(id, _)| *id == active_id)
                .map(|(_, login)| account_label(login))
                .unwrap_or_default();
            submenu = submenu.action(
                format!("Disconnect {active_login}"),
                zed_actions::DisconnectGitHost {
                    host: host.clone(),
                    account_id: Some(active_id),
                }
                .boxed_clone(),
            );
        }
        submenu
    })
}
