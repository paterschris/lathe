use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

use gpui::AsyncApp;
use util::ResultExt as _;

use crate::Client;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CollabAccount {
    pub id: String,
    pub label: String,
    pub user_id: u64,
    pub server_url: String,
    #[serde(default)]
    pub login: Option<String>,
    #[serde(default)]
    pub avatar_uri: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CollabAccountsIndex {
    #[serde(default)]
    pub accounts: Vec<CollabAccount>,
    #[serde(default)]
    pub active_id: Option<String>,
}

impl CollabAccountsIndex {
    pub fn active(&self) -> Option<&CollabAccount> {
        let id = self.active_id.as_ref()?;
        self.accounts.iter().find(|a| &a.id == id)
    }

    pub fn find(&self, id: &str) -> Option<&CollabAccount> {
        self.accounts.iter().find(|a| a.id == id)
    }

    pub fn upsert(&mut self, account: CollabAccount) {
        if let Some(existing) = self.accounts.iter_mut().find(|a| a.id == account.id) {
            *existing = account;
        } else {
            self.accounts.push(account);
        }
    }

    pub fn remove(&mut self, id: &str) -> Option<CollabAccount> {
        let position = self.accounts.iter().position(|a| a.id == id)?;
        let removed = self.accounts.remove(position);
        if self.active_id.as_deref() == Some(id) {
            self.active_id = self.accounts.first().map(|a| a.id.clone());
        }
        Some(removed)
    }
}

fn accounts_path() -> PathBuf {
    paths::config_dir().join("collab_accounts.json")
}

pub fn load_index() -> CollabAccountsIndex {
    std::fs::read(accounts_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save_index(index: &CollabAccountsIndex) -> Result<()> {
    let path = accounts_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let bytes = serde_json::to_vec_pretty(index).context("serialize collab accounts index")?;
    std::fs::write(&path, bytes).context("write collab_accounts.json")
}

/// Updates the label of the active account if it differs from `label`.
/// No-op when there is no active account.
pub fn set_active_label(label: &str) -> Result<()> {
    let mut index = load_index();
    let Some(active_id) = index.active_id.clone() else {
        return Ok(());
    };
    let Some(account) = index.accounts.iter_mut().find(|a| a.id == active_id) else {
        return Ok(());
    };
    if account.label == label {
        return Ok(());
    }
    account.label = label.to_string();
    save_index(&index)
}

/// Caches the active account's login + avatar so other workspaces bound to
/// this account can render it in their title bars even when it isn't the
/// currently-connected account. No-op when there is no active account.
pub fn set_active_user_info(login: &str, avatar_uri: &str) -> Result<()> {
    let mut index = load_index();
    let Some(active_id) = index.active_id.clone() else {
        return Ok(());
    };
    let Some(account) = index.accounts.iter_mut().find(|a| a.id == active_id) else {
        return Ok(());
    };
    let mut changed = false;
    if account.label != login {
        account.label = login.to_string();
        changed = true;
    }
    if account.login.as_deref() != Some(login) {
        account.login = Some(login.to_string());
        changed = true;
    }
    if account.avatar_uri.as_deref() != Some(avatar_uri) {
        account.avatar_uri = Some(avatar_uri.to_string());
        changed = true;
    }
    if !changed {
        return Ok(());
    }
    save_index(&index)
}

pub fn account_id_for(server_url: &str, user_id: u64) -> String {
    let host = url::Url::parse(server_url)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_string()))
        .unwrap_or_else(|| server_url.to_string());
    format!("{host}#{user_id}")
}

pub fn keychain_url(server_url: &str, user_id: u64) -> String {
    format!("{server_url}#user_id={user_id}")
}

/// Multi-account collab session management. Lathe lets a user keep several
/// signed-in collab accounts and switch between them; these methods drive the
/// disconnect/reauth handshake and keep the account index in sync with the
/// active session. Kept here, alongside the account persistence layer, so the
/// multi-account feature stays out of the upstream-owned `client.rs`.
impl Client {
    /// Returns all saved collab accounts.
    pub fn list_accounts(&self) -> Vec<CollabAccount> {
        load_index().accounts
    }

    /// Returns the id of the currently active saved account, if any.
    pub fn active_account_id(&self) -> Option<String> {
        load_index().active_id
    }

    /// Disconnects the current session, marks the given account as active,
    /// and signs back in using that account's stored credentials.
    pub async fn switch_account(self: &Arc<Self>, account_id: String, cx: &AsyncApp) -> Result<()> {
        let mut index = load_index();
        if index.find(&account_id).is_none() {
            anyhow::bail!("unknown collab account: {account_id}");
        }
        if index.active_id.as_deref() == Some(&account_id)
            && !self.status().borrow().is_signed_out()
        {
            return Ok(());
        }

        self.state.write().credentials = None;
        self.cloud_client.clear_credentials();
        self.disconnect(cx);

        index.active_id = Some(account_id);
        save_index(&index).log_err();

        self.sign_in_with_optional_connect(true, cx).await
    }

    /// Starts a new browser sign-in flow to add a second account, disconnecting
    /// the current session first. The new account is stored under its own
    /// keychain entry and becomes the active account on success.
    pub async fn add_account(self: &Arc<Self>, cx: &AsyncApp) -> Result<()> {
        self.state.write().credentials = None;
        self.cloud_client.clear_credentials();
        self.disconnect(cx);

        // Clearing active_id causes read_credentials to return None, which
        // forces a fresh browser auth flow. write_credentials will insert the
        // new account into the index and mark it active.
        let mut index = load_index();
        index.active_id = None;
        save_index(&index).log_err();

        self.sign_in_with_optional_connect(true, cx).await
    }

    /// Deletes the given account's keychain entry and removes it from the
    /// index. If it was the active account, the session is torn down; if other
    /// accounts remain, the first is promoted and signed in.
    pub async fn remove_account(self: &Arc<Self>, account_id: String, cx: &AsyncApp) -> Result<()> {
        let mut index = load_index();
        let Some(account) = index.find(&account_id).cloned() else {
            return Ok(());
        };

        let was_active = index.active_id.as_deref() == Some(&account_id);
        let server_url = self
            .credentials_provider
            .server_url(cx)
            .unwrap_or_else(|_| account.server_url.clone());
        let keychain_url = keychain_url(&server_url, account.user_id);

        if was_active {
            self.state.write().credentials = None;
            self.cloud_client.clear_credentials();
            self.disconnect(cx);
        }

        self.credentials_provider
            .provider
            .delete_credentials(&keychain_url, cx)
            .await
            .log_err();

        index.remove(&account_id);
        save_index(&index).log_err();

        if was_active && !index.accounts.is_empty() {
            self.sign_in_with_optional_connect(true, cx).await?;
        }
        Ok(())
    }
}
