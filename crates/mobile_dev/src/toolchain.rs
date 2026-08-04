//! Managed Android toolchain: detection and first-run install of JDK 17
//! (Azul Zulu) and the Android SDK into a Lathe-managed directory, so local
//! Android builds work without an Android Studio install or shell profile
//! setup. Mirrors how `node_runtime` manages its Node.js download.
//!
//! Layout under [`managed_root`] (`~/.lathe/android_toolchain`):
//! - `jdk/<zulu release>/` (JAVA_HOME is the folder containing `bin/java`;
//!   on macOS that's the tarball's `Contents/Home`)
//! - `sdk/` (ANDROID_HOME: `cmdline-tools/latest/`, `platform-tools/`,
//!   `platforms/`, `build-tools/`, `licenses/`)

use std::env::consts;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use async_compression::futures::bufread::GzipDecoder;
use async_tar::Archive;
use futures::io::{AsyncBufReadExt, BufReader};
use futures::stream::StreamExt as _;
use futures::{AsyncReadExt as _, AsyncWriteExt as _};
use gpui::SharedString;
use http_client::HttpClient;
use serde::Deserialize;
use smol::channel;
use util::archive::extract_zip;
use util::command::{Stdio, new_command};

/// Pinned Android command-line tools build (the `_latest` alias moves, the
/// numbered artifact does not). https://developer.android.com/studio
const CMDLINE_TOOLS_BUILD: &str = "11076708";

/// Matches the component table in docs/mobile-development.md. The NDK is
/// deliberately absent: with licenses accepted, gradle fetches the exact NDK
/// revision the project pins on first build.
const SDK_PACKAGES: &[&str] = &[
    "platform-tools",
    "platforms;android-35",
    "build-tools;35.0.0",
];

const SDKMANAGER_RELATIVE: &str = "cmdline-tools/latest/bin/sdkmanager";

pub fn managed_root() -> PathBuf {
    // NOT under data_dir(): on macOS that is "~/Library/Application Support",
    // and the Android SDK does not work from a path containing spaces (the
    // sdkmanager launcher mis-splits its own classpath and dies with
    // "Could not find or load main class Support....").
    paths::home_dir().join(".lathe/android_toolchain")
}

fn managed_sdk_root() -> PathBuf {
    managed_root().join("sdk")
}

