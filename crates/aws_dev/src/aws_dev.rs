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

/// The first worktree root that carries its own `.aws/config`. Worktree
/// order decides precedence, so a multi-root project takes the config from
/// the first root that has one rather than merging them.
pub(crate) fn project_config_path(worktree_roots: &[PathBuf]) -> Option<PathBuf> {
    worktree_roots
        .iter()
        .map(|root| root.join(".aws").join("config"))
        .find(|path| path.is_file())
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
    if profile.is_empty()
        || !profile
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

    #[test]
    fn parses_sso_from_either_marker_key() {
        let config = r#"
[profile via-session]
sso_session = my-org

[profile via-start-url]
sso_start_url = https://example.awsapps.com/start

[profile plain]
region = us-east-1
"#;
        let profiles = parse_profiles(config);
        assert_eq!(
            profiles
                .iter()
                .map(|profile| (profile.name.as_ref(), profile.sso))
                .collect::<Vec<_>>(),
            vec![("plain", false), ("via-session", true), ("via-start-url", true)],
        );
    }

    #[test]
    fn ignores_sections_that_are_not_profiles() {
        // Only `[default]` and `[profile <name>]` name a profile. `[sso-session
        // my-org]` and a bare `[work]` are valid config sections but are not
        // profiles, and keys before any section belong to no profile at all.
        let config = r#"
region = us-east-1

[sso-session my-org]
sso_start_url = https://example.awsapps.com/start

[work]
sso_session = my-org

[services my-services]
s3 =

[default]
region = eu-west-1
"#;
        let profiles = parse_profiles(config);
        assert_eq!(
            profiles
                .iter()
                .map(|profile| profile.name.as_ref())
                .collect::<Vec<_>>(),
            vec!["default"],
        );
        assert!(!profiles[0].sso);
    }

    #[test]
    fn sorts_default_first_then_alphabetically() {
        let config = "[profile zulu]\n[profile alpha]\n[default]\n[profile Mike]\n";
        assert_eq!(
            parse_profiles(config)
                .iter()
                .map(|profile| profile.name.as_ref())
                .collect::<Vec<_>>(),
            vec!["default", "Mike", "alpha", "zulu"],
        );
    }

    #[test]
    fn hides_the_lathe_wrapper_profiles_it_writes() {
        let config = "[profile work]\n[profile lathe-work]\ncredential_process = aws configure export-credentials --profile work --format process\n";
        assert_eq!(
            parse_profiles(config)
                .iter()
                .map(|profile| profile.name.as_ref())
                .collect::<Vec<_>>(),
            vec!["work"],
        );
    }

    #[test]
    fn a_repeated_section_is_dropped_rather_than_merged() {
        // The AWS CLI merges duplicate sections; this parser keeps the first
        // and ignores the second, including its keys. Pinned so a change in
        // this behavior is deliberate rather than incidental.
        let config = r#"
[profile work]
region = us-east-1

[profile work]
sso_session = my-org
"#;
        let profiles = parse_profiles(config);
        assert_eq!(profiles.len(), 1);
        assert!(!profiles[0].sso, "the repeated section's keys are ignored");
    }

    #[test]
    fn parses_nothing_from_empty_or_malformed_input() {
        for config in ["", "\n\n", "[unclosed\n", "]backwards[\n", "= no key\n", "[]\n", "[profile ]\n"] {
            assert!(
                parse_profiles(config).is_empty(),
                "expected no profiles from {config:?}",
            );
        }
    }

    #[test]
    fn credential_process_target_requires_the_exact_export_command() {
        assert_eq!(
            parse_export_credentials_target(
                " aws configure export-credentials --profile work --format process"
            ),
            Some("work".to_string()),
        );
        for value in [
            "aws configure list --profile work",
            "aws sso login --profile work",
            "some-helper --profile work",
            "aws configure",
            "",
        ] {
            assert_eq!(
                parse_export_credentials_target(value),
                None,
                "expected no target from {value:?}",
            );
        }
    }

    #[test]
    fn credential_process_target_handles_a_missing_or_dangling_profile_flag() {
        assert_eq!(
            parse_export_credentials_target("aws configure export-credentials --format process"),
            None,
        );
        assert_eq!(
            parse_export_credentials_target("aws configure export-credentials --profile"),
            None,
        );
        assert_eq!(
            parse_export_credentials_target(
                "aws configure export-credentials --format process --profile work"
            ),
            Some("work".to_string()),
        );
    }

    #[test]
    fn project_config_path_takes_the_first_root_that_has_one() {
        let dir = tempfile::tempdir().expect("creating a temp dir");
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir_all(first.join(".aws")).expect("creating the first root");
        std::fs::create_dir_all(second.join(".aws")).expect("creating the second root");
        std::fs::write(second.join(".aws").join("config"), "[default]\n")
            .expect("writing the second config");

        // Only the second root has one, so it wins despite the ordering.
        assert_eq!(
            project_config_path(&[first.clone(), second.clone()]),
            Some(second.join(".aws").join("config")),
        );

        // Once both have one, worktree order decides.
        std::fs::write(first.join(".aws").join("config"), "[default]\n")
            .expect("writing the first config");
        assert_eq!(
            project_config_path(&[first.clone(), second.clone()]),
            Some(first.join(".aws").join("config")),
        );
        assert_eq!(
            project_config_path(&[second.clone(), first]),
            Some(second.join(".aws").join("config")),
        );
    }

    #[test]
    fn project_config_path_ignores_roots_without_a_config_file() {
        let dir = tempfile::tempdir().expect("creating a temp dir");
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root).expect("creating the root");
        assert_eq!(project_config_path(std::slice::from_ref(&root)), None);
        assert_eq!(project_config_path(&[]), None);

        // A directory named `config` is not a config file.
        std::fs::create_dir_all(root.join(".aws").join("config")).expect("creating the decoy");
        assert_eq!(project_config_path(&[root]), None);
    }

    #[test]
    fn v2_wrapper_appends_without_disturbing_existing_contents() {
        let dir = tempfile::tempdir().expect("creating a temp dir");
        let path = dir.path().join("config");
        let original = "[default]\nregion = us-east-1\n\n[profile work]\nsso_session = my-org\n";
        std::fs::write(&path, original).expect("writing the config");

        ensure_v2_wrapper("work", path.clone()).expect("adding the wrapper");

        let contents = std::fs::read_to_string(&path).expect("reading the config back");
        assert!(
            contents.starts_with(original),
            "the original config must be preserved byte for byte",
        );
        assert!(contents.contains("[profile lathe-work]"));
        assert!(contents.contains(
            "credential_process = aws configure export-credentials --profile work --format process"
        ));
    }

    #[test]
    fn v2_wrapper_is_idempotent() {
        let dir = tempfile::tempdir().expect("creating a temp dir");
        let path = dir.path().join("config");
        std::fs::write(&path, "[profile work]\n").expect("writing the config");

        ensure_v2_wrapper("work", path.clone()).expect("adding the wrapper");
        let after_first = std::fs::read_to_string(&path).expect("reading the config back");
        ensure_v2_wrapper("work", path.clone()).expect("re-running the wrapper");
        let after_second = std::fs::read_to_string(&path).expect("reading the config back");

        assert_eq!(
            after_first, after_second,
            "a second call must not append a duplicate block",
        );
        assert_eq!(after_second.matches("[profile lathe-work]").count(), 1);
    }

    #[test]
    fn v2_wrapper_separates_the_block_from_an_unterminated_last_line() {
        let dir = tempfile::tempdir().expect("creating a temp dir");
        let path = dir.path().join("config");
        // No trailing newline: appending naively would splice the comment onto
        // the end of `region = us-east-1` and corrupt the profile.
        std::fs::write(&path, "[profile work]\nregion = us-east-1").expect("writing the config");

        ensure_v2_wrapper("work", path.clone()).expect("adding the wrapper");

        let contents = std::fs::read_to_string(&path).expect("reading the config back");
        assert!(contents.contains("region = us-east-1\n"));
        assert_eq!(
            parse_profiles(&contents)
                .iter()
                .map(|profile| profile.name.as_ref())
                .collect::<Vec<_>>(),
            vec!["work"],
        );
    }

    #[test]
    fn v2_wrapper_creates_a_missing_config_and_its_parents() {
        let dir = tempfile::tempdir().expect("creating a temp dir");
        let path = dir.path().join("nested").join(".aws").join("config");

        ensure_v2_wrapper("work", path.clone()).expect("adding the wrapper");

        let contents = std::fs::read_to_string(&path).expect("reading the new config");
        assert!(contents.contains("[profile lathe-work]"));
    }

    #[test]
    fn v2_wrapper_refuses_names_that_would_corrupt_the_config() {
        let dir = tempfile::tempdir().expect("creating a temp dir");
        let path = dir.path().join("config");
        let original = "[default]\nregion = us-east-1\n";

        // This file is the user's own `~/.aws/config` in the common case, so a
        // name carrying a newline or a bracket must be refused before any write
        // rather than escaping the block it is meant to stay inside.
        for name in [
            "work]\n[profile injected",
            "work\nregion = elsewhere",
            "with space",
            "semi;colon",
            "quote\"mark",
            "",
        ] {
            std::fs::write(&path, original).expect("writing the config");
            let result = ensure_v2_wrapper(name, path.clone());
            assert!(result.is_err(), "expected {name:?} to be refused");
            assert_eq!(
                std::fs::read_to_string(&path).expect("reading the config back"),
                original,
                "a refused name must leave the file untouched",
            );
        }
    }

    #[test]
    fn v2_wrapper_writes_a_block_its_own_parser_understands() {
        // The writer and the parser have to agree: the wrapper is what makes
        // `chained_to` resolve, and it is what the picker filters out by name.
        let dir = tempfile::tempdir().expect("creating a temp dir");
        let path = dir.path().join("config");
        std::fs::write(&path, "[profile work]\nsso_session = my-org\n")
            .expect("writing the config");

        ensure_v2_wrapper("work", path.clone()).expect("adding the wrapper");

        let contents = std::fs::read_to_string(&path).expect("reading the config back");
        let visible = parse_profiles(&contents);
        assert_eq!(
            visible
                .iter()
                .map(|profile| profile.name.as_ref())
                .collect::<Vec<_>>(),
            vec!["work"],
            "the wrapper must stay hidden from the picker",
        );

        let wrapper_line = contents
            .lines()
            .find(|line| line.trim_start().starts_with("credential_process"))
            .expect("the wrapper writes a credential_process line");
        let (_, value) = wrapper_line
            .split_once('=')
            .expect("the credential_process line is a key/value pair");
        assert_eq!(
            parse_export_credentials_target(value),
            Some("work".to_string()),
            "the parser must recover the target the writer put in",
        );
    }
}
