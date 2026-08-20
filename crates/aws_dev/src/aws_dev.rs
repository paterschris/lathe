//! AWS profile selector for the status bar.
//!
//! Lets the user pick an AWS profile whose browser login session (created
//! with `aws login` or `aws sso login`, no long-lived access keys) is
//! injected into every locally spawned terminal, task, and debug session via
//! `AWS_PROFILE`. The selection is per window: the env overlay
//! ([`project::terminals::ActiveAwsProfile`]) lives on each workspace's
//! `Project`, and it is persisted per workspace so a reopened project comes
//! back with the profile it had. The menu is per window too: it lists only
//! the profiles that have been used in that workspace (its "shortlist"),
//! with the rest reachable behind a "Show All Profiles" entry, so every
//! window isn't cluttered with every profile in `~/.aws/config`. And when a
//! project brings its own AWS config (`.aws/config` at a worktree root),
//! that file replaces the global one entirely: only its profiles are listed,
//! and spawned processes get `AWS_CONFIG_FILE` pointing at it.

mod profile_picker;

pub use profile_picker::AwsProfileSelector;

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use db::kvp::KeyValueStore;
use db::write_and_log;
use gpui::{App, SharedString};
use project::terminals::ActiveAwsProfile;
use serde::Deserialize;
use util::ResultExt as _;
use util::command::new_command;
use workspace::WorkspaceId;

pub(crate) const SESSION_POLL_INTERVAL: Duration = Duration::from_secs(60);

const ACTIVE_PROFILE_KEY: &str = "aws_dev_active_profile";
const V2_COMPAT_KEY: &str = "aws_dev_v2_compat";
const KNOWN_PROFILES_KEY: &str = "aws_dev_known_profiles";
const CONFIG_FILE_KEY: &str = "aws_dev_config_file";

fn profile_key(workspace_id: WorkspaceId) -> String {
    format!("{ACTIVE_PROFILE_KEY}-{}", workspace_id.raw())
}

fn v2_compat_key(workspace_id: WorkspaceId) -> String {
    format!("{V2_COMPAT_KEY}-{}", workspace_id.raw())
}

fn known_profiles_key(workspace_id: WorkspaceId) -> String {
    format!("{KNOWN_PROFILES_KEY}-{}", workspace_id.raw())
}

fn config_file_key(workspace_id: WorkspaceId) -> String {
    format!("{CONFIG_FILE_KEY}-{}", workspace_id.raw())
}

pub(crate) fn restore_known_profiles(workspace_id: WorkspaceId, cx: &App) -> Vec<String> {
    KeyValueStore::global(cx)
        .read_kvp(&known_profiles_key(workspace_id))
        .log_err()
        .flatten()
        .and_then(|value| serde_json::from_str::<Vec<String>>(&value).log_err())
        .unwrap_or_default()
}

pub(crate) fn persist_known_profiles(workspace_id: WorkspaceId, profiles: &[String], cx: &App) {
    let Some(value) = serde_json::to_string(profiles).log_err() else {
        return;
    };
    let store = KeyValueStore::global(cx);
    let key = known_profiles_key(workspace_id);
    write_and_log(cx, move || async move { store.write_kvp(key, value).await });
}

pub(crate) fn restore_state(workspace_id: WorkspaceId, cx: &App) -> Option<ActiveAwsProfile> {
    let store = KeyValueStore::global(cx);
    let profile = store
        .read_kvp(&profile_key(workspace_id))
        .log_err()
        .flatten()
        .filter(|profile| !profile.is_empty());
    let v2_compat = store
        .read_kvp(&v2_compat_key(workspace_id))
        .log_err()
        .flatten()
        .is_some_and(|value| value == "true");
    let config_file = store
        .read_kvp(&config_file_key(workspace_id))
        .log_err()
        .flatten()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    if profile.is_none() && !v2_compat {
        None
    } else {
        Some(ActiveAwsProfile {
            profile,
            v2_compat,
            config_file,
        })
    }
}