/// The managed adb binary, when the managed platform-tools are installed.
/// Callers fall back to `adb` on PATH.
pub fn managed_adb_path() -> Option<PathBuf> {
    let adb = managed_sdk_root().join("platform-tools/adb");
    adb.is_file().then_some(adb)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComponentStatus {
    /// Installed in the Lathe-managed directory; the path is the component's
    /// root (JAVA_HOME, ANDROID_HOME, binary, or license file).
    Managed(PathBuf),
    /// Found outside the managed directory (env var or platform default).
    System(PathBuf),
    Missing,
}

impl ComponentStatus {
    pub fn is_present(&self) -> bool {
        !matches!(self, ComponentStatus::Missing)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolchainStatus {
    pub jdk: ComponentStatus,
    pub sdk: ComponentStatus,
    pub platform_tools: ComponentStatus,
    pub licenses: ComponentStatus,
}

impl ToolchainStatus {
    pub fn all_present(&self) -> bool {
        self.jdk.is_present()
            && self.sdk.is_present()
            && self.platform_tools.is_present()
            && self.licenses.is_present()
    }
}

/// Synchronous filesystem probing; call from a background thread.
pub fn detect() -> ToolchainStatus {
    let jdk = if let Some(home) = find_java_home(&managed_root().join("jdk")) {
        ComponentStatus::Managed(home)
    } else if let Some(home) = std::env::var_os("JAVA_HOME")
        .map(PathBuf::from)
        .filter(|home| home.join("bin/java").is_file())
    {
        ComponentStatus::System(home)
    } else {
        ComponentStatus::Missing
    };

    let managed_sdk = managed_sdk_root();
    let managed_sdk_present = managed_sdk.join(SDKMANAGER_RELATIVE).is_file();
    let (sdk_dir, managed) = if managed_sdk_present {
        (Some(managed_sdk), true)
    } else {
        (system_sdk_dir(), false)
    };

    let sdk = match &sdk_dir {
        Some(dir) if dir.join(SDKMANAGER_RELATIVE).is_file() => {
            if managed {
                ComponentStatus::Managed(dir.clone())
            } else {
                ComponentStatus::System(dir.clone())
            }
        }
        _ => ComponentStatus::Missing,
    };

    let component_in_sdk = |relative: &str| -> ComponentStatus {
        match &sdk_dir {
            Some(dir) if dir.join(relative).exists() => {
                if managed {
                    ComponentStatus::Managed(dir.join(relative))
                } else {
                    ComponentStatus::System(dir.join(relative))
                }
            }
            _ => ComponentStatus::Missing,
        }
    };

    ToolchainStatus {
        jdk,
        sdk,
        platform_tools: component_in_sdk("platform-tools/adb"),
        licenses: component_in_sdk("licenses/android-sdk-license"),
    }
}

fn system_sdk_dir() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
        .iter()
        .filter_map(|var| std::env::var_os(var).map(PathBuf::from))
        .collect();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        candidates.push(home.join("Library/Android/sdk"));
        candidates.push(home.join("Android/Sdk"));
    }
    candidates.into_iter().find(|dir| {
        dir.join(SDKMANAGER_RELATIVE).is_file() || dir.join("platform-tools/adb").is_file()
    })
}

fn find_java_home(jdk_dir: &Path) -> Option<PathBuf> {
    for entry in std::fs::read_dir(jdk_dir).ok()?.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        for candidate in [path.join("Contents/Home"), path.clone()] {
            if candidate.join("bin/java").is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Env for build subprocesses so gradle/expo use the managed toolchain
/// without any shell profile setup. Empty when nothing is managed (system
/// toolchains are assumed to already be wired up by the user's shell).
pub fn build_env(status: Option<&ToolchainStatus>) -> Vec<(String, String)> {
    let Some(status) = status else {
        return Vec::new();
    };
    let mut env = Vec::new();
    let mut path_prepends: Vec<PathBuf> = Vec::new();

    if let ComponentStatus::Managed(home) = &status.jdk {
        env.push(("JAVA_HOME".to_string(), home.to_string_lossy().into_owned()));
        path_prepends.push(home.join("bin"));
    }
    if let ComponentStatus::Managed(sdk) = &status.sdk {
        let sdk_str = sdk.to_string_lossy().into_owned();
        env.push(("ANDROID_HOME".to_string(), sdk_str.clone()));
        env.push(("ANDROID_SDK_ROOT".to_string(), sdk_str));
        path_prepends.push(sdk.join("platform-tools"));
        path_prepends.push(sdk.join("cmdline-tools/latest/bin"));
    }
    if !path_prepends.is_empty() {
        let path = path_prepends
            .iter()
            .map(|dir| dir.to_string_lossy().into_owned())
            .chain(std::env::var("PATH").ok())
            .collect::<Vec<_>>()
            .join(":");
        env.push(("PATH".to_string(), path));
    }
    env
}

#[derive(Clone, Debug)]
pub enum InstallEvent {
    Line(SharedString),
    Finished(InstallOutcome),
}

#[derive(Clone, Debug)]
pub enum InstallOutcome {
    Success,
    Failure(SharedString),
}

/// One running toolchain install plus a receiver for its progress lines.
/// The work is a sequence of steps
/// (two downloads, then sdkmanager runs), so a single task drives it all.
pub struct InstallSession {
    events: channel::Receiver<InstallEvent>,
}

impl InstallSession {
    pub fn spawn(http: Arc<dyn HttpClient>) -> Self {
        let (tx, rx) = channel::unbounded::<InstallEvent>();
        smol::spawn(async move {
            let outcome = match run_install(http, &tx).await {
                Ok(()) => InstallOutcome::Success,
                Err(err) => InstallOutcome::Failure(SharedString::from(format!("{err:#}"))),
            };
            tx.send(InstallEvent::Finished(outcome)).await.ok();
        })
        .detach();
        Self { events: rx }
    }

    pub fn events(&self) -> channel::Receiver<InstallEvent> {
        self.events.clone()
    }
}

pub(crate) async fn send_line(tx: &channel::Sender<InstallEvent>, line: impl Into<SharedString>) {
    tx.send(InstallEvent::Line(line.into())).await.ok();
}

async fn run_install(http: Arc<dyn HttpClient>, tx: &channel::Sender<InstallEvent>) -> Result<()> {
    let root = managed_root();
    smol::fs::create_dir_all(&root)
        .await
        .context("creating managed toolchain directory")?;

    let java_home = match detect().jdk {
        ComponentStatus::Managed(home) | ComponentStatus::System(home) => {
            send_line(tx, format!("JDK 17: found at {}", home.display())).await;
            home
        }
        ComponentStatus::Missing => install_jdk(&http, &root, tx).await?,
    };

    let sdk = managed_sdk_root();
    if sdk.join(SDKMANAGER_RELATIVE).is_file() {
        send_line(tx, "Command-line tools: already installed").await;
    } else if system_sdk_dir().is_some_and(|dir| dir.join(SDKMANAGER_RELATIVE).is_file()) {
        // A full system SDK exists; installing a second managed one would
        // just waste disk and shadow it.
        send_line(
            tx,
            "Command-line tools: found in system SDK; skipping managed install",
        )
        .await;
    } else {
        install_cmdline_tools(&http, &sdk, tx).await?;
    }

    // Re-detect: the sdkmanager we drive below may be the system one.
    let status = detect();
    let Some(sdk_root) = (match &status.sdk {
        ComponentStatus::Managed(dir) | ComponentStatus::System(dir) => Some(dir.clone()),
        ComponentStatus::Missing => None,
    }) else {
        bail!("sdkmanager unavailable after command-line tools install");
    };
    let sdkmanager = sdk_root.join(SDKMANAGER_RELATIVE);

    accept_licenses(&sdkmanager, &sdk_root, &java_home, tx).await?;
    install_sdk_packages(&sdkmanager, &sdk_root, &java_home, tx).await?;

    send_line(
        tx,
        "NDK: not preinstalled; gradle downloads the project's pinned revision on first build",
    )
    .await;
    send_line(tx, "Toolchain install complete").await;
    Ok(())
}

#[derive(Deserialize)]
struct ZuluPackage {
    name: String,
    download_url: String,
}

async fn install_jdk(
    http: &Arc<dyn HttpClient>,
    root: &Path,
    tx: &channel::Sender<InstallEvent>,
) -> Result<PathBuf> {
    let os = match consts::OS {
        "macos" => "macos",
        "linux" => "linux",
        other => bail!("managed JDK install is not supported on {other}"),
    };
    let arch = match consts::ARCH {
        "aarch64" => "aarch64",
        "x86_64" => "x64",
        other => bail!("managed JDK install is not supported on {other}"),
    };

    send_line(tx, "JDK 17: resolving latest Azul Zulu release...").await;
    let metadata_url = format!(
        "https://api.azul.com/metadata/v1/zulu/packages/?java_version=17&os={os}&arch={arch}\
         &archive_type=tar.gz&java_package_type=jdk&javafx_bundled=false\
         &latest=true&release_status=ga"
    );
    let mut response = http
        .get(&metadata_url, Default::default(), true)
        .await
        .context("querying Azul metadata API")?;
    if !response.status().is_success() {
        bail!("Azul metadata API returned {}", response.status());
    }
    let mut body = String::new();
    response
        .body_mut()
        .read_to_string(&mut body)
        .await
        .context("reading Azul metadata response")?;
    let mut packages: Vec<ZuluPackage> =
        serde_json::from_str(&body).context("parsing Azul metadata response")?;
    // `latest=true` can surface Azul's CRaC-enabled builds first; prefer the
    // standard JDK when one is in the result set.
    packages.sort_by_key(|package| package.name.contains("-crac-"));
    let package = packages
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("Azul metadata API returned no JDK 17 packages"))?;

    send_line(tx, format!("JDK 17: downloading {}...", package.name)).await;
    let mut response = http
        .get(&package.download_url, Default::default(), true)
        .await
        .context("downloading Zulu JDK tarball")?;
    if !response.status().is_success() {
        bail!("JDK download returned {}", response.status());
    }

    let jdk_dir = root.join("jdk");
    // Clear any partial extraction from an earlier failed attempt.
    smol::fs::remove_dir_all(&jdk_dir).await.ok();
    smol::fs::create_dir_all(&jdk_dir)
        .await
        .context("creating jdk directory")?;

    send_line(tx, "JDK 17: extracting...").await;
    let decompressed = GzipDecoder::new(BufReader::new(response.body_mut()));
    Archive::new(decompressed)
        .unpack(&jdk_dir)
        .await
        .context("extracting Zulu JDK tarball")?;

    let home = find_java_home(&jdk_dir)
        .ok_or_else(|| anyhow!("extracted JDK has no bin/java under {}", jdk_dir.display()))?;
    send_line(tx, format!("JDK 17: installed at {}", home.display())).await;
    Ok(home)
}

async fn install_cmdline_tools(
    http: &Arc<dyn HttpClient>,
    sdk: &Path,
    tx: &channel::Sender<InstallEvent>,
) -> Result<()> {
    let os = match consts::OS {
        "macos" => "mac",
        "linux" => "linux",
        other => bail!("managed SDK install is not supported on {other}"),
    };
    let url = format!(
        "https://dl.google.com/android/repository/commandlinetools-{os}-{CMDLINE_TOOLS_BUILD}_latest.zip"
    );

    send_line(tx, "Command-line tools: downloading...").await;
    let mut response = http
        .get(&url, Default::default(), true)
        .await
        .context("downloading Android command-line tools")?;
    if !response.status().is_success() {
        bail!("command-line tools download returned {}", response.status());
    }

    let staging = sdk.join("cmdline-tools-staging");
    smol::fs::remove_dir_all(&staging).await.ok();
    smol::fs::create_dir_all(&staging)
        .await
        .context("creating cmdline-tools staging directory")?;

    send_line(tx, "Command-line tools: extracting...").await;
    extract_zip(&staging, response.body_mut())
        .await
        .context("extracting command-line tools zip")?;

    // The zip's single root folder is `cmdline-tools/`; sdkmanager insists on
    // living at `cmdline-tools/latest/`.
    let latest = sdk.join("cmdline-tools/latest");
    smol::fs::remove_dir_all(&latest).await.ok();
    smol::fs::create_dir_all(
        latest
            .parent()
            .context("cmdline-tools path has no parent")?,
    )
    .await
    .context("creating cmdline-tools directory")?;
    smol::fs::rename(staging.join("cmdline-tools"), &latest)
        .await
        .context("moving command-line tools into place")?;
    smol::fs::remove_dir_all(&staging).await.ok();

    send_line(
        tx,
        format!("Command-line tools: installed at {}", latest.display()),
    )
    .await;
    Ok(())
}

async fn accept_licenses(
    sdkmanager: &Path,
    sdk_root: &Path,
    java_home: &Path,
    tx: &channel::Sender<InstallEvent>,
) -> Result<()> {
    send_line(tx, "Licenses: accepting SDK package licenses...").await;
    let mut child = new_command(sdkmanager)
        .arg(format!("--sdk_root={}", sdk_root.display()))
        .arg("--licenses")
        .env("JAVA_HOME", java_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning sdkmanager --licenses")?;

    if let Some(mut stdin) = child.stdin.take() {
        // One "y" per license prompt; extras are ignored once the prompt
        // stream ends.
        stdin.write_all("y\n".repeat(100).as_bytes()).await.ok();
        stdin.close().await.ok();
    }

    // Drain both pipes so the child never blocks on a full pipe; the license
    // text itself is noise, so it isn't forwarded to the panel.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let drain = smol::spawn(async move {
        if let Some(mut stdout) = stdout {
            let mut sink = String::new();
            stdout.read_to_string(&mut sink).await.ok();
        }
        let mut stderr_text = String::new();
        if let Some(mut stderr) = stderr {
            stderr.read_to_string(&mut stderr_text).await.ok();
        }
        stderr_text
    });

    let status = child
        .status()
        .await
        .context("waiting for sdkmanager --licenses")?;
    let stderr_text = drain.await;
    if status.success() {
        send_line(tx, "Licenses: accepted").await;
        Ok(())
    } else {
        bail!(
            "sdkmanager --licenses exited with {status}: {}",
            stderr_text.trim()
        );
    }
}

async fn install_sdk_packages(
    sdkmanager: &Path,
    sdk_root: &Path,
    java_home: &Path,
    tx: &channel::Sender<InstallEvent>,
) -> Result<()> {
    send_line(
        tx,
        format!("SDK packages: installing {}...", SDK_PACKAGES.join(", ")),
    )
    .await;
    let mut child = new_command(sdkmanager)
        .arg(format!("--sdk_root={}", sdk_root.display()))
        .args(SDK_PACKAGES)
        .env("JAVA_HOME", java_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning sdkmanager")?;

    let stderr = child.stderr.take();
    let stderr_task = smol::spawn(async move {
        let mut collected = String::new();
        if let Some(mut stderr) = stderr {
            stderr.read_to_string(&mut collected).await.ok();
        }
        collected
    });

    if let Some(stdout) = child.stdout.take() {
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines.next().await {
            let Ok(line) = line else { break };
            // sdkmanager redraws a `[==== ] NN%` bar with carriage returns;
            // forward only the final state of each redraw to keep the panel
            // log readable.
            let line = line.rsplit('\r').next().unwrap_or(&line).trim().to_string();
            if !line.is_empty() {
                send_line(tx, line).await;
            }
        }
    }

    let status = child.status().await.context("waiting for sdkmanager")?;
    let stderr_text = stderr_task.await;
    if status.success() {
        send_line(tx, "SDK packages: installed").await;
        Ok(())
    } else {
        bail!("sdkmanager exited with {status}: {}", stderr_text.trim());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_env_empty_without_status() {
        assert!(build_env(None).is_empty());
    }

    #[test]
    fn build_env_skips_system_components() {
        let status = ToolchainStatus {
            jdk: ComponentStatus::System(PathBuf::from("/opt/jdk")),
            sdk: ComponentStatus::System(PathBuf::from("/opt/sdk")),
            platform_tools: ComponentStatus::System(PathBuf::from("/opt/sdk/platform-tools/adb")),
            licenses: ComponentStatus::Missing,
        };
        assert!(build_env(Some(&status)).is_empty());
    }

    #[test]
    fn build_env_exports_managed_paths() {
        let status = ToolchainStatus {
            jdk: ComponentStatus::Managed(PathBuf::from("/managed/jdk/home")),
            sdk: ComponentStatus::Managed(PathBuf::from("/managed/sdk")),
            platform_tools: ComponentStatus::Managed(PathBuf::from(
                "/managed/sdk/platform-tools/adb",
            )),
            licenses: ComponentStatus::Managed(PathBuf::from(
                "/managed/sdk/licenses/android-sdk-license",
            )),
        };
        let env = build_env(Some(&status));
        let get = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
        assert_eq!(get("JAVA_HOME"), Some("/managed/jdk/home"));
        assert_eq!(get("ANDROID_HOME"), Some("/managed/sdk"));
        assert_eq!(get("ANDROID_SDK_ROOT"), Some("/managed/sdk"));
        let path = get("PATH").expect("PATH should be exported");
        assert!(path.starts_with("/managed/jdk/home/bin:/managed/sdk/platform-tools"));
    }

    #[test]
    fn all_present_requires_every_component() {
        let mut status = ToolchainStatus {
            jdk: ComponentStatus::System(PathBuf::from("/opt/jdk")),
            sdk: ComponentStatus::Managed(PathBuf::from("/managed/sdk")),
            platform_tools: ComponentStatus::Managed(PathBuf::from("/managed/adb")),
            licenses: ComponentStatus::Managed(PathBuf::from("/managed/license")),
        };
        assert!(status.all_present());
        status.licenses = ComponentStatus::Missing;
        assert!(!status.all_present());
    }
}
