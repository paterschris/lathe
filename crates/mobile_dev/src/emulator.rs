//! Android emulator (AVD) support for the mobile_dev panel.
//!
//! Like [`crate::adb`] and [`crate::apple`], everything shells out to the
//! Android SDK's own tooling (`emulator -list-avds`, `emulator -avd <name>`)
//! rather than reimplementing anything. Callers pass the managed-toolchain env
//! from [`crate::toolchain::build_env`] so the emulator works even when Lathe
//! was launched without an Android SDK on `PATH`.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use util::command::{Stdio, new_command};

use crate::toolchain;

/// The emulator binary, searched for in every known SDK (Lathe-managed and
/// system); falls back to `emulator` on `PATH`.
pub fn emulator_program() -> PathBuf {
    for sdk in toolchain::sdk_dirs() {
        let emulator = sdk.join("emulator/emulator");
        if emulator.is_file() {
            return emulator;
        }
    }
    PathBuf::from("emulator")
}

/// Path to `sdkmanager` in the SDK that has the command-line tools, if any.
pub fn sdkmanager_path() -> Option<PathBuf> {
    let path = toolchain::sdk_dir()?.join("cmdline-tools/latest/bin/sdkmanager");
    path.is_file().then_some(path)
}

/// Path to `avdmanager` in the SDK that has the command-line tools, if any.
pub fn avdmanager_path() -> Option<PathBuf> {
    let path = toolchain::sdk_dir()?.join("cmdline-tools/latest/bin/avdmanager");
    path.is_file().then_some(path)
}

/// A sensible default system image for this host's architecture. Google APIs
/// images match what Android Studio picks for a fresh AVD.
pub fn default_system_image() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "system-images;android-35;google_apis;arm64-v8a"
    } else {
        "system-images;android-35;google_apis;x86_64"
    }
}

/// The names of the AVDs installed on this machine, from
/// `emulator -list-avds`. Returns an empty list (not an error) when the
/// emulator package isn't installed, so the panel just omits the AVD list.
pub async fn list_avds(env: &[(String, String)]) -> Result<Vec<String>> {
    let mut command = new_command(emulator_program());
    command.arg("-list-avds");
    for (key, value) in env {
        command.env(key, value);
    }
    let output = match command.output().await {
        Ok(output) => output,
        // No emulator binary at all: treat as "no AVDs" rather than surfacing
        // a spawn error on every poll.
        Err(_) => return Ok(Vec::new()),
    };
    if !output.status.success() {
        bail!(
            "emulator -list-avds failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Launch the named AVD. The emulator runs for the lifetime of the simulated
/// device (long past this call), so we spawn it detached rather than with
/// `kill_on_drop`: closing the panel must not tear down a running emulator.
pub fn launch_avd(name: &str, env: &[(String, String)]) -> Result<()> {
    let mut command = new_command(emulator_program());
    command
        .arg("-avd")
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in env {
        command.env(key, value);
    }
    command
        .spawn()
        .with_context(|| format!("launching emulator {name}"))?;
    Ok(())
}
