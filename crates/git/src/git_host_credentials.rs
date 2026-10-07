use std::{path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result};
use collections::HashMap;
use credentials_provider::CredentialsProvider;
use gpui::{
    App, AppContext as _, AsyncApp, BorrowAppContext as _, Context, EntityId, Global, Subscription,
};
use serde::{Deserialize, Serialize};
use util::ResultExt as _;

use crate::hosting_provider::{GitHostAuth, GitHostAuthKind, GitHostingProviderRegistry};

/// A specific git host Lathe can authenticate against for pull-request
/// operations: the protocol it speaks plus the hostname it lives at.
///
/// Resolved from the hosting-provider registry rather than hardcoded, so
/// enterprise and self-hosted instances are first-class. Their hostnames are
/// only known at runtime (from the `git_hosting_providers` setting, or inferred
/// from the repository's own remote), which a fixed enum could never express.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHost {
    kind: GitHostAuthKind,
    host: Arc<str>,
    /// The provider's configured name, e.g. "GitHub" or "BigCorp GitHub".
    display_name: Arc<str>,
}

impl GitHost {
    pub fn kind(&self) -> GitHostAuthKind {
        self.kind
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// Name to show in menus and modals. Falls back to the hostname when a
    /// self-hosted provider was configured without a distinguishing name, so
    /// two GitHub entries never render identically.
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// Whether this is the vendor's public instance rather than an enterprise
    /// or self-hosted deployment. Drives the connect flow: only the public
    /// GitHub can use the device flow, because the OAuth app backing it is
    /// registered against `github.com`.
    pub fn is_public_instance(&self) -> bool {
        &*self.host == self.kind.public_host()
    }

    /// Builds the API auth value for a stored `(username, secret)` credential.
    pub fn auth(&self, username: String, secret: String) -> GitHostAuth {
        self.kind.auth(username, secret)
    }

    /// Resolves a hostname to a connectable host by asking the provider registry
    /// which provider serves it. Returns `None` for hosts with no registered
    /// provider, or whose provider has no authentication flow.
    pub fn resolve(cx: &App, host: &str) -> Option<GitHost> {
        connectable_hosts(cx)
            .into_iter()
            .find(|candidate| &*candidate.host == host)
    }

    /// Async-context counterpart of [`GitHost::resolve`].
    pub fn resolve_async(cx: &AsyncApp, host: &str) -> Option<GitHost> {
        cx.update(|cx| GitHost::resolve(cx, host))
    }
}

/// Every host in the registry that Lathe knows how to authenticate against,
/// deduplicated by hostname with the vendor's public instances listed first.
///
/// The set is dynamic: registering a self-hosted provider (through settings or
/// from a repository remote) makes that instance connectable without any change
/// here.
pub fn connectable_hosts(cx: &App) -> Vec<GitHost> {
    let Some(registry) = GitHostingProviderRegistry::try_global(cx) else {
        return Vec::new();
    };
    let mut hosts: Vec<GitHost> = Vec::new();
    for provider in registry.list_hosting_providers() {
        let Some(kind) = provider.auth_kind() else {
            continue;
        };
        let Some(host) = provider.base_url().host_str().map(Arc::<str>::from) else {
            continue;
        };
        if hosts.iter().any(|existing| existing.host == host) {
            continue;
        }
        let name = provider.name();
        // A self-hosted provider configured without a distinguishing name would
        // otherwise render as a second, identical "GitHub" menu entry.
        let display_name: Arc<str> = if &*host == kind.public_host() {
            Arc::from(name.as_str())
        } else {
            Arc::from(format!("{name} ({host})").as_str())
        };
        hosts.push(GitHost {
            kind,
            host,
            display_name,
        });
    }
    hosts.sort_by_key(|host| !host.is_public_instance());
    hosts
}

/// The keychain key a host's original, single credential is stored under.
/// Still used for the account that existing connection becomes, so upgrading
/// does not ask anyone to sign in again.
pub fn host_credential_url(host: &str) -> String {
    format!("https://{host}")
}

/// The id given to a connection made before a host could hold several
/// accounts. Its secret stays under [`host_credential_url`].
const LEGACY_ACCOUNT_ID: &str = "default";

fn account_credential_url(host: &str, account_id: &str) -> String {
    if account_id == LEGACY_ACCOUNT_ID {
        host_credential_url(host)
    } else {
        format!("https://{host}/lathe-git-account/{account_id}")
    }
}

/// One connected account on a git host. Holds no secret: that lives in the
/// keychain and is read only when an API call is made.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHostAccount {
    pub id: String,
    /// The account's login, shown in menus. Empty when the host couldn't
    /// report it at connect time.
    pub username: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct HostAccounts {
    accounts: Vec<GitHostAccount>,
    /// Used by workspaces that haven't picked an account of their own: the
    /// account connected or switched to most recently.
    default: Option<String>,
}

impl HostAccounts {
    fn find(&self, account_id: &str) -> Option<&GitHostAccount> {
        self.accounts
            .iter()
            .find(|account| account.id == account_id)
    }

