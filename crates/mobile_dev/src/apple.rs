//! Apple platform support: iOS simulators, physical devices, and Xcode
//! toolchain detection for the mobile_dev panel.
//!
//! Like [`crate::adb`], everything shells out to Apple's own tooling
//! (`xcrun simctl`, `xcrun devicectl`, `xcode-select`, `xcodebuild`) rather
//! than reimplementing any device protocol. Callers gate on
//! `cfg!(target_os = "macos")`; off macOS these commands don't exist and the
//! panel never invokes this module.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use collections::HashMap;
use futures::AsyncReadExt as _;
use futures::io::{AsyncBufReadExt, BufReader};
use futures::stream::Stream;
use gpui::{BackgroundExecutor, SharedString};
use serde::Deserialize;
use smol::channel;
use smol::stream::StreamExt as _;
use util::command::{Stdio, new_command};

use crate::toolchain::{ComponentStatus, InstallEvent, InstallOutcome, send_line};

/// A simulator known to CoreSimulator or a physical device known to
/// CoreDevice.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct AppleDevice {
    /// CoreSimulator / CoreDevice UDID; what `expo run:ios --device` and
    /// `simctl` commands address the device by.
    pub udid: SharedString,
    pub name: SharedString,
    pub kind: AppleDeviceKind,
    pub state: AppleDeviceState,
    /// OS the device runs, e.g. "iOS 26.5".
    pub os_version: Option<SharedString>,
}

impl AppleDevice {
    /// Convenience: pretty label for UI display.
    pub fn label(&self) -> SharedString {
        match &self.os_version {
            Some(os) => SharedString::from(format!("{} ({os})", self.name)),
            None => self.name.clone(),
        }
    }

