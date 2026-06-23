use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _, Request};
use serde::Deserialize;
use urlencoding::encode;

/// OAuth client id for the registered "Lathe" GitHub app used by the device
/// flow. This is a public identifier; the device flow ships no client secret.
/// Override at runtime with `LATHE_GITHUB_CLIENT_ID` to test against a different
/// app without recompiling.
const DEFAULT_GITHUB_CLIENT_ID: &str = "Ov23liVASWEz9GsjMtf2";

/// Scopes requested for pull-request operations: full repo access (to read and
/// act on PRs in private repositories) plus read access to org membership.
const GITHUB_SCOPE: &str = "repo read:org";

pub fn github_client_id() -> String {
    std::env::var("LATHE_GITHUB_CLIENT_ID")
        .ok()
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| DEFAULT_GITHUB_CLIENT_ID.to_string())
}

/// Whether a usable client id is configured. An empty id would make the device
/// flow hit GitHub with no client and get an opaque error, so callers surface a
/// clear message instead.
fn client_id_is_configured(client_id: &str) -> bool {
    !client_id.is_empty()
}

#[derive(Debug, Clone)]
pub struct DeviceCode {
    pub device_code: String,
    /// The short code the user types into the browser verification page.
    pub user_code: String,
    /// The URL the user opens to authorize (e.g. `https://github.com/login/device`).
    pub verification_uri: String,
    /// Minimum seconds to wait between [`poll_for_token`] calls.
    pub interval: u64,
}

fn default_interval() -> u64 {
    5
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default = "default_interval")]
    interval: u64,
}

/// Step 1: ask GitHub for a device code and the user-facing verification code.
pub async fn request_device_code(client: &Arc<dyn HttpClient>) -> Result<DeviceCode> {
    let client_id = github_client_id();
    if !client_id_is_configured(&client_id) {
        bail!(
            "GitHub sign-in is not configured. Register a Lathe GitHub OAuth app \
             with device flow enabled and set its client id (or export \
             LATHE_GITHUB_CLIENT_ID) to connect GitHub."
        );
    }
    let body = format!(
        "client_id={}&scope={}",
        encode(&client_id),
        encode(GITHUB_SCOPE)
    );
    let request = Request::post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("User-Agent", "Lathe")
        .follow_redirects(http_client::RedirectPolicy::FollowAll)
        .body(AsyncBody::from(body))?;

    let mut response = client
        .send(request)
        .await
        .context("requesting GitHub device code")?;
    let mut bytes = Vec::new();
    response.body_mut().read_to_end(&mut bytes).await?;
    if !response.status().is_success() {
        let text = String::from_utf8_lossy(&bytes);
        bail!(
            "GitHub device code request failed ({}): {text:?}",
            response.status().as_u16()
        );
    }

    let parsed: DeviceCodeResponse =
        serde_json::from_slice(&bytes).context("parsing GitHub device code response")?;
    Ok(DeviceCode {
        device_code: parsed.device_code,
        user_code: parsed.user_code,
        verification_uri: parsed.verification_uri,
        interval: parsed.interval,
    })
}

/// Outcome of a single token poll. The caller loops while it sees `Pending` or
/// `SlowDown`, honoring (and, on `SlowDown`, increasing) the poll interval.
pub enum DeviceTokenPoll {
    Pending,
    SlowDown,
    Authorized(String),
}

#[derive(Deserialize)]
struct AccessTokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// Step 2 (repeated): poll for the access token once the user has authorized.
pub async fn poll_for_token(
    client: &Arc<dyn HttpClient>,
    device_code: &str,
) -> Result<DeviceTokenPoll> {
    let body = format!(
        "client_id={}&device_code={}&grant_type={}",
        encode(&github_client_id()),
        encode(device_code),
        encode("urn:ietf:params:oauth:grant-type:device_code")
    );
    let request = Request::post("https://github.com/login/oauth/access_token")
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("User-Agent", "Lathe")
        .follow_redirects(http_client::RedirectPolicy::FollowAll)
        .body(AsyncBody::from(body))?;

    let mut response = client
        .send(request)
        .await
        .context("polling GitHub for device token")?;
    let mut bytes = Vec::new();
    response.body_mut().read_to_end(&mut bytes).await?;

    let parsed: AccessTokenResponse =
        serde_json::from_slice(&bytes).context("parsing GitHub device token response")?;
    if let Some(token) = parsed.access_token {
        return Ok(DeviceTokenPoll::Authorized(token));
    }
    match parsed.error.as_deref() {
        Some("authorization_pending") => Ok(DeviceTokenPoll::Pending),
        Some("slow_down") => Ok(DeviceTokenPoll::SlowDown),
        Some("expired_token") => bail!("the device code expired before authorization completed"),
        Some("access_denied") => bail!("authorization was denied"),
        Some(other) => bail!("GitHub device authorization failed: {other}"),
        None => bail!("GitHub returned neither an access token nor an error"),
    }
}

/// Fetches the authenticated user's GitHub login, for display after connecting.
pub async fn fetch_login(client: &Arc<dyn HttpClient>, token: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct GithubUser {
        login: String,
    }

    let request = Request::get("https://api.github.com/user")
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "Lathe")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("Authorization", format!("Bearer {token}"))
        .follow_redirects(http_client::RedirectPolicy::FollowAll)
        .body(AsyncBody::default())?;

    let mut response = client
        .send(request)
        .await
        .context("fetching authenticated GitHub user")?;
    let mut bytes = Vec::new();
    response.body_mut().read_to_end(&mut bytes).await?;
    if !response.status().is_success() {
        let text = String::from_utf8_lossy(&bytes);
        bail!(
            "fetching GitHub user failed ({}): {text:?}",
            response.status().as_u16()
        );
    }

    let user: GithubUser = serde_json::from_slice(&bytes).context("parsing GitHub user")?;
    Ok(user.login)
}