    fn default_account(&self) -> Option<&GitHostAccount> {
        self.default
            .as_deref()
            .and_then(|id| self.find(id))
            .or_else(|| self.accounts.first())
    }
}

/// Which accounts each host has. Shared by every window and every running
/// instance, since it is just a list of what's in the keychain; which one a
/// workspace uses is tracked separately, per workspace.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct AccountIndex {
    #[serde(default)]
    hosts: HashMap<String, HostAccounts>,
}

fn index_path() -> PathBuf {
    paths::config_dir().join("git_host_accounts.json")
}

fn load_index() -> AccountIndex {
    match std::fs::read(index_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).log_err().unwrap_or_default(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => AccountIndex::default(),
        Err(error) => {
            log::error!("git host accounts: failed to read index: {error}");
            AccountIndex::default()
        }
    }
}

fn save_index(index: &AccountIndex) -> Result<()> {
    let path = index_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(index)?)
        .with_context(|| format!("writing {}", path.display()))
}

struct GlobalGitHostCredentials(Arc<dyn CredentialsProvider>);

impl Global for GlobalGitHostCredentials {}

/// In-memory copy of the account index, for synchronous use while rendering.
/// Holds NO secrets.
#[derive(Default)]
struct GitHostConnections(AccountIndex);

impl Global for GitHostConnections {}

/// The account each workspace picked for each host, keyed by the workspace
/// entity and then by hostname. A workspace with no entry for a host uses the
/// host's default account.
#[derive(Default)]
pub struct WorkspaceGitHostAccounts(HashMap<EntityId, HashMap<String, String>>);

impl Global for WorkspaceGitHostAccounts {}

impl WorkspaceGitHostAccounts {
    pub fn bindings(workspace: EntityId, cx: &App) -> HashMap<String, String> {
        cx.try_global::<Self>()
            .and_then(|this| this.0.get(&workspace).cloned())
            .unwrap_or_default()
    }

    pub fn set_all(workspace: EntityId, bindings: HashMap<String, String>, cx: &mut App) {
        cx.default_global::<Self>().0.insert(workspace, bindings);
    }

    /// Binds (`Some`) or unbinds (`None`) the workspace's account for `host`
    /// and returns the workspace's full binding map, for persisting. Views
    /// observing connections reload, since the account behind their requests
    /// may have changed.
    pub fn bind(
        workspace: EntityId,
        host: &str,
        account_id: Option<String>,
        cx: &mut App,
    ) -> HashMap<String, String> {
        let bindings = cx.default_global::<Self>().0.entry(workspace).or_default();
        match account_id {
            Some(account_id) => {
                bindings.insert(host.to_string(), account_id);
            }
            None => {
                bindings.remove(host);
            }
        }
        let bindings = bindings.clone();
        cx.update_default_global::<GitHostConnections, _>(|_, _| {});
        cx.refresh_windows();
        bindings
    }
}

/// Installs the credentials provider used for git-host secrets and kicks off an
/// initial refresh of the connection snapshot. Call once at startup.
pub fn init(provider: Arc<dyn CredentialsProvider>, cx: &mut App) {
    cx.set_global(GlobalGitHostCredentials(provider));
    refresh_connections(cx);
}

fn provider(cx: &AsyncApp) -> Result<Arc<dyn CredentialsProvider>> {
    cx.try_read_global::<GlobalGitHostCredentials, _>(|global, _| global.0.clone())
        .context("git host credentials store is not initialized")
}

