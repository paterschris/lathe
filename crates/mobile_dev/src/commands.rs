//! The catalog of dev-workflow commands the mobile_dev panel can run, and how
//! each one resolves to a concrete process for the detected project.
//!
//! Every method the panel offers goes through [`ResolvedCommand`], which the
//! panel runs in an interactive terminal tab. Resolution is project-kind
//! aware: an Expo project runs `expo run:*`, a bare React Native project runs
//! the React Native CLI directly, and both surface the scripts the project
//! declares in its own `package.json`.

use std::path::PathBuf;

use crate::mobile_project::{MobileProject, ProjectKind};
use crate::toolchain;

/// A fully-resolved command ready to spawn. `wants_android_env` asks the panel
/// to inject the managed Android toolchain env (JAVA_HOME/ANDROID_HOME/PATH)
/// so gradle, adb, and the emulator work without shell setup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedCommand {
    /// Short human label for the output pane header.
    pub label: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub wants_android_env: bool,
}

impl ResolvedCommand {
    fn npx(label: impl Into<String>, args: Vec<String>, cwd: PathBuf, android: bool) -> Self {
        Self {
            label: label.into(),
            program: "npx".to_string(),
            args,
            cwd,
            wants_android_env: android,
        }
    }
}

/// Run a `package.json` script by name (no extra positional args; scripts that
/// require them print their own usage into the output pane, which is itself
/// useful feedback).
pub fn run_script(project: &MobileProject, name: &str, extra_args: &[String]) -> ResolvedCommand {
    let mut args = project.package_manager.run_script_args(name);
    args.extend(extra_args.iter().cloned());
    let label = if extra_args.is_empty() {
        format!("{} {}", project.package_manager.program(), name)
    } else {
        format!(
            "{} {} {}",
            project.package_manager.program(),
            name,
            extra_args.join(" ")
        )
    };
    ResolvedCommand {
        label,
        program: project.package_manager.program().to_string(),
        args,
        cwd: project.root.clone(),
        // Scripts may invoke gradle; the env is a harmless superset for the
        // iOS/JS ones.
        wants_android_env: true,
    }
}

/// Install JS dependencies (`<pm> install`). Run as a prerequisite before any
/// command when `node_modules` is missing.
pub fn install_deps(project: &MobileProject) -> ResolvedCommand {
    ResolvedCommand {
        label: format!("{} install", project.package_manager.program()),
        program: project.package_manager.program().to_string(),
        args: vec!["install".to_string()],
        cwd: project.root.clone(),
        wants_android_env: false,
    }
}

/// Start the Metro bundler (`<pm> start`). Long-lived; the panel keeps the
/// session alive until the user stops it.
pub fn metro(project: &MobileProject, localhost: bool) -> ResolvedCommand {
    let mut args = project.package_manager.run_script_args("start");
    if localhost {
        // Expo serves the dev-server URL on localhost instead of the host's LAN
        // IP, so an Android emulator (which can't reach the LAN IP but does have
        // `adb reverse tcp:8081`) can load the app in Expo Go.
        args.push("--localhost".to_string());
    }
    ResolvedCommand {
        label: if localhost {
            "Metro (localhost)".to_string()
        } else {
            "Metro bundler".to_string()
        },
        program: project.package_manager.program().to_string(),
        args,
        cwd: project.root.clone(),
        wants_android_env: false,
    }
}

/// Install iOS CocoaPods using the project's own method: `bundle exec pod
/// install` when the project pins its Ruby tooling with a Gemfile (the panel
/// provisions the pinned Ruby via rbenv and runs [`bundle_install`] first),
/// otherwise a direct `pod install` against a system/Homebrew CocoaPods.
pub fn pod_install(project: &MobileProject) -> ResolvedCommand {
    if project.has_gemfile {
        bundle_command(
            project,
            "bundle exec pod install",
            vec!["exec".to_string(), "pod".to_string(), "install".to_string()],
        )
    } else {
        ResolvedCommand {
            label: "pod install".to_string(),
            program: pod_program(),
            args: vec!["install".to_string()],
            cwd: project.root.join("ios"),
            wants_android_env: false,
        }
    }
}

