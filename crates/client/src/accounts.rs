use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CollabAccount {
    pub id: String,
    pub label: String,
    pub user_id: u64,
    pub server_url: String,
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