pub(crate) fn persist_state(workspace_id: WorkspaceId, state: &ActiveAwsProfile, cx: &App) {
    let store = KeyValueStore::global(cx);
    let profile = state.profile.clone().unwrap_or_default();
    let v2_compat = state.v2_compat.to_string();
    let config_file = state
        .config_file
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    let profile_key = profile_key(workspace_id);
    let v2_compat_key = v2_compat_key(workspace_id);
    let config_file_key = config_file_key(workspace_id);
    write_and_log(cx, move || async move {
        store.write_kvp(profile_key, profile).await?;
        store.write_kvp(v2_compat_key, v2_compat).await?;
        store.write_kvp(config_file_key, config_file).await
    });
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AwsProfile {
    pub name: SharedString,
    /// Backed by IAM Identity Center, so logging in goes through
    /// `aws sso login` instead of `aws login`.
    pub sso: bool,
    /// `Some(target)` when the profile proxies another one via
    /// `credential_process = aws configure export-credentials --profile
    /// <target>`. Such a profile can't hold a browser login session itself
    /// (the CLI refuses), so logging in has to happen on the target.
    pub chained_to: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub enum SessionStatus {
    #[default]
    Unknown,
    CliMissing,
    NotLoggedIn,
    Active {
        expires_at: Option<DateTime<Utc>>,
    },
}

pub(crate) fn aws_config_path() -> PathBuf {
    std::env::var("AWS_CONFIG_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| paths::home_dir().join(".aws").join("config"))
}

pub(crate) fn discover_profiles(config_path: PathBuf) -> Vec<AwsProfile> {
    match std::fs::read_to_string(config_path) {
        Ok(contents) => parse_profiles(&contents),
        Err(_) => Vec::new(),
    }
}

fn parse_profiles(contents: &str) -> Vec<AwsProfile> {
    let mut profiles: Vec<AwsProfile> = Vec::new();
    let mut current: Option<usize> = None;
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            let section = line[1..line.len() - 1].trim();
            let name = if section == "default" {
                Some(section)
            } else {
                section.strip_prefix("profile ").map(str::trim)
            };
            current = None;
            if let Some(name) = name
                && !name.is_empty()
                && !name.starts_with("lathe-")
                && !profiles.iter().any(|profile| profile.name.as_ref() == name)
            {
                profiles.push(AwsProfile {
                    name: SharedString::from(name.to_string()),
                    sso: false,
                    chained_to: None,
                });
                current = Some(profiles.len() - 1);
            }
        } else if let Some(index) = current
            && let Some((key, value)) = line.split_once('=')
        {
            let key = key.trim();
            if (key == "sso_session" || key == "sso_start_url")
                && let Some(profile) = profiles.get_mut(index)
            {
                profile.sso = true;
            } else if key == "credential_process"
                && let Some(profile) = profiles.get_mut(index)
            {
                profile.chained_to = parse_export_credentials_target(value);
            }
        }
    }
    profiles.sort_by(|a, b| {
        (a.name.as_ref() != "default")
            .cmp(&(b.name.as_ref() != "default"))
            .then_with(|| a.name.cmp(&b.name))
    });
    profiles
}

fn parse_export_credentials_target(value: &str) -> Option<String> {
    let mut tokens = value.split_whitespace();
    if (tokens.next(), tokens.next(), tokens.next())
        != (Some("aws"), Some("configure"), Some("export-credentials"))
    {
        return None;
    }
    let mut tokens = tokens.skip_while(|token| *token != "--profile");
    tokens.next();
    tokens.next().map(|target| target.to_string())
}

#[derive(Deserialize)]
struct ExportedCredentials {
    #[serde(rename = "Expiration")]
    expiration: Option<String>,
}

pub(crate) async fn probe_session(profile: String, config_file: Option<PathBuf>) -> SessionStatus {
    let mut command = new_command("aws");
    command.args([
        "configure",
        "export-credentials",
        "--profile",
        &profile,
        "--format",
        "process",
    ]);
    if let Some(config_file) = config_file {
        command.env("AWS_CONFIG_FILE", config_file);
    }
    let output = command.output().await;
    match output {
        Err(error) if error.kind() == io::ErrorKind::NotFound => SessionStatus::CliMissing,
        Err(error) => {
            log::warn!("aws_dev: failed to run the aws CLI: {error}");
            SessionStatus::NotLoggedIn
        }
        Ok(output) if output.status.success() => {
            let expires_at = serde_json::from_slice::<ExportedCredentials>(&output.stdout)
                .ok()
                .and_then(|credentials| credentials.expiration)
                .and_then(|expiration| DateTime::parse_from_rfc3339(&expiration).ok())
                .map(|expiration| expiration.with_timezone(&Utc));
            SessionStatus::Active { expires_at }
        }
        Ok(_) => SessionStatus::NotLoggedIn,
    }
}

pub(crate) async fn run_login(
    profile: String,
    sso: bool,
    config_file: Option<PathBuf>,
) -> Result<()> {
    let mut command = new_command("aws");
    if sso {
        command.args(["sso", "login"]);
    } else {
        command.arg("login");
    }
    command.args(["--profile", &profile]);
    if let Some(config_file) = config_file {
        command.env("AWS_CONFIG_FILE", config_file);
    }
    let output = command
        .output()
        .await
        .context("running the `aws` CLI (is AWS CLI v2 installed?)")?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("`aws login` failed with no error output")
            .trim()
            .to_string();
        Err(anyhow!(detail))
    }
}

/// Append a `credential_process` wrapper profile to the given AWS config
/// file (global or project-local) so AWS SDK v2 apps (which can't read
/// login/SSO sessions from a profile natively) resolve self-refreshing
/// credentials through the CLI.
pub(crate) fn ensure_v2_wrapper(profile: &str, path: PathBuf) -> Result<()> {
    if !profile
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        bail!("profile name '{profile}' is not supported for the SDK v2 wrapper");
    }
    let contents = std::fs::read_to_string(&path).unwrap_or_default();
    let header = format!("[profile {}]", ActiveAwsProfile::wrapper_name(profile));
    if contents.lines().any(|line| line.trim() == header) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut block = String::new();
    if !contents.is_empty() && !contents.ends_with('\n') {
        block.push('\n');
    }
    block.push_str(&format!(
        "\n# Added by Lathe so AWS SDK v2 apps can use the '{profile}' login session.\n\
         # Safe to delete.\n\
         {header}\n\
         credential_process = aws configure export-credentials --profile {profile} --format process\n"
    ));
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(block.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_profiles_and_detects_sso() {
        let config = r#"
[default]
region = us-east-1

[profile work]
sso_session = my-org
region = us-west-2

[profile s3-test]
region = eu-west-1

[profile staging]
credential_process = aws configure export-credentials --profile default --format process
region = us-east-1

[profile lathe-s3-test]
credential_process = aws configure export-credentials --profile s3-test --format process
"#;
        let profiles = parse_profiles(config);
        assert_eq!(
            profiles
                .iter()
                .map(|profile| (
                    profile.name.as_ref(),
                    profile.sso,
                    profile.chained_to.as_deref()
                ))
                .collect::<Vec<_>>(),
            vec![
                ("default", false, None),
                ("s3-test", false, None),
                ("staging", false, Some("default")),
                ("work", true, None),
            ],
        );
    }
}