/// `bundle install`, run before `bundle exec pod install` so the CocoaPods gem
/// the Gemfile pins is present.
pub fn bundle_install(project: &MobileProject) -> ResolvedCommand {
    bundle_command(project, "bundle install", vec!["install".to_string()])
}

/// Build a `bundle <args>` command in `ios/`, wrapped in `rbenv exec` when the
/// project pins a Ruby version so Bundler runs under that Ruby (rbenv reads the
/// project's `.ruby-version`). Without a pin, `bundle` runs directly. Bundler
/// walks up from `ios/` to find a root Gemfile.
fn bundle_command(project: &MobileProject, label: &str, bundle_args: Vec<String>) -> ResolvedCommand {
    let ios = project.root.join("ios");
    if project.ruby_version.is_some() {
        let mut args = vec!["exec".to_string(), "bundle".to_string()];
        args.extend(bundle_args);
        ResolvedCommand {
            label: label.to_string(),
            program: rbenv_program(),
            args,
            cwd: ios,
            wants_android_env: false,
        }
    } else {
        ResolvedCommand {
            label: label.to_string(),
            program: "bundle".to_string(),
            args: bundle_args,
            cwd: ios,
            wants_android_env: false,
        }
    }
}

/// `brew install rbenv ruby-build` when rbenv is missing (and Homebrew is
/// present); `None` when rbenv already exists or there's no Homebrew.
pub fn install_rbenv(project: &MobileProject) -> Option<ResolvedCommand> {
    if which("rbenv").is_some() {
        return None;
    }
    let brew = which("brew")?;
    Some(ResolvedCommand {
        label: "brew install rbenv".to_string(),
        program: brew.to_string_lossy().into_owned(),
        args: vec![
            "install".to_string(),
            "rbenv".to_string(),
            "ruby-build".to_string(),
        ],
        cwd: project.root.clone(),
        wants_android_env: false,
    })
}

/// `rbenv install -s <version>` to build the project's pinned Ruby (`-s` skips
/// when already installed). Requires rbenv + ruby-build (see [`install_rbenv`]).
pub fn install_ruby(project: &MobileProject, version: &str) -> ResolvedCommand {
    ResolvedCommand {
        label: format!("rbenv install {version}"),
        program: rbenv_program(),
        args: vec!["install".to_string(), "-s".to_string(), version.to_string()],
        cwd: project.root.clone(),
        wants_android_env: false,
    }
}

/// Whether the pinned Ruby is already built under rbenv.
pub fn ruby_installed(version: &str) -> bool {
    std::env::var_os("HOME")
        .map(|home| {
            PathBuf::from(home)
                .join(format!(".rbenv/versions/{version}/bin/ruby"))
                .is_file()
        })
        .unwrap_or(false)
}

fn rbenv_program() -> String {
    which("rbenv")
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/opt/homebrew/bin/rbenv".to_string())
}

/// `brew install cocoapods`, or `None` when nothing needs doing: CocoaPods is
/// already available, or there's no Homebrew to install it with.
pub fn install_cocoapods(project: &MobileProject) -> Option<ResolvedCommand> {
    if which("pod").is_some() {
        return None;
    }
    let brew = which("brew")?;
    Some(ResolvedCommand {
        label: "brew install cocoapods".to_string(),
        program: brew.to_string_lossy().into_owned(),
        args: vec!["install".to_string(), "cocoapods".to_string()],
        cwd: project.root.clone(),
        wants_android_env: false,
    })
}

fn pod_program() -> String {
    which("pod")
        .map(|path| path.to_string_lossy().into_owned())
        // Homebrew's Apple-Silicon location; correct once a preceding
        // `brew install cocoapods` chain step has run.
        .unwrap_or_else(|| "/opt/homebrew/bin/pod".to_string())
}