    /// Whether a build can target the device right now. Shutdown simulators
    /// count because `expo run:ios` boots them on demand.
    pub fn is_usable(&self) -> bool {
        match self.kind {
            AppleDeviceKind::Simulator => matches!(
                self.state,
                AppleDeviceState::Booted | AppleDeviceState::Shutdown
            ),
            AppleDeviceKind::Physical => matches!(self.state, AppleDeviceState::Connected),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AppleDeviceKind {
    Simulator,
    Physical,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AppleDeviceState {
    /// Simulator is running.
    Booted,
    /// Simulator exists but is not running; bootable on demand.
    Shutdown,
    /// Physical device is paired and reachable.
    Connected,
    /// Physical device known to Xcode but not currently reachable.
    Unavailable,
    Other,
}

/// One-shot combined listing: physical devices first, then simulators. A
/// `devicectl` failure downgrades to "no physical devices" because older
/// Xcodes ship without it, while a `simctl` failure is a real error (no
/// usable Xcode at all).
pub async fn list_devices() -> Result<Vec<AppleDevice>> {
    let simulators = list_simulators().await?;
    let mut devices = match list_physical_devices().await {
        Ok(devices) => devices,
        Err(err) => {
            log::debug!("mobile_dev: devicectl listing failed: {err:#}");
            Vec::new()
        }
    };
    devices.extend(relevant_simulators(simulators));
    Ok(devices)
}

#[derive(Deserialize)]
struct SimctlList {
    devices: HashMap<String, Vec<SimctlDevice>>,
}

#[derive(Deserialize)]
struct SimctlDevice {
    name: String,
    udid: String,
    state: String,
    #[serde(default, rename = "isAvailable")]
    is_available: bool,
}

/// All available iOS simulators, from `simctl list devices available --json`.
pub async fn list_simulators() -> Result<Vec<AppleDevice>> {
    let output = new_command("xcrun")
        .arg("simctl")
        .arg("list")
        .arg("devices")
        .arg("available")
        .arg("--json")
        .output()
        .await
        .context("running `xcrun simctl list`; is Xcode installed?")?;
    if !output.status.success() {
        bail!(
            "simctl list failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_simulators(&String::from_utf8_lossy(&output.stdout))
}

pub(crate) fn parse_simulators(json: &str) -> Result<Vec<AppleDevice>> {
    let list: SimctlList = serde_json::from_str(json).context("parsing simctl device list")?;
    let mut devices = Vec::new();
    for (runtime_identifier, simulators) in list.devices {
        let Some((os_name, version)) = parse_runtime_identifier(&runtime_identifier) else {
            continue;
        };
        if os_name != "iOS" {
            continue;
        }
        let os_version = SharedString::from(format!("{os_name} {version}"));
        for simulator in simulators {
            if !simulator.is_available {
                continue;
            }
            let state = match simulator.state.as_str() {
                "Booted" => AppleDeviceState::Booted,
                "Shutdown" => AppleDeviceState::Shutdown,
                _ => AppleDeviceState::Other,
            };
            devices.push(AppleDevice {
                udid: SharedString::from(simulator.udid),
                name: SharedString::from(simulator.name),
                kind: AppleDeviceKind::Simulator,
                state,
                os_version: Some(os_version.clone()),
            });
        }
    }
    Ok(devices)
}

/// `com.apple.CoreSimulator.SimRuntime.iOS-26-5` -> `("iOS", "26.5")`.
fn parse_runtime_identifier(identifier: &str) -> Option<(String, String)> {
    let tail = identifier.rsplit('.').next()?;
    let (os, version) = tail.split_once('-')?;
    Some((os.to_string(), version.replace('-', ".")))
}

/// The full simulator roster can run to dozens of entries across installed
/// runtimes. Keep the list scannable: every booted simulator, plus the whole
/// device set of the newest installed iOS runtime, booted first.
pub(crate) fn relevant_simulators(mut simulators: Vec<AppleDevice>) -> Vec<AppleDevice> {
    let newest = simulators
        .iter()
        .filter_map(|device| device.os_version.clone())
        .max_by_key(|version| version_sort_key(version));
    simulators
        .retain(|device| device.state == AppleDeviceState::Booted || device.os_version == newest);
    simulators.sort_by_key(|device| {
        (
            device.state != AppleDeviceState::Booted,
            device.name.clone(),
        )
    });
    simulators
}

fn version_sort_key(version: &str) -> (u32, u32) {
    let digits = version.trim_start_matches(|c: char| !c.is_ascii_digit());
    let mut parts = digits.split('.');
    let major = parts.next().and_then(|part| part.parse().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|part| part.parse().ok()).unwrap_or(0);
    (major, minor)
}

#[derive(Deserialize)]
struct DevicectlOutput {
    result: Option<DevicectlResult>,
}

#[derive(Deserialize)]
struct DevicectlResult {
    devices: Option<Vec<DevicectlDevice>>,
}

#[derive(Deserialize)]
struct DevicectlDevice {
    #[serde(rename = "connectionProperties")]
    connection: Option<DevicectlConnection>,
    #[serde(rename = "deviceProperties")]
    device: Option<DevicectlProperties>,
    #[serde(rename = "hardwareProperties")]
    hardware: Option<DevicectlHardware>,
}

#[derive(Deserialize)]
struct DevicectlConnection {
    #[serde(rename = "tunnelState")]
    tunnel_state: Option<String>,
}

#[derive(Deserialize)]
struct DevicectlProperties {
    name: Option<String>,
    #[serde(rename = "osVersionNumber")]
    os_version: Option<String>,
}

#[derive(Deserialize)]
struct DevicectlHardware {
    udid: Option<String>,
    platform: Option<String>,
}

static DEVICECTL_OUTPUT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Physical iPhones and iPads known to CoreDevice (Xcode 15+). `devicectl`
/// only writes structured output to a file, so round-trip through the temp
/// dir; the counter keeps concurrent panels (one per workspace window) from
/// clobbering each other's file.
pub async fn list_physical_devices() -> Result<Vec<AppleDevice>> {
    let json_path = std::env::temp_dir().join(format!(
        "lathe-devicectl-{}-{}.json",
        std::process::id(),
        DEVICECTL_OUTPUT_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let output = new_command("xcrun")
        .arg("devicectl")
        .arg("list")
        .arg("devices")
        .arg("--json-output")
        .arg(&json_path)
        .output()
        .await
        .context("running `xcrun devicectl list devices`")?;
    let json = smol::fs::read_to_string(&json_path).await;
    smol::fs::remove_file(&json_path).await.ok();
    if !output.status.success() {
        bail!(
            "devicectl list failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_physical_devices(&json.context("reading devicectl output file")?)
}

pub(crate) fn parse_physical_devices(json: &str) -> Result<Vec<AppleDevice>> {
    let parsed: DevicectlOutput =
        serde_json::from_str(json).context("parsing devicectl device list")?;
    let mut devices = Vec::new();
    for entry in parsed
        .result
        .and_then(|result| result.devices)
        .unwrap_or_default()
    {
        let Some(udid) = entry.hardware.as_ref().and_then(|h| h.udid.clone()) else {
            continue;
        };
        if let Some(platform) = entry.hardware.as_ref().and_then(|h| h.platform.as_deref())
            && platform != "iOS"
            && platform != "iPadOS"
        {
            continue;
        }
        let name = entry
            .device
            .as_ref()
            .and_then(|properties| properties.name.clone())
            .unwrap_or_else(|| udid.clone());
        let os_version = entry
            .device
            .as_ref()
            .and_then(|properties| properties.os_version.as_deref())
            .map(|version| SharedString::from(format!("iOS {version}")));
        let state = match entry
            .connection
            .as_ref()
            .and_then(|connection| connection.tunnel_state.as_deref())
        {
            Some("connected") => AppleDeviceState::Connected,
            _ => AppleDeviceState::Unavailable,
        };
        devices.push(AppleDevice {
            udid: SharedString::from(udid),
            name: SharedString::from(name),
            kind: AppleDeviceKind::Physical,
            state,
            os_version,
        });
    }
    Ok(devices)
}

/// Poll-based device tracker, mirroring [`crate::adb::track_devices`]. The
/// interval is longer than the ADB one because `devicectl` is noticeably
/// slower than `adb devices`.
pub fn track_devices(
    interval: Duration,
    executor: BackgroundExecutor,
) -> impl Stream<Item = Result<Vec<AppleDevice>>> {
    smol::stream::unfold(true, move |first_tick| {
        let executor = executor.clone();
        async move {
            if !first_tick {
                executor.timer(interval).await;
            }
            Some((list_devices().await, false))
        }
    })
}

/// Stream the unified log of a booted simulator, filtered to processes whose
/// name contains `process_hint` when given. Mirrors [`crate::adb::logcat`]:
/// dropping the receiver kills the child via `kill_on_drop`.
pub fn log_stream(udid: &str, process_hint: Option<&str>) -> impl Stream<Item = Result<String>> {
    let udid = udid.to_owned();
    let predicate = process_hint
        .map(|hint| format!("process CONTAINS[c] \"{}\"", hint.replace('"', "")));
    let (tx, rx) = channel::unbounded::<Result<String>>();
    smol::spawn(async move {
        let mut cmd = new_command("xcrun");
        cmd.arg("simctl")
            .arg("spawn")
            .arg(&udid)
            .arg("log")
            .arg("stream")
            .arg("--level")
            .arg("debug")
            .arg("--style")
            .arg("compact");
        if let Some(predicate) = &predicate {
            cmd.arg("--predicate").arg(predicate);
        }
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                let _ = tx
                    .send(Err(
                        anyhow::Error::from(err).context("spawning simctl log stream")
                    ))
                    .await;
                return;
            }
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = tx
                .send(Err(anyhow::anyhow!("simctl log stream had no stdout pipe")))
                .await;
            return;
        };
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines.next().await {
            let item =
                line.map_err(|e| anyhow::Error::from(e).context("reading simctl log stream"));
            if tx.send(item).await.is_err() {
                break;
            }
        }
        let _ = child.status().await;
    })
    .detach();
    rx
}

/// Boot the simulator with `udid` and bring the Simulator app to the
/// foreground. Booting an already-booted simulator is treated as success
/// (`simctl` reports "Unable to boot device in current state: Booted").
pub async fn boot_simulator(udid: &str) -> Result<()> {
    let output = new_command("xcrun")
        .arg("simctl")
        .arg("boot")
        .arg(udid)
        .output()
        .await
        .context("running `xcrun simctl boot`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.contains("current state: Booted") {
            bail!("simctl boot failed: {}", stderr.trim());
        }
    }
    // Best-effort: surface the simulator window. A failure here (e.g. no
    // window server) shouldn't fail the boot.
    new_command("open")
        .arg("-a")
        .arg("Simulator")
        .output()
        .await
        .ok();
    Ok(())
}

/// Shut down the simulator with `udid`. Shutting down an already-shutdown
/// simulator is treated as success.
pub async fn shutdown_simulator(udid: &str) -> Result<()> {
    let output = new_command("xcrun")
        .arg("simctl")
        .arg("shutdown")
        .arg(udid)
        .output()
        .await
        .context("running `xcrun simctl shutdown`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.contains("current state: Shutdown") {
            bail!("simctl shutdown failed: {}", stderr.trim());
        }
    }
    Ok(())
}

/// What the Apple toolchain probe found. Everything here is Apple-managed
/// (there is no Lathe-managed install like the Android side), so present
/// components always report [`ComponentStatus::System`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppleToolchainStatus {
    /// Full Xcode. The standalone Command Line Tools also answer
    /// `xcode-select -p` but cannot build iOS apps, so they count as missing.
    pub xcode: ComponentStatus,
    /// First line of `xcodebuild -version`, e.g. "Xcode 26.6".
    pub xcode_version: Option<SharedString>,
    /// At least one installed iOS simulator runtime.
    pub ios_runtime: ComponentStatus,
    /// CocoaPods, which `expo run:ios` invokes during prebuild.
    pub cocoapods: ComponentStatus,
}

impl AppleToolchainStatus {
    pub fn all_present(&self) -> bool {
        self.xcode.is_present() && self.ios_runtime.is_present() && self.cocoapods.is_present()
    }
}

pub async fn detect() -> AppleToolchainStatus {
    let xcode = detect_xcode().await;
    let (xcode_version, ios_runtime) = match &xcode {
        ComponentStatus::Missing => (None, ComponentStatus::Missing),
        _ => (xcodebuild_version().await, detect_ios_runtime().await),
    };
    AppleToolchainStatus {
        xcode,
        xcode_version,
        ios_runtime,
        cocoapods: detect_cocoapods(),
    }
}

async fn detect_xcode() -> ComponentStatus {
    let Ok(output) = new_command("xcode-select").arg("-p").output().await else {
        return ComponentStatus::Missing;
    };
    if !output.status.success() {
        return ComponentStatus::Missing;
    }
    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    if path.join("Platforms/iPhoneOS.platform").is_dir() {
        ComponentStatus::System(path)
    } else {
        ComponentStatus::Missing
    }
}

async fn xcodebuild_version() -> Option<SharedString> {
    let output = new_command("xcodebuild")
        .arg("-version")
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .next()
        .map(|line| SharedString::from(line.trim().to_string()))
}

#[derive(Deserialize)]
struct SimctlRuntimes {
    runtimes: Vec<SimctlRuntime>,
}

#[derive(Deserialize)]
struct SimctlRuntime {
    name: String,
    #[serde(default, rename = "isAvailable")]
    is_available: bool,
    #[serde(rename = "runtimeRoot")]
    runtime_root: Option<String>,
}

async fn detect_ios_runtime() -> ComponentStatus {
    let Ok(output) = new_command("xcrun")
        .arg("simctl")
        .arg("list")
        .arg("runtimes")
        .arg("--json")
        .output()
        .await
    else {
        return ComponentStatus::Missing;
    };
    if !output.status.success() {
        return ComponentStatus::Missing;
    }
    let Ok(parsed) = serde_json::from_slice::<SimctlRuntimes>(&output.stdout) else {
        return ComponentStatus::Missing;
    };
    parsed
        .runtimes
        .into_iter()
        .find(|runtime| runtime.is_available && runtime.name.starts_with("iOS"))
        .map(|runtime| {
            ComponentStatus::System(PathBuf::from(runtime.runtime_root.unwrap_or_default()))
        })
        .unwrap_or(ComponentStatus::Missing)
}

fn detect_cocoapods() -> ComponentStatus {
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("pod"))
                .collect()
        })
        .unwrap_or_default();
    // Homebrew's install dirs, in case Lathe was launched with a minimal
    // GUI-app PATH that doesn't include them.
    candidates.push(PathBuf::from("/opt/homebrew/bin/pod"));
    candidates.push(PathBuf::from("/usr/local/bin/pod"));
    candidates
        .into_iter()
        .find(|pod| pod.is_file())
        .map(ComponentStatus::System)
        .unwrap_or(ComponentStatus::Missing)
}

/// One running `xcodebuild -downloadPlatform iOS` plus a receiver for its
/// progress lines. Xcode itself cannot be installed this way (it comes from
/// the App Store or developer.apple.com), but the simulator runtime can.
/// Like [`crate::toolchain::InstallSession`], the download keeps running if
/// the panel is dropped mid-way so a nearly-finished multi-GB fetch isn't
/// thrown away.
pub struct RuntimeInstallSession {
    events: channel::Receiver<InstallEvent>,
}

impl RuntimeInstallSession {
    pub fn spawn() -> Self {
        let (tx, rx) = channel::unbounded::<InstallEvent>();
        smol::spawn(async move {
            let outcome = match run_runtime_install(&tx).await {
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

async fn run_runtime_install(tx: &channel::Sender<InstallEvent>) -> Result<()> {
    send_line(tx, "iOS runtime: starting download via xcodebuild...").await;
    let mut child = new_command("xcodebuild")
        .arg("-downloadPlatform")
        .arg("iOS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning xcodebuild -downloadPlatform iOS")?;

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
            // xcodebuild redraws its progress line with carriage returns;
            // forward only the final state of each redraw.
            let line = line.rsplit('\r').next().unwrap_or(&line).trim().to_string();
            if !line.is_empty() {
                send_line(tx, line).await;
            }
        }
    }

    let status = child
        .status()
        .await
        .context("waiting for xcodebuild -downloadPlatform")?;
    let stderr_text = stderr_task.await;
    if status.success() {
        send_line(tx, "iOS runtime: installed").await;
        Ok(())
    } else {
        bail!(
            "xcodebuild -downloadPlatform exited with {status}: {}",
            stderr_text.trim()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simulators_filters_non_ios_runtimes() {
        let json = r#"{
            "devices": {
                "com.apple.CoreSimulator.SimRuntime.iOS-26-5": [
                    {"name": "iPhone 17 Pro", "udid": "AAA", "state": "Shutdown", "isAvailable": true},
                    {"name": "iPhone 17", "udid": "BBB", "state": "Booted", "isAvailable": true}
                ],
                "com.apple.CoreSimulator.SimRuntime.watchOS-11-0": [
                    {"name": "Apple Watch", "udid": "CCC", "state": "Shutdown", "isAvailable": true}
                ]
            }
        }"#;
        let devices = parse_simulators(json).unwrap();
        assert_eq!(devices.len(), 2);
        assert!(devices.iter().all(|d| d.kind == AppleDeviceKind::Simulator));
        assert!(
            devices
                .iter()
                .all(|d| d.os_version.as_deref() == Some("iOS 26.5"))
        );
        let booted = devices.iter().find(|d| d.udid.as_ref() == "BBB").unwrap();
        assert_eq!(booted.state, AppleDeviceState::Booted);
    }

    #[test]
    fn parse_simulators_skips_unavailable() {
        let json = r#"{
            "devices": {
                "com.apple.CoreSimulator.SimRuntime.iOS-18-2": [
                    {"name": "iPhone 16", "udid": "AAA", "state": "Shutdown", "isAvailable": false}
                ]
            }
        }"#;
        assert!(parse_simulators(json).unwrap().is_empty());
    }

    #[test]
    fn relevant_simulators_keeps_newest_runtime_and_booted() {
        let simulator = |name: &str, os: &str, state: AppleDeviceState| AppleDevice {
            udid: SharedString::from(format!("{name}-{os}")),
            name: SharedString::from(name.to_string()),
            kind: AppleDeviceKind::Simulator,
            state,
            os_version: Some(SharedString::from(os.to_string())),
        };
        let filtered = relevant_simulators(vec![
            simulator("iPhone 16", "iOS 18.2", AppleDeviceState::Shutdown),
            simulator("iPhone 15", "iOS 18.2", AppleDeviceState::Booted),
            simulator("iPhone 17", "iOS 26.5", AppleDeviceState::Shutdown),
        ]);
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].name.as_ref(), "iPhone 15");
        assert_eq!(filtered[0].state, AppleDeviceState::Booted);
        assert_eq!(filtered[1].name.as_ref(), "iPhone 17");
    }

    #[test]
    fn version_sort_key_orders_major_versions_numerically() {
        assert!(version_sort_key("iOS 26.5") > version_sort_key("iOS 9.3"));
        assert!(version_sort_key("iOS 18.2") > version_sort_key("iOS 18.1"));
    }

    #[test]
    fn parse_physical_devices_maps_connection_state() {
        let json = r#"{
            "result": {
                "devices": [
                    {
                        "connectionProperties": {"tunnelState": "connected"},
                        "deviceProperties": {"name": "Chris's iPhone", "osVersionNumber": "26.0"},
                        "hardwareProperties": {"udid": "00008120-000A", "platform": "iOS"}
                    },
                    {
                        "connectionProperties": {"tunnelState": "disconnected"},
                        "deviceProperties": {"name": "Old iPad", "osVersionNumber": "17.7"},
                        "hardwareProperties": {"udid": "00008101-000B", "platform": "iPadOS"}
                    },
                    {
                        "connectionProperties": {"tunnelState": "connected"},
                        "deviceProperties": {"name": "Watch"},
                        "hardwareProperties": {"udid": "00008301-000C", "platform": "watchOS"}
                    }
                ]
            }
        }"#;
        let devices = parse_physical_devices(json).unwrap();
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].state, AppleDeviceState::Connected);
        assert_eq!(devices[0].label().as_ref(), "Chris's iPhone (iOS 26.0)");
        assert!(devices[0].is_usable());
        assert_eq!(devices[1].state, AppleDeviceState::Unavailable);
        assert!(!devices[1].is_usable());
    }

    #[test]
    fn parse_physical_devices_tolerates_empty_result() {
        assert!(parse_physical_devices(r#"{"result": {}}"#).unwrap().is_empty());
        assert!(parse_physical_devices(r#"{}"#).unwrap().is_empty());
    }

    #[test]
    fn simulator_label_includes_runtime() {
        let device = AppleDevice {
            udid: SharedString::from("AAA"),
            name: SharedString::from("iPhone 17 Pro"),
            kind: AppleDeviceKind::Simulator,
            state: AppleDeviceState::Shutdown,
            os_version: Some(SharedString::from("iOS 26.5")),
        };
        assert_eq!(device.label().as_ref(), "iPhone 17 Pro (iOS 26.5)");
        assert!(device.is_usable());
    }
}
