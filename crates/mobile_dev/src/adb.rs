//! Wrapper around the `adb` command-line tool.
//!
//! Lathe's mobile_dev panel talks to physical Android devices through this
//! module. We deliberately call into the standard `adb` binary rather than
//! reimplement the host-side ADB protocol, because Google's protocol can and
//! does change between platform-tools releases.
//!
//! All functions here spawn `adb` as a subprocess. They expect `adb` to be on
//! PATH (the bundled toolchain installed by Phase 3 will guarantee this; until
//! then a user-installed Android SDK is required).

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::io::{AsyncBufReadExt, BufReader};
use futures::stream::Stream;
use gpui::{BackgroundExecutor, SharedString};
use smol::channel;
use smol::stream::StreamExt as _;
use util::command::{Stdio, new_command};

/// Prefer the Lathe-managed platform-tools adb (consistent with the env the
/// panel injects into builds); fall back to whatever `adb` is on PATH.
fn adb_program() -> std::path::PathBuf {
    crate::toolchain::managed_adb_path().unwrap_or_else(|| std::path::PathBuf::from("adb"))
}

/// A device returned by `adb devices -l`.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct AdbDevice {
    /// The opaque device identifier `adb` uses to address the device. For
    /// USB devices this is the hardware serial; for wireless devices it is
    /// the `ip:port` pair the host connected to.
    pub serial: SharedString,
    pub state: AdbDeviceState,
    /// The raw `model:` value `adb devices -l` reports (underscored, e.g.
    /// `Pixel_7`). Kept verbatim because it is also the name the Expo CLI
    /// matches `--device` against; [`AdbDevice::label`] prettifies it.
    pub model: Option<SharedString>,
    /// For emulators, the AVD name from `adb -s <serial> emu avd name`.
    pub avd_name: Option<SharedString>,
    pub transport: AdbTransport,
}

impl AdbDevice {
    /// Convenience: pretty label for UI display.
    pub fn label(&self) -> SharedString {
        // An emulator's model (`sdk_gphone64_arm64`) says nothing about which
        // AVD it is, so prefer the AVD name when we have one.
        if let Some(avd) = &self.avd_name {
            return SharedString::from(format!("{avd} ({})", self.serial));
        }
        match &self.model {
            Some(model) => {
                SharedString::from(format!("{} ({})", model.replace('_', " "), self.serial))
            }
            None => self.serial.clone(),
        }
    }

    /// Whether this is a local emulator rather than a physical device.
    pub fn is_emulator(&self) -> bool {
        self.serial.starts_with("emulator-")
    }

    /// What to pass to `expo run:android --device`. The Expo CLI resolves that
    /// flag against its OWN device names, never the adb serial: the AVD name
    /// for an emulator, the raw `model:` value for a physical device (see
    /// `getAttachedDevicesAsync` in @expo/cli). Passing the serial fails with
    /// "Could not find device with name: emulator-5554".
    pub fn expo_device_name(&self) -> SharedString {
        self.avd_name
            .clone()
            .or_else(|| self.model.clone())
            .unwrap_or_else(|| self.serial.clone())
    }