/// Locate `binary` on `PATH`, plus the Homebrew bin dirs (which a GUI-launched
/// app often doesn't inherit on `PATH`).
fn which(binary: &str) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(binary))
                .collect()
        })
        .unwrap_or_default();
    candidates.push(PathBuf::from(format!("/opt/homebrew/bin/{binary}")));
    candidates.push(PathBuf::from(format!("/usr/local/bin/{binary}")));
    candidates.into_iter().find(|path| path.is_file())
}

/// Launch the Spotlight sidecar (`npx @spotlightjs/spotlight`) for live Sentry
/// monitoring. Long-lived.
pub fn spotlight(project: &MobileProject) -> ResolvedCommand {
    ResolvedCommand::npx(
        "Spotlight",
        vec!["@spotlightjs/spotlight".to_string()],
        project.root.clone(),
        false,
    )
}

/// `adb reverse tcp:8081 tcp:8081`, so a USB-attached Android device can reach
/// the Metro dev server on the host. Optionally scoped to `serial`.
pub fn adb_reverse(project: &MobileProject, serial: Option<&str>) -> ResolvedCommand {
    let program = toolchain::managed_adb_path()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "adb".to_string());
    let mut args = Vec::new();
    if let Some(serial) = serial {
        args.push("-s".to_string());
        args.push(serial.to_string());
    }
    args.extend([
        "reverse".to_string(),
        "tcp:8081".to_string(),
        "tcp:8081".to_string(),
    ]);
    ResolvedCommand {
        label: "adb reverse".to_string(),
        program,
        args,
        cwd: project.root.clone(),
        wants_android_env: true,
    }
}

/// Build and run on iOS: `expo run:ios` for Expo, `react-native run-ios` for
/// bare RN. `scheme` picks the Xcode scheme (bare RN only); `device_udid`
/// targets a specific simulator/device.
pub fn run_ios(
    project: &MobileProject,
    scheme: Option<&str>,
    device_udid: Option<&str>,
) -> ResolvedCommand {
    match project.kind {
        ProjectKind::Expo => {
            let mut args = vec!["expo".to_string(), "run:ios".to_string(), "--device".to_string()];
            if let Some(udid) = device_udid {
                args.push(udid.to_string());
            }
            ResolvedCommand::npx("Run iOS", args, project.root.clone(), false)
        }
        ProjectKind::BareReactNative => {
            let mut args = vec!["react-native".to_string(), "run-ios".to_string()];
            if let Some(scheme) = scheme {
                args.push(format!("--scheme={scheme}"));
            }
            if let Some(udid) = device_udid {
                args.push("--udid".to_string());
                args.push(udid.to_string());
            }
            ResolvedCommand::npx("Run iOS", args, project.root.clone(), false)
        }
    }
}

/// Build and run on Android: `expo run:android` for Expo, `react-native
/// run-android` for bare RN. `variant` picks the gradle build variant (bare RN
/// only); `device_serial` targets a specific device/emulator.
///
/// The two CLIs disagree on how to name a device: the RN CLI takes the adb
/// serial, while Expo matches `--device` against its own device names (AVD name
/// for an emulator, `model:` for a phone), hence `expo_device_name` alongside
/// `device_serial`. See [`crate::adb::AdbDevice::expo_device_name`].
pub fn run_android(
    project: &MobileProject,
    variant: Option<&str>,
    device_serial: Option<&str>,
    expo_device_name: Option<&str>,
) -> ResolvedCommand {
    match project.kind {
        ProjectKind::Expo => {
            let mut args = vec!["expo".to_string(), "run:android".to_string()];
            // A bare `--device` makes the Expo CLI prompt for a device, which
            // works in the panel's interactive terminal; omitting the flag
            // entirely makes it pick the booted device itself. Only pass the
            // flag when we have a name Expo will actually recognise.
            if let Some(name) = expo_device_name {
                args.push("--device".to_string());
                args.push(name.to_string());
            }
            ResolvedCommand::npx("Run Android", args, project.root.clone(), true)
        }
        ProjectKind::BareReactNative => {
            let mut args = vec!["react-native".to_string(), "run-android".to_string()];
            if let Some(variant) = variant {
                args.push(format!("--mode={variant}"));
            }
            if let Some(serial) = device_serial {
                args.push("--deviceId".to_string());
                args.push(serial.to_string());
            }
            ResolvedCommand::npx("Run Android", args, project.root.clone(), true)
        }
    }
}

