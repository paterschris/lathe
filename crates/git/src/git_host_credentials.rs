use std::sync::Arc;

use anyhow::{Context as _, Result};
use collections::HashMap;
use credentials_provider::CredentialsProvider;
use gpui::{App, AsyncApp, Context, Global, Subscription};

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

/// The keychain key under which a host's credential is stored. Keyed by the
/// host's base URL so the secret sits alongside other per-host credentials.
pub fn host_credential_url(host: &str) -> String {
    format!("https://{host}")
}

struct GlobalGitHostCredentials(Arc<dyn CredentialsProvider>);

impl Global for GlobalGitHostCredentials {}

/// In-memory snapshot of which hosts are currently connected and the username to
/// display for each. Holds NO secrets, only enough for the title-bar menu to
/// render synchronously. The authoritative secret is always read fresh from the
/// keychain when an API call is actually made.
#[derive(Default)]
struct GitHostConnections(HashMap<Arc<str>, String>);

impl Global for GitHostConnections {}

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

/// Reads the stored `(username, secret)` for a host, if one is connected.
pub async fn get(cx: &AsyncApp, host: &str) -> Result<Option<(String, String)>> {
    let provider = provider(cx)?;
    let url = host_credential_url(host);
    let Some((username, secret)) = provider.read_credentials(&url, cx).await? else {
        return Ok(None);
    };
    let secret = String::from_utf8(secret).context("stored credential was not valid UTF-8")?;
    Ok(Some((username, secret)))
}

/// Reads the stored credential for a host and converts it into a ready-to-use
/// [`GitHostAuth`]. Returns `None` when the host has no registered provider with
/// an auth flow, or when nothing is connected for it.
pub async fn auth_for_host(cx: &AsyncApp, host: &str) -> Result<Option<GitHostAuth>> {
    let Some(git_host) = GitHost::resolve_async(cx, host) else {
        return Ok(None);
    };
    Ok(get(cx, host)
        .await?
        .map(|(username, secret)| git_host.auth(username, secret)))
}

/// Stores a host credential, then refreshes the connection snapshot.
pub async fn set(cx: &AsyncApp, host: &str, username: &str, secret: &str) -> Result<()> {
    let provider = provider(cx)?;
    let url = host_credential_url(host);
    provider
        .write_credentials(&url, username, secret.as_bytes(), cx)
        .await?;
    cx.update(refresh_connections);
    Ok(())
}

/// Clears a host credential, then refreshes the connection snapshot.
pub async fn clear(cx: &AsyncApp, host: &str) -> Result<()> {
    let provider = provider(cx)?;
    let url = host_credential_url(host);
    provider.delete_credentials(&url, cx).await?;
    cx.update(refresh_connections);
    Ok(())
}

/// Returns the connected username for `host` from the in-memory snapshot, for
/// synchronous use during rendering. May briefly lag a connect/disconnect until
/// the async refresh completes.
pub fn connected_username(cx: &App, host: &str) -> Option<String> {
    cx.try_global::<GitHostConnections>()
        .and_then(|connections| connections.0.get(host).cloned())
}

/// Re-reads every connectable host's credential from the keychain, updates the
/// in-memory snapshot, and refreshes open windows so menus reflect the change.
pub fn refresh_connections(cx: &mut App) {
    // Resolve the host set synchronously: the registry lives behind a global
    // that the spawned task cannot borrow across its await points.
    let hosts: Vec<Arc<str>> = connectable_hosts(cx)
        .into_iter()
        .map(|host| host.host)
        .collect();
    cx.spawn(async move |cx| {
        let mut connections = HashMap::default();
        for host in hosts {
            if let Ok(Some((username, _secret))) = get(cx, &host).await {
                connections.insert(host, username);
            }
        }
        cx.update(|cx| {
            cx.set_global(GitHostConnections(connections));
            cx.refresh_windows();
        })
    })
    .detach();
}

/// Registers `on_change` to run whenever the set of connected git hosts changes
/// (a host is connected or disconnected, which goes through `refresh_connections`
/// after [`set`] / [`clear`]). Pull-request views use this to reload after the
/// user reconnects an expired account. The returned [`Subscription`] must be
/// retained for the callback to stay active.
pub fn observe_connections<T: 'static>(
    cx: &mut Context<T>,
    on_change: impl FnMut(&mut T, &mut Context<T>) + 'static,
) -> Subscription {
    cx.observe_global::<GitHostConnections>(on_change)
}
