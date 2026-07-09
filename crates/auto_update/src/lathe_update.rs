use crate::ReleaseAsset;
use anyhow::{Context as _, Result};
use http_client::{HttpClient, HttpClientWithUrl};
use release_channel::ReleaseChannel;
use serde::Deserialize;
use smol::io::AsyncReadExt;
use std::{env, sync::Arc};

/// GitHub API URL for Lathe releases, resolved per channel.
///
/// - `LATHE_BETA_UPDATE_URL` overrides the URL for Beta builds.
/// - `LATHE_UPDATE_URL` overrides the URL for Stable/Preview/Beta builds.
///
/// Both are read from `option_env!` (so CI can bake a different URL into the
/// binary at build time) with a runtime `env::var` fallback for development.
/// When neither is set, Stable/Preview/Beta default to the Lathe fork's
/// GitHub releases endpoint so locally built binaries still get update
/// checks without any environment configuration. Nightly/Dev never poll.
const LATHE_DEFAULT_UPDATE_URL: &str =
    "https://api.github.com/repos/paterschris/lathe/releases?per_page=30";

pub fn update_base_url(channel: ReleaseChannel) -> Option<String> {
    let beta = option_env!("LATHE_BETA_UPDATE_URL")
        .map(str::to_owned)
        .or_else(|| env::var("LATHE_BETA_UPDATE_URL").ok())
        .filter(|s| !s.is_empty());
    let generic = option_env!("LATHE_UPDATE_URL")
        .map(str::to_owned)
        .or_else(|| env::var("LATHE_UPDATE_URL").ok())
        .filter(|s| !s.is_empty());
    match channel {
        ReleaseChannel::Beta => Some(
            beta.or(generic)
                .unwrap_or_else(|| LATHE_DEFAULT_UPDATE_URL.to_owned()),
        ),
        ReleaseChannel::Stable | ReleaseChannel::Preview => {
            Some(generic.unwrap_or_else(|| LATHE_DEFAULT_UPDATE_URL.to_owned()))
        }
        ReleaseChannel::Nightly | ReleaseChannel::Dev => None,
    }
}

/// Optional HTML URL for release notes (e.g. the GitHub releases HTML page).
/// Falls back to a naive transform of the API URL, then to `None`.
pub fn release_notes_url(channel: ReleaseChannel) -> Option<String> {
    if let Some(explicit) = option_env!("LATHE_RELEASE_NOTES_URL")
        .map(str::to_owned)
        .or_else(|| env::var("LATHE_RELEASE_NOTES_URL").ok())
        .filter(|s| !s.is_empty())
    {
        return Some(explicit);
    }
    let api = update_base_url(channel)?;
    // api.github.com/repos/OWNER/REPO/releases/... -> github.com/OWNER/REPO/releases
    api.strip_prefix("https://api.github.com/repos/")
        .and_then(|rest| rest.split_once("/releases"))
        .map(|(owner_repo, _)| format!("https://github.com/{owner_repo}/releases"))
        .or(Some(api))
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize, Clone)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

/// The tag suffix used by the release workflow for each non-stable channel.
/// Stable has no suffix. Dev has no published releases.
fn channel_tag_suffix(channel: ReleaseChannel) -> Option<&'static str> {
    match channel {
        ReleaseChannel::Stable => Some(""),
        ReleaseChannel::Preview => Some("-preview"),
        ReleaseChannel::Beta => Some("-beta"),
        ReleaseChannel::Nightly => Some("-nightly"),
        ReleaseChannel::Dev => None,
    }
}

fn release_matches_channel(release: &GitHubRelease, channel: ReleaseChannel) -> bool {
    if release.draft {
        return false;
    }
    let Some(suffix) = channel_tag_suffix(channel) else {
        return false;
    };
    if suffix.is_empty() {
        // Stable: tag has no `-foo` suffix at all (e.g. `v0.236.0`).
        !release.tag_name.contains('-')
    } else {
        release.tag_name.ends_with(suffix)
    }
}

pub async fn get_release_asset(
    http_client: Arc<HttpClientWithUrl>,
    api_url: &str,
    channel: ReleaseChannel,
    os: &str,
    arch: &str,
) -> Result<ReleaseAsset> {
    let mut response = http_client.get(api_url, Default::default(), true).await?;
    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;

    anyhow::ensure!(
        response.status().is_success(),
        "failed to fetch release from {api_url}: {:?}",
        String::from_utf8_lossy(&body),
    );

    // Accept either response shape so old binaries with `/releases/latest`
    // baked in keep working, while newer binaries can be pointed at the
    // `/releases` list endpoint to discover prerelease channels (beta,
    // preview) that `/latest` excludes. List responses are returned newest
    // first by the GitHub API, so the first match wins.
    let release: GitHubRelease = if let Ok(single) = serde_json::from_slice::<GitHubRelease>(&body)
    {
        single
    } else {
        let releases: Vec<GitHubRelease> = serde_json::from_slice(&body).with_context(|| {
            format!(
                "error deserializing release(s) {:?}",
                String::from_utf8_lossy(&body),
            )
        })?;
        releases
            .into_iter()
            .find(|r| release_matches_channel(r, channel))
            .with_context(|| format!("no release matched channel {:?} at {api_url}", channel))?
    };

    // Asset naming produced by the release workflow:
    //   macOS:   Lathe-<version>-<arch>-macos.{dmg,zip}    (arch: aarch64 | x86_64)
    //   Linux:   Lathe-<version>-<arch>-linux.tar.gz
    //   Windows: Lathe-<version>-<arch>.exe
    // macOS prefers .dmg when available (used by the in-app installer's
    // hdiutil mount flow); .zip is accepted as a fallback.
    let matches = |asset: &GitHubAsset, ext: &str| {
        asset.name.ends_with(ext)
            && asset.name.contains(arch)
            && (os != "linux" || asset.name.contains("linux"))
    };

    let asset = match os {
        "macos" => release
            .assets
            .iter()
            .find(|a| matches(a, ".dmg"))
            .or_else(|| release.assets.iter().find(|a| matches(a, ".zip")))
            .cloned(),
        "linux" => release
            .assets
            .iter()
            .find(|a| matches(a, ".tar.gz"))
            .cloned(),
        "windows" => release.assets.iter().find(|a| matches(a, ".exe")).cloned(),
        other => anyhow::bail!("unsupported os: {other}"),
    }
    .with_context(|| {
        format!(
            "no matching asset for os={os} arch={arch} in release {}",
            release.tag_name
        )
    })?;

    // tag_name like "v0.234.7" -> semver "0.234.7".
    let version = release.tag_name.trim_start_matches('v').to_string();

    Ok(ReleaseAsset {
        version,
        url: asset.browser_download_url,
    })
}
