use client::accounts::CollabAccount;
use gpui::{Action, SharedString, WeakEntity};
use ui::{ContextMenu, Icon, IconName, Label, prelude::*};
use util::ResultExt;
use workspace::Workspace;

pub(crate) fn append_accounts_menu(
    menu: ContextMenu,
    is_signed_in: bool,
    saved_accounts: Vec<CollabAccount>,
    active_account_id: Option<String>,
    display_account_id: Option<String>,
    user_login: Option<SharedString>,
    workspace: WeakEntity<Workspace>,
) -> ContextMenu {
    menu.when(is_signed_in || !saved_accounts.is_empty(), |this| {
        let mut this = this.separator().header("Accounts");
        for account in &saved_accounts {
            let account_id = account.id.clone();
            let is_active = display_account_id.as_deref() == Some(&account.id);
            let is_globally_active = active_account_id.as_deref() == Some(&account.id);
            let label = if is_globally_active {
                user_login
                    .as_ref()
                    .map(|login| login.to_string())
                    .unwrap_or_else(|| {
                        account
                            .login
                            .clone()
                            .unwrap_or_else(|| account.label.clone())
                    })
            } else {
                account
                    .login
                    .clone()
                    .unwrap_or_else(|| account.label.clone())
            };
            if is_globally_active && user_login.is_some() {
                client::accounts::set_active_label(&label).log_err();
            }
            this = this.custom_entry(
                move |_window, _cx| {
                    let mut row = h_flex()
                        .w_full()
                        .justify_between()
                        .child(Label::new(label.clone()));
                    if is_active {
                        row = row.child(Icon::new(IconName::Check).color(Color::Accent));
                    }
                    row.into_any_element()
                },
                {
                    let account_id = account_id.clone();
                    let workspace = workspace.clone();
                    move |window, cx| {
                        if let Some(workspace) = workspace.upgrade() {
                            workspace.update(cx, |workspace, cx| {
                                workspace.set_bound_collab_account_id(Some(account_id.clone()), cx);
                            });
                        }
                        window.dispatch_action(
                            client::SwitchAccount {
                                account_id: account_id.clone(),
                            }
                            .boxed_clone(),
                            cx,
                        );
                    }
                },
            );
        }
        this.action("Add Account…", client::AddAccount.boxed_clone())
            .when(is_signed_in, |this| {
                let active_label = active_account_id.as_ref().and_then(|id| {
                    saved_accounts
                        .iter()
                        .find(|account| &account.id == id)
                        .map(|account| account.label.clone())
                });
                let sign_out_label = match active_label {
                    Some(label) => format!("Sign Out of {label}"),
                    None => "Sign Out".to_string(),
                };
                this.action(sign_out_label, client::SignOut.boxed_clone())
            })
    })
}