async fn read_credential(cx: &AsyncApp, url: &str) -> Result<Option<(String, String)>> {
    let provider = provider(cx)?;
    let Some((username, secret)) = provider.read_credentials(url, cx).await? else {
        return Ok(None);
    };
    let secret = String::from_utf8(secret).context("stored credential was not valid UTF-8")?;
    Ok(Some((username, secret)))
}

/// Every account connected for `host`, in the order they were added.
pub fn accounts(cx: &App, host: &str) -> Vec<GitHostAccount> {
    cx.try_global::<GitHostConnections>()
        .and_then(|connections| connections.0.hosts.get(host))
        .map(|host_accounts| host_accounts.accounts.clone())
        .unwrap_or_default()
}

/// The account `workspace` uses for `host`: the workspace's own pick when it
/// still exists, otherwise the host's default. `None` when nothing is
/// connected.
pub fn active_account(cx: &App, workspace: Option<EntityId>, host: &str) -> Option<GitHostAccount> {
    let host_accounts = cx.try_global::<GitHostConnections>()?.0.hosts.get(host)?;
    workspace
        .and_then(|workspace| {
            let bound_id = cx
                .try_global::<WorkspaceGitHostAccounts>()?
                .0
                .get(&workspace)?
                .get(host)?;
            host_accounts.find(bound_id)
        })
        .or_else(|| host_accounts.default_account())
        .cloned()
}

/// The login of the account `workspace` uses for `host`, for synchronous use
/// during rendering. May briefly lag a connect/disconnect until the async
/// refresh completes.
pub fn connected_username(cx: &App, workspace: Option<EntityId>, host: &str) -> Option<String> {
    active_account(cx, workspace, host).map(|account| account.username)
}

/// Reads the credential of the account `workspace` uses for `host` and
/// converts it into a ready-to-use [`GitHostAuth`]. Returns `None` when the
/// host has no registered provider with an auth flow, or when nothing is
/// connected for it.
pub async fn auth_for_host(
    cx: &AsyncApp,
    workspace: Option<EntityId>,
    host: &str,
) -> Result<Option<GitHostAuth>> {
    let Some(git_host) = GitHost::resolve_async(cx, host) else {
        return Ok(None);
    };
    let Some(account) = cx.update(|cx| active_account(cx, workspace, host)) else {
        return Ok(None);
    };
    Ok(
        read_credential(cx, &account_credential_url(host, &account.id))
            .await?
            .map(|(username, secret)| git_host.auth(username, secret)),
    )
}

/// Stores a credential as an account on `host`, makes it the host's default,
/// and returns its id. Reconnecting an account that is already there (same
/// non-empty username) replaces its secret rather than adding a duplicate.
pub async fn add_account(
    cx: &AsyncApp,
    host: &str,
    username: &str,
    secret: &str,
) -> Result<String> {
    let provider = provider(cx)?;
    let mut index = cx.background_spawn(async { load_index() }).await;
    let host_accounts = index.hosts.entry(host.to_string()).or_default();
    let existing = (!username.is_empty())
        .then(|| {
            host_accounts
                .accounts
                .iter()
                .find(|account| account.username == username)
        })
        .flatten()
        .map(|account| account.id.clone());
    let account_id = existing.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    provider
        .write_credentials(
            &account_credential_url(host, &account_id),
            username,
            secret.as_bytes(),
            cx,
        )
        .await?;
    if host_accounts.find(&account_id).is_none() {
        host_accounts.accounts.push(GitHostAccount {
            id: account_id.clone(),
            username: username.to_string(),
        });
    }
    host_accounts.default = Some(account_id.clone());
    cx.background_spawn(async move { save_index(&index) })
        .await?;
    cx.update(refresh_connections);
    Ok(account_id)
}

/// Deletes one account's credential and drops it from the host's list.
/// Workspaces that were using it fall back to the host's default.
pub async fn remove_account(cx: &AsyncApp, host: &str, account_id: &str) -> Result<()> {
    let provider = provider(cx)?;
    provider
        .delete_credentials(&account_credential_url(host, account_id), cx)
        .await?;
    let mut index = cx.background_spawn(async { load_index() }).await;
    if let Some(host_accounts) = index.hosts.get_mut(host) {
        host_accounts
            .accounts
            .retain(|account| account.id != account_id);
        if host_accounts.default.as_deref() == Some(account_id) {
            host_accounts.default = host_accounts
                .accounts
                .first()
                .map(|account| account.id.clone());
        }
        if host_accounts.accounts.is_empty() {
            index.hosts.remove(host);
        }
    }
    cx.background_spawn(async move { save_index(&index) })
        .await?;
    cx.update(refresh_connections);
    Ok(())
}