    pub fn is_usable(&self) -> bool {
        matches!(self.state, AdbDeviceState::Online)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AdbDeviceState {
    /// `device` in `adb devices -l`. Ready to receive commands.
    Online,
    /// `offline`. Authorization was granted but the connection is dropped.
    Offline,
    /// `unauthorized`. The phone has not granted the host's ADB key yet.
    /// The user must accept the prompt that appears on the device.
    Unauthorized,
    /// Anything else `adb` reports (e.g. `bootloader`, `recovery`, `sideload`).
    Other,
}

impl AdbDeviceState {
    fn from_token(token: &str) -> Self {
        match token {
            "device" => Self::Online,
            "offline" => Self::Offline,
            "unauthorized" => Self::Unauthorized,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AdbTransport {
    /// Plugged in over USB.
    Usb,
    /// Connected via wireless debugging. Serial looks like `192.168.1.10:5555`.
    Wireless,
}

impl AdbTransport {
    fn from_serial(serial: &str) -> Self {
        if serial.contains(':') {
            Self::Wireless
        } else {
            Self::Usb
        }
    }
}

/// One-shot device listing. Returns whatever `adb devices -l` knows about
/// right now. For continuous tracking use [`track_devices`].
pub async fn list_devices() -> Result<Vec<AdbDevice>> {
    let output = new_command(adb_program())
        .arg("devices")
        .arg("-l")
        .output()
        .await
        .context("running `adb devices -l`; is adb on PATH?")?;
    if !output.status.success() {
        bail!(
            "adb devices failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let mut devices = parse_devices(&String::from_utf8_lossy(&output.stdout));
    for device in &mut devices {
        if device.is_emulator() && device.is_usable() {
            device.avd_name = avd_name(&device.serial).await;
        }
    }
    Ok(devices)
}

/// The AVD an emulator was booted from, via `adb -s <serial> emu avd name`.
/// `None` when the console query fails (a still-booting emulator answers
/// nothing useful), so callers just fall back to the serial.
async fn avd_name(serial: &str) -> Option<SharedString> {
    let output = new_command(adb_program())
        .args(["-s", serial, "emu", "avd", "name"])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    // Output is the name followed by adb's own "OK" acknowledgement line.
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && *line != "OK")
        .map(|name| SharedString::from(name.to_string()))
}

/// Parse the output of `adb devices -l`. Public for tests.
pub(crate) fn parse_devices(output: &str) -> Vec<AdbDevice> {
    let mut devices = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty()
            || line.starts_with("List of devices")
            || line.starts_with('*')
            || line.starts_with('-')
        {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(serial_raw) = parts.next() else {
            continue;
        };
        let Some(state_token) = parts.next() else {
            continue;
        };

        let serial = SharedString::from(serial_raw.to_string());
        let state = AdbDeviceState::from_token(state_token);
        let transport = AdbTransport::from_serial(serial_raw);

        let mut model = None;
        for tag in parts {
            if let Some(value) = tag.strip_prefix("model:") {
                model = Some(SharedString::from(value.to_string()));
            }
        }

        devices.push(AdbDevice {
            serial,
            state,
            model,
            avd_name: None,
            transport,
        });
    }
    devices
}

/// Poll-based device tracker. Emits the current device list every time it
/// changes, plus once at startup. Polling is cheap (a `Vec<AdbDevice>` per
/// tick), two-second cadence is fast enough for human-perceptible hot-plug
/// detection, and the parser is the same as one-shot listing.
///
/// We chose polling over `adb` server's `host:track-devices` socket because
/// the wire protocol changes more often than the CLI output and adds no
/// fidelity for our use case.
pub fn track_devices(
    interval: Duration,
    executor: BackgroundExecutor,
) -> impl Stream<Item = Result<Vec<AdbDevice>>> {
    smol::stream::unfold(
        (None::<Vec<AdbDevice>>, true),
        move |(previous, first_tick)| {
            let executor = executor.clone();
            async move {
                if !first_tick {
                    executor.timer(interval).await;
                }
                let current = list_devices().await;
                let (emit_value, next_previous) = match (current, previous) {
                    (Ok(now), None) => {
                        let cloned = now.clone();
                        (Ok(now), Some(cloned))
                    }
                    (Ok(now), Some(prev)) if now == prev => {
                        let cloned = now.clone();
                        (Ok(now), Some(cloned))
                    }
                    (Ok(now), Some(_)) => {
                        let cloned = now.clone();
                        (Ok(now), Some(cloned))
                    }
                    (Err(err), prev) => (Err(err), prev),
                };
                Some((emit_value, (next_previous, false)))
            }
        },
    )
}

/// Install (or reinstall) an APK on the given device. Equivalent to
/// `adb -s <serial> install -r <apk_path>`.
pub async fn install(serial: &str, apk_path: &Path) -> Result<()> {
    let output = new_command(adb_program())
        .arg("-s")
        .arg(serial)
        .arg("install")
        .arg("-r")
        .arg(apk_path)
        .output()
        .await
        .with_context(|| format!("installing {} on {serial}", apk_path.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!(
            "adb install failed:\nstdout: {}\nstderr: {}",
            stdout.trim(),
            stderr.trim()
        );
    }
    Ok(())
}

/// Launch the given package's main activity. `package` is the Android
/// package id (e.g. `com.paterschris.unbenched`); `activity` is the
/// fully-qualified activity class, typically `<package>.MainActivity`.
pub async fn launch(serial: &str, package: &str, activity: &str) -> Result<()> {
    let component = format!("{package}/{activity}");
    let output = new_command(adb_program())
        .arg("-s")
        .arg(serial)
        .arg("shell")
        .arg("am")
        .arg("start")
        .arg("-n")
        .arg(&component)
        .output()
        .await
        .with_context(|| format!("launching {component} on {serial}"))?;
    if !output.status.success() {
        bail!(
            "adb am start failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Look up the PID of `package` on the device, or `None` if it isn't running.
pub async fn pid_of(serial: &str, package: &str) -> Result<Option<u32>> {
    let output = new_command(adb_program())
        .arg("-s")
        .arg(serial)
        .arg("shell")
        .arg("pidof")
        .arg("-s")
        .arg(package)
        .output()
        .await
        .with_context(|| format!("pidof {package} on {serial}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.trim().parse::<u32>().ok())
}

/// Stream logcat output line by line. If `pid_filter` is set the output is
/// limited to that process (so you see only your own app's logs).
///
/// The returned stream terminates when the underlying `adb logcat` process
/// exits, which happens when ADB drops the device. Dropping the receiver
/// closes the channel, which causes the background task to exit and the
/// child process to be killed (via `kill_on_drop`).
pub fn logcat(serial: &str, pid_filter: Option<u32>) -> impl Stream<Item = Result<String>> {
    let serial = serial.to_owned();
    let (tx, rx) = channel::unbounded::<Result<String>>();
    smol::spawn(async move {
        let mut cmd = new_command(adb_program());
        cmd.arg("-s")
            .arg(&serial)
            .arg("logcat")
            .arg("-v")
            .arg("threadtime");
        if let Some(pid) = pid_filter {
            cmd.arg(format!("--pid={pid}"));
        }
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                let _ = tx
                    .send(Err(anyhow::Error::from(err).context("spawning adb logcat")))
                    .await;
                return;
            }
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = tx
                .send(Err(anyhow::anyhow!("adb logcat had no stdout pipe")))
                .await;
            return;
        };
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines.next().await {
            let item = line.map_err(|e| anyhow::Error::from(e).context("reading adb logcat"));
            if tx.send(item).await.is_err() {
                break;
            }
        }
        let _ = child.status().await;
    })
    .detach();
    rx
}

/// Pair a wireless device (Android 11+). `addr` is the IP:port shown on the
/// phone's pairing screen, `code` is the six-digit code.
pub async fn pair(addr: &str, code: &str) -> Result<()> {
    let output = new_command(adb_program())
        .arg("pair")
        .arg(addr)
        .arg(code)
        .output()
        .await
        .context("running `adb pair`")?;
    if !output.status.success() {
        bail!(
            "adb pair failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Connect to an already-paired wireless device. After a phone reboot or
/// network blip you have to reconnect with the *non-pair* port.
pub async fn connect(addr: &str) -> Result<()> {
    let output = new_command(adb_program())
        .arg("connect")
        .arg(addr)
        .output()
        .await
        .context("running `adb connect`")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || stdout.contains("failed") || stdout.contains("cannot") {
        bail!(
            "adb connect failed: {}",
            stdout.trim().lines().next().unwrap_or("(no stdout)")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_output() {
        let s = "List of devices attached\n\n";
        assert!(parse_devices(s).is_empty());
    }

    #[test]
    fn parse_single_usb_device() {
        let s = "List of devices attached\n\
                 ABC123XYZ              device usb:1-3 product:cheetah model:Pixel_7 device:cheetah transport_id:1\n";
        let devices = parse_devices(s);
        assert_eq!(devices.len(), 1);
        let d = &devices[0];
        assert_eq!(d.serial.as_ref(), "ABC123XYZ");
        assert_eq!(d.state, AdbDeviceState::Online);
        assert_eq!(d.transport, AdbTransport::Usb);
        assert_eq!(d.model.as_deref(), Some("Pixel_7"));
    }

    #[test]
    fn parse_wireless_device() {
        let s = "List of devices attached\n\
                 192.168.1.42:5555      device product:cheetah model:Pixel_7 device:cheetah transport_id:5\n";
        let devices = parse_devices(s);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].transport, AdbTransport::Wireless);
        assert_eq!(devices[0].state, AdbDeviceState::Online);
    }

    #[test]
    fn parse_unauthorized_state() {
        let s = "List of devices attached\n\
                 ABC123XYZ              unauthorized usb:1-3\n";
        let devices = parse_devices(s);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].state, AdbDeviceState::Unauthorized);
        assert!(!devices[0].is_usable());
    }

    #[test]
    fn parse_skips_daemon_messages() {
        let s = "* daemon not running; starting now at tcp:5037\n\
                 * daemon started successfully\n\
                 List of devices attached\n\
                 ABC123XYZ              device model:Pixel_8\n";
        let devices = parse_devices(s);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].model.as_deref(), Some("Pixel_8"));
    }

    #[test]
    fn parse_multiple_devices() {
        let s = "List of devices attached\n\
                 ABC                    device model:Pixel_7\n\
                 192.168.1.10:5555      device model:Pixel_8\n\
                 XYZ                    unauthorized\n";
        let devices = parse_devices(s);
        assert_eq!(devices.len(), 3);
        assert_eq!(devices[0].transport, AdbTransport::Usb);
        assert_eq!(devices[1].transport, AdbTransport::Wireless);
        assert_eq!(devices[2].state, AdbDeviceState::Unauthorized);
    }

    #[test]
    fn device_label_falls_back_to_serial() {
        let d = AdbDevice {
            serial: SharedString::from("ABC"),
            state: AdbDeviceState::Online,
            model: None,
            avd_name: None,
            transport: AdbTransport::Usb,
        };
        assert_eq!(d.label().as_ref(), "ABC");
    }

    #[test]
    fn device_label_includes_model() {
        let d = AdbDevice {
            serial: SharedString::from("ABC"),
            state: AdbDeviceState::Online,
            model: Some(SharedString::from("Pixel_8")),
            avd_name: None,
            transport: AdbTransport::Usb,
        };
        assert_eq!(d.label().as_ref(), "Pixel 8 (ABC)");
    }

    #[test]
    fn emulator_prefers_avd_name_over_model() {
        let d = AdbDevice {
            serial: SharedString::from("emulator-5554"),
            state: AdbDeviceState::Online,
            model: Some(SharedString::from("sdk_gphone64_arm64")),
            avd_name: Some(SharedString::from("Lathe_Pixel_API35")),
            transport: AdbTransport::Usb,
        };
        assert!(d.is_emulator());
        assert_eq!(d.label().as_ref(), "Lathe_Pixel_API35 (emulator-5554)");
        // Never the serial: `expo run:android --device emulator-5554` fails.
        assert_eq!(d.expo_device_name().as_ref(), "Lathe_Pixel_API35");
    }

    #[test]
    fn physical_device_expo_name_is_the_raw_model() {
        let d = AdbDevice {
            serial: SharedString::from("ABC123XYZ"),
            state: AdbDeviceState::Online,
            model: Some(SharedString::from("Pixel_7")),
            avd_name: None,
            transport: AdbTransport::Usb,
        };
        assert!(!d.is_emulator());
        assert_eq!(d.expo_device_name().as_ref(), "Pixel_7");
    }

    #[test]
    fn expo_name_falls_back_to_serial_without_model() {
        let d = AdbDevice {
            serial: SharedString::from("ABC"),
            state: AdbDeviceState::Online,
            model: None,
            avd_name: None,
            transport: AdbTransport::Usb,
        };
        assert_eq!(d.expo_device_name().as_ref(), "ABC");
    }
}
