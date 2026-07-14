use std::sync::Arc;

use anyhow::{Context as _, Result};
use collections::HashMap;
use credentials_provider::CredentialsProvider;
use gpui::{App, AsyncApp, Context, Global, Subscription};

use crate::hosting_provider::GitHostAuth;

/// A git hosting provider that Lathe can authenticate against for pull-request
/// operations. Each kind maps to a single canonical host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitHostKind {
    GitHub,
    BitbucketCloud,
}

impl GitHostKind {
    pub const ALL: [GitHostKind; 2] = [GitHostKind::GitHub, GitHostKind::BitbucketCloud];

    /// The canonical remote host this kind authenticates against.
    pub fn host(self) -> &'static str {
        match self {
            GitHostKind::GitHub => "github.com",
            GitHostKind::BitbucketCloud => "bitbucket.org",
        }
    }

    /// Human-readable name for menus and toasts.
    pub fn display_name(self) -> &'static str {
        match self {
            GitHostKind::GitHub => "GitHub",
            GitHostKind::BitbucketCloud => "Bitbucket Cloud",
        }
    }

    pub fn from_host(host: &str) -> Option<GitHostKind> {
        GitHostKind::ALL
            .into_iter()
            .find(|kind| kind.host() == host)
    }

    /// Builds the API auth value for a stored `(username, secret)` credential.
    /// GitHub uses bearer tokens; Bitbucket Cloud uses HTTP Basic.
    pub fn auth(self, username: String, secret: String) -> GitHostAuth {
        match self {
            GitHostKind::GitHub => GitHostAuth::Bearer(secret),
            GitHostKind::BitbucketCloud => GitHostAuth::Basic { username, secret },
        }
    }
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
struct GitHostConnections(HashMap<&'static str, String>);

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
/// [`GitHostAuth`]. Returns `None` when the host is unknown or not connected.
pub async fn auth_for_host(cx: &AsyncApp, host: &str) -> Result<Option<GitHostAuth>> {
    let Some(kind) = GitHostKind::from_host(host) else {
        return Ok(None);
    };
    Ok(get(cx, host)
        .await?
        .map(|(username, secret)| kind.auth(username, secret)))
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

/// Re-reads every known host's credential from the keychain, updates the
/// in-memory snapshot, and refreshes open windows so menus reflect the change.
pub fn refresh_connections(cx: &mut App) {
    cx.spawn(async move |cx| {
        let mut connections = HashMap::default();
        for kind in GitHostKind::ALL {
            if let Ok(Some((username, _secret))) = get(cx, kind.host()).await {
                connections.insert(kind.host(), username);
            }
        }
        cx.update(|cx| {
            cx.set_global(GitHostConnections(connections));
            cx.refresh_windows();
        });
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
