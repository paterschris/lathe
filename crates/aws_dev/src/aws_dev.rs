//! AWS profile selector for the status bar.
//!
//! Lets the user pick an AWS profile whose browser login session (created
//! with `aws login` or `aws sso login`, no long-lived access keys) is
//! injected into every locally spawned terminal, task, and debug session via
//! `AWS_PROFILE`. The env overlay itself lives in
//! [`project::terminals::ActiveAwsProfile`] so both the terminal and DAP
//! spawn paths can reach it.

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

pub(crate) const SESSION_POLL_INTERVAL: Duration = Duration::from_secs(60);

const ACTIVE_PROFILE_KEY: &str = "aws_dev_active_profile";
const V2_COMPAT_KEY: &str = "aws_dev_v2_compat";

pub fn init(cx: &mut App) {
    let store = KeyValueStore::global(cx);
    let profile = store
        .read_kvp(ACTIVE_PROFILE_KEY)
        .log_err()
        .flatten()
        .filter(|profile| !profile.is_empty());
    let v2_compat = store
        .read_kvp(V2_COMPAT_KEY)
        .log_err()
        .flatten()
        .is_some_and(|value| value == "true");
    cx.set_global(ActiveAwsProfile { profile, v2_compat });
}

pub(crate) fn persist_state(cx: &App) {
    let state = cx
        .try_global::<ActiveAwsProfile>()
        .cloned()
        .unwrap_or_default();
    let store = KeyValueStore::global(cx);
    write_and_log(cx, move || async move {
        store
            .write_kvp(
                ACTIVE_PROFILE_KEY.to_string(),
                state.profile.unwrap_or_default(),
            )
            .await?;
        store
            .write_kvp(V2_COMPAT_KEY.to_string(), state.v2_compat.to_string())
            .await
    });
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AwsProfile {
    pub name: SharedString,
    /// Backed by IAM Identity Center, so logging in goes through
    /// `aws sso login` instead of `aws login`.
    pub sso: bool,
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

fn aws_config_path() -> PathBuf {
    std::env::var("AWS_CONFIG_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| paths::home_dir().join(".aws").join("config"))
}

pub(crate) fn discover_profiles() -> Vec<AwsProfile> {
    match std::fs::read_to_string(aws_config_path()) {
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
                });
                current = Some(profiles.len() - 1);
            }
        } else if let Some(index) = current
            && let Some((key, _)) = line.split_once('=')
        {
            let key = key.trim();
            if (key == "sso_session" || key == "sso_start_url")
                && let Some(profile) = profiles.get_mut(index)
            {
                profile.sso = true;
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

#[derive(Deserialize)]
struct ExportedCredentials {
    #[serde(rename = "Expiration")]
    expiration: Option<String>,
}

pub(crate) async fn probe_session(profile: String) -> SessionStatus {
    let output = new_command("aws")
        .args([
            "configure",
            "export-credentials",
            "--profile",
            &profile,
            "--format",
            "process",
        ])
        .output()
        .await;
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

pub(crate) async fn run_login(profile: String, sso: bool) -> Result<()> {
    let mut command = new_command("aws");
    if sso {
        command.args(["sso", "login"]);
    } else {
        command.arg("login");
    }
    command.args(["--profile", &profile]);
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

/// Append a `credential_process` wrapper profile to the AWS config file so
/// AWS SDK v2 apps (which can't read login/SSO sessions from a profile
/// natively) resolve self-refreshing credentials through the CLI.
pub(crate) fn ensure_v2_wrapper(profile: &str) -> Result<()> {
    if !profile
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        bail!("profile name '{profile}' is not supported for the SDK v2 wrapper");
    }
    let path = aws_config_path();
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

[profile lathe-s3-test]
credential_process = aws configure export-credentials --profile s3-test --format process
"#;
        let profiles = parse_profiles(config);
        assert_eq!(
            profiles
                .iter()
                .map(|profile| (profile.name.as_ref(), profile.sso))
                .collect::<Vec<_>>(),
            vec![("default", false), ("s3-test", false), ("work", true)],
        );
    }
}