/// Build + install a bare React Native Android app via gradle, then launch it
/// with adb. This bypasses `react-native run-android`, whose `getInstallApkName`
/// mis-resolves the APK for product-flavor builds: it looks for
/// `app-<variant>.apk` while gradle emits `app-<flavor>-<buildType>.apk`, so the
/// CLI errors ("Could not find the correct install APK file") even though the
/// app installed. Gradle's `install<Variant>` task has no such problem, and we
/// launch the flavor's exact `application_id` with `monkey`.
pub fn run_android_gradle(
    project: &MobileProject,
    variant: &str,
    device_serial: Option<&str>,
    application_id: &str,
) -> Vec<ResolvedCommand> {
    let task = format!(":app:install{}", capitalize(variant));
    let install = ResolvedCommand {
        label: format!("gradlew {task}"),
        program: "./gradlew".to_string(),
        args: vec![task, "-PreactNativeDevServerPort=8081".to_string()],
        cwd: project.root.join("android"),
        wants_android_env: true,
    };
    let adb = toolchain::managed_adb_path()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "adb".to_string());
    let mut launch_args = Vec::new();
    if let Some(serial) = device_serial {
        launch_args.push("-s".to_string());
        launch_args.push(serial.to_string());
    }
    launch_args.extend([
        "shell".to_string(),
        "monkey".to_string(),
        "-p".to_string(),
        application_id.to_string(),
        "-c".to_string(),
        "android.intent.category.LAUNCHER".to_string(),
        "1".to_string(),
    ]);
    let launch = ResolvedCommand {
        label: format!("launch {application_id}"),
        program: adb,
        args: launch_args,
        cwd: project.root.clone(),
        wants_android_env: true,
    };
    vec![install, launch]
}