/// Re-reads the account index, adopts any connection made before hosts could
/// hold several accounts, updates the in-memory snapshot, and refreshes open
/// windows so menus reflect the change.
pub fn refresh_connections(cx: &mut App) {
    // Resolved synchronously: the registry lives behind a global that the
    // spawned task cannot borrow across its await points.
    let hosts: Vec<Arc<str>> = connectable_hosts(cx)
        .into_iter()
        .map(|host| host.host)
        .collect();
    cx.spawn(async move |cx| {
        let mut index = cx.background_spawn(async { load_index() }).await;
        let mut adopted = false;
        for host in hosts {
            if index.hosts.contains_key(&*host) {
                continue;
            }
            if let Ok(Some((username, _secret))) =
                read_credential(cx, &host_credential_url(&host)).await
            {
                index.hosts.insert(
                    host.to_string(),
                    HostAccounts {
                        accounts: vec![GitHostAccount {
                            id: LEGACY_ACCOUNT_ID.to_string(),
                            username,
                        }],
                        default: Some(LEGACY_ACCOUNT_ID.to_string()),
                    },
                );
                adopted = true;
            }
        }
        if adopted {
            let snapshot = index.clone();
            cx.background_spawn(async move { save_index(&snapshot) })
                .await
                .log_err();
        }
        cx.update(|cx| {
            cx.set_global(GitHostConnections(index));
            cx.refresh_windows();
        })
    })
    .detach();
}

/// Registers `on_change` to run whenever the connected accounts change, or a
/// workspace switches which account it uses. Pull-request views use this to
/// reload with the right credentials. The returned [`Subscription`] must be
/// retained for the callback to stay active.
pub fn observe_connections<T: 'static>(
    cx: &mut Context<T>,
    on_change: impl FnMut(&mut T, &mut Context<T>) + 'static,
) -> Subscription {
    cx.observe_global::<GitHostConnections>(on_change)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str) -> GitHostAccount {
        GitHostAccount {
            id: id.to_string(),
            username: format!("user-{id}"),
        }
    }

    #[test]
    fn default_account_falls_back_to_first_when_unset_or_stale() {
        let mut host_accounts = HostAccounts {
            accounts: vec![account("a"), account("b")],
            default: Some("b".to_string()),
        };
        assert_eq!(host_accounts.default_account().unwrap().id, "b");
        host_accounts.default = Some("gone".to_string());
        assert_eq!(host_accounts.default_account().unwrap().id, "a");
        host_accounts.default = None;
        assert_eq!(host_accounts.default_account().unwrap().id, "a");
    }

    #[test]
    fn legacy_account_keeps_its_original_keychain_entry() {
        assert_eq!(
            account_credential_url("github.com", LEGACY_ACCOUNT_ID),
            "https://github.com"
        );
        assert_eq!(
            account_credential_url("github.com", "abc"),
            "https://github.com/lathe-git-account/abc"
        );
    }

    #[gpui::test]
    fn workspaces_resolve_their_own_account(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut index = AccountIndex::default();
            index.hosts.insert(
                "github.com".to_string(),
                HostAccounts {
                    accounts: vec![account("work"), account("personal")],
                    default: Some("work".to_string()),
                },
            );
            cx.set_global(GitHostConnections(index));

            let first = EntityId::from(1u64);
            let second = EntityId::from(2u64);
            WorkspaceGitHostAccounts::bind(first, "github.com", Some("personal".into()), cx);

            let active = |workspace, cx: &App| {
                active_account(cx, Some(workspace), "github.com").map(|account| account.id)
            };
            assert_eq!(active(first, cx).as_deref(), Some("personal"));
            assert_eq!(active(second, cx).as_deref(), Some("work"));

            // A pick whose account was removed falls back to the default.
            WorkspaceGitHostAccounts::bind(second, "github.com", Some("deleted".into()), cx);
            assert_eq!(active(second, cx).as_deref(), Some("work"));
            assert!(active_account(cx, Some(first), "gitlab.com").is_none());
        });
    }
}