fn capitalize(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mobile_project::{PackageManager, ProjectKind};

    fn project(kind: ProjectKind) -> MobileProject {
        MobileProject {
            root: PathBuf::from("/tmp/app"),
            display_name: "App".to_string(),
            kind,
            package_manager: PackageManager::Yarn,
            android_package: None,
            ios_bundle_identifier: None,
            scripts: Vec::new(),
            ios_schemes: Vec::new(),
            android_variants: Vec::new(),
            android_flavor_application_ids: Default::default(),
            has_ios: true,
            has_android: true,
            has_podfile: true,
            has_gemfile: false,
            bundler_version: None,
            ruby_version: None,
            readme_run_hints: Vec::new(),
            has_node_modules: true,
            uses_eas: false,
        }
    }

    #[test]
    fn run_script_uses_package_manager() {
        let command = run_script(&project(ProjectKind::BareReactNative), "lint", &[]);
        assert_eq!(command.program, "yarn");
        assert_eq!(command.args, vec!["lint"]);
    }

    #[test]
    fn run_script_appends_extra_args() {
        let command = run_script(
            &project(ProjectKind::BareReactNative),
            "ios",
            &["tommys".to_string(), "staging".to_string()],
        );
        assert_eq!(command.args, vec!["ios", "tommys", "staging"]);
    }

    #[test]
    fn bare_rn_run_ios_uses_scheme_and_udid() {
        let command = run_ios(
            &project(ProjectKind::BareReactNative),
            Some("TommysStaging"),
            Some("SIM-UDID"),
        );
        assert_eq!(command.program, "npx");
        assert!(command.args.iter().any(|a| a == "react-native"));
        assert!(command.args.iter().any(|a| a == "--scheme=TommysStaging"));
        let udid_idx = command.args.iter().position(|a| a == "--udid").unwrap();
        assert_eq!(command.args.get(udid_idx + 1).map(String::as_str), Some("SIM-UDID"));
    }

    #[test]
    fn expo_run_ios_uses_expo_cli() {
        let command = run_ios(&project(ProjectKind::Expo), None, Some("SIM-UDID"));
        assert!(command.args.iter().any(|a| a == "expo"));
        assert!(command.args.iter().any(|a| a == "run:ios"));
    }

    #[test]
    fn bare_rn_run_android_uses_mode() {
        let command = run_android(
            &project(ProjectKind::BareReactNative),
            Some("tommysStagingDebug"),
            None,
            None,
        );
        assert!(command.args.iter().any(|a| a == "--mode=tommysStagingDebug"));
        assert!(command.wants_android_env);
    }

    #[test]
    fn bare_rn_run_android_targets_the_serial() {
        let command = run_android(
            &project(ProjectKind::BareReactNative),
            None,
            Some("emulator-5554"),
            Some("Lathe_Pixel_API35"),
        );
        let idx = command.args.iter().position(|a| a == "--deviceId").unwrap();
        assert_eq!(command.args.get(idx + 1).map(String::as_str), Some("emulator-5554"));
    }

    #[test]
    fn expo_run_android_targets_the_expo_device_name_not_the_serial() {
        let command = run_android(
            &project(ProjectKind::Expo),
            None,
            Some("emulator-5554"),
            Some("Lathe_Pixel_API35"),
        );
        let idx = command.args.iter().position(|a| a == "--device").unwrap();
        assert_eq!(
            command.args.get(idx + 1).map(String::as_str),
            Some("Lathe_Pixel_API35")
        );
        assert!(!command.args.iter().any(|a| a == "emulator-5554"));
    }

    #[test]
    fn expo_run_android_omits_device_flag_without_a_name() {
        let command = run_android(&project(ProjectKind::Expo), None, None, None);
        assert_eq!(command.args, vec!["expo", "run:android"]);
    }

    #[test]
    fn pod_install_uses_bundler_when_gemfile_present() {
        let mut project = project(ProjectKind::BareReactNative);
        project.has_gemfile = true;
        let command = pod_install(&project);
        assert_eq!(command.program, "bundle");
        assert_eq!(command.args, vec!["exec", "pod", "install"]);

        project.has_gemfile = false;
        let command = pod_install(&project);
        assert_eq!(command.args, vec!["install"]);
        assert!(command.program.ends_with("pod"));
    }

    #[test]
    fn bundler_runs_under_rbenv_when_ruby_pinned() {
        let mut project = project(ProjectKind::BareReactNative);
        project.has_gemfile = true;

        // No Ruby pin: plain `bundle`.
        let command = pod_install(&project);
        assert_eq!(command.program, "bundle");
        assert_eq!(command.args, vec!["exec", "pod", "install"]);

        // Ruby pinned: wrapped in `rbenv exec bundle ...`.
        project.ruby_version = Some("3.3.4".to_string());
        let command = pod_install(&project);
        assert!(command.program.ends_with("rbenv"));
        assert_eq!(command.args, vec!["exec", "bundle", "exec", "pod", "install"]);
    }

    #[test]
    fn install_ruby_uses_rbenv() {
        let command = install_ruby(&project(ProjectKind::BareReactNative), "3.3.4");
        assert!(command.program.ends_with("rbenv"));
        assert_eq!(command.args, vec!["install", "-s", "3.3.4"]);
    }

    #[test]
    fn adb_reverse_scopes_to_serial() {
        let command = adb_reverse(&project(ProjectKind::BareReactNative), Some("ABC123"));
        assert!(command.args.windows(2).any(|w| w == ["-s", "ABC123"]));
        assert!(command.args.iter().any(|a| a == "reverse"));
    }
}
