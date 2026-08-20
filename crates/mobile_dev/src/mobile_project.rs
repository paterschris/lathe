//! Detect React Native / Expo projects in the active workspace and probe
//! everything the mobile_dev panel can act on.
//!
//! Detection is deliberately broad: an Expo project (`app.json` with an
//! `expo` key, or an `expo` dependency) and a bare React Native project (a
//! `react-native` dependency, or sibling `ios/` and `android/` folders) both
//! qualify. Once a project is recognised we probe as much as we can about it
//! (package manager, the scripts it defines, iOS schemes, Android gradle
//! variants, bundle identifiers) so the panel can surface exactly the
//! workflow methods that apply to that project.
//!
//! All I/O here is synchronous; call [`detect_at`] from a background task. We
//! read from disk rather than Lathe's worktree tree because project-config
//! shapes are volatile and we don't want to pay the live-doc cost for what is
//! fundamentally a one-shot project sniff.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Whether the project is managed/bare Expo or plain React Native. Only
/// changes which run command the panel prefers (`expo run:*` vs the React
/// Native CLI) and which cloud-build options it offers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectKind {
    Expo,
    BareReactNative,
}

impl ProjectKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Expo => "Expo",
            Self::BareReactNative => "React Native",
        }
    }
}

/// The JS package manager the project uses, inferred from its lockfile. Drives
/// how the panel runs `package.json` scripts and the Metro server.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackageManager {
    Yarn,
    Npm,
    Pnpm,
    Bun,
}

impl PackageManager {
    pub fn program(self) -> &'static str {
        match self {
            Self::Yarn => "yarn",
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::Bun => "bun",
        }
    }

    pub fn label(self) -> &'static str {
        self.program()
    }

    /// Args that run the `package.json` script named `script`. `yarn` accepts
    /// the bare script name; the others require an explicit `run`.
    pub fn run_script_args(self, script: &str) -> Vec<String> {
        match self {
            Self::Yarn => vec![script.to_string()],
            Self::Npm | Self::Pnpm | Self::Bun => vec!["run".to_string(), script.to_string()],
        }
    }
}

/// One entry from the project's `package.json` `scripts` block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectScript {
    pub name: String,
    /// The raw command the script runs, kept for tooltip display.
    pub command: String,
}

/// Everything we detected about a recognised mobile project. Optional fields
/// are `None`/empty when the corresponding probe found nothing, and the panel
/// hides the matching controls accordingly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MobileProject {
    pub root: PathBuf,
    pub display_name: String,
    pub kind: ProjectKind,
    pub package_manager: PackageManager,
    /// Android application id (`android.package` for Expo, `applicationId`
    /// from `build.gradle` for bare RN). Optional on fresh projects.
    pub android_package: Option<String>,
    /// iOS bundle identifier (`ios.bundleIdentifier` for Expo,
    /// `PRODUCT_BUNDLE_IDENTIFIER` from the pbxproj for bare RN).
    pub ios_bundle_identifier: Option<String>,
    /// Scripts declared in `package.json`, alphabetically ordered.
    pub scripts: Vec<ProjectScript>,
    /// Shared Xcode scheme names (sans `.xcscheme`), for the native iOS run.
    pub ios_schemes: Vec<String>,
    /// Best-effort gradle build variants (`<flavor>Debug`) for the native
    /// Android run. Empty when the gradle files couldn't be parsed.
    pub android_variants: Vec<String>,
    /// Per-flavor `applicationId` from `build.gradle`, so the panel can launch
    /// the exact package a flavored variant installs (which the React Native
    /// CLI mis-resolves for flavored builds).
    pub android_flavor_application_ids: BTreeMap<String, String>,
    pub has_ios: bool,
    pub has_android: bool,
    pub has_podfile: bool,
    /// Whether the project manages its Ruby tooling (CocoaPods, fastlane) with
    /// Bundler, i.e. a `Gemfile` is present at the root or under `ios/`. When
    /// so, the panel installs pods the project's own way (`bundle exec pod
    /// install`) rather than a generic `pod install`.
    pub has_gemfile: bool,
    /// The Bundler version pinned in `Gemfile.lock` (`BUNDLED WITH`). Kept for
    /// display; the panel prefers running Bundler under the project's pinned
    /// Ruby over forcing this exact Bundler version.
    pub bundler_version: Option<String>,
    /// The Ruby version the project pins (`.ruby-version`, or `ruby "x"` in the
    /// Gemfile). When set, the panel runs Bundler under that Ruby via rbenv.
    pub ruby_version: Option<String>,
    /// Run/start commands scraped from the project's README, in order, so the
    /// panel can suggest how to run this specific app.
    pub readme_run_hints: Vec<String>,
    /// Whether `node_modules` is present. `false` means JS dependencies still
    /// need installing before any command can run.
    pub has_node_modules: bool,
    /// Whether cloud (EAS) builds are relevant: an `eas.json` exists, or the
    /// project is Expo.
    pub uses_eas: bool,
}

impl MobileProject {
    /// The `applicationId` a gradle variant installs. Strips the build type
    /// (`Debug`/`Release`) to get the flavor, then looks up its per-flavor
    /// `applicationId`, falling back to the project's default Android package.
    pub fn variant_application_id(&self, variant: &str) -> Option<String> {
        let flavor = variant
            .strip_suffix("Debug")
            .or_else(|| variant.strip_suffix("Release"))
            .unwrap_or(variant);
        self.android_flavor_application_ids
            .get(flavor)
            .cloned()
            .or_else(|| self.android_package.clone())
    }
}

/// Recognise the project rooted at `root`, or `None` if it isn't a React
/// Native / Expo project. I/O is synchronous; call from a background task.
pub fn detect_at(root: &Path) -> Option<MobileProject> {
    let package_json = read_package_json(root);
    let app_json = read_app_json(root);

    let has_ios = root.join("ios").is_dir();
    let has_android = root.join("android").is_dir();

    let has_dep = |name: &str| {
        package_json
            .as_ref()
            .is_some_and(|package| package.has_dependency(name))
    };

    let expo_section = app_json.as_ref().and_then(|app| app.expo.as_ref());
    let is_expo = has_dep("expo") || expo_section.is_some();
    let is_bare_rn = has_dep("react-native") || (has_ios && has_android);

    let kind = if is_expo {
        ProjectKind::Expo
    } else if is_bare_rn {
        ProjectKind::BareReactNative
    } else {
        return None;
    };

    let display_name = package_json
        .as_ref()
        .and_then(|package| package.name.clone())
        .or_else(|| expo_section.and_then(|expo| expo.name.clone()))
        .or_else(|| app_json.as_ref().and_then(|app| app.display_name.clone()))
        .or_else(|| {
            root.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "Mobile project".to_string());

    let android_package = expo_section
        .and_then(|expo| expo.android.as_ref())
        .and_then(|android| android.package.clone())
        .or_else(|| detect_android_application_id(root));

    let ios_bundle_identifier = expo_section
        .and_then(|expo| expo.ios.as_ref())
        .and_then(|ios| ios.bundle_identifier.clone())
        .or_else(|| detect_ios_bundle_identifier(root));

    let scripts = package_json
        .as_ref()
        .map(|package| package.scripts())
        .unwrap_or_default();

    Some(MobileProject {
        root: root.to_path_buf(),
        display_name,
        kind,
        package_manager: detect_package_manager(root),
        android_package,
        ios_bundle_identifier,
        scripts,
        ios_schemes: detect_ios_schemes(root),
        android_variants: detect_android_variants(root),
        android_flavor_application_ids: detect_flavor_application_ids(root),
        has_ios,
        has_android,
        has_podfile: root.join("ios/Podfile").is_file(),
        has_gemfile: root.join("Gemfile").is_file() || root.join("ios/Gemfile").is_file(),
        bundler_version: detect_bundler_version(root),
        ruby_version: detect_ruby_version(root),
        readme_run_hints: detect_readme_run_hints(root),
        has_node_modules: root.join("node_modules").is_dir(),
        uses_eas: is_expo || root.join("eas.json").is_file(),
    })
}

fn read_package_json(root: &Path) -> Option<PackageJson> {
    let contents = std::fs::read_to_string(root.join("package.json")).ok()?;
    serde_json::from_str(&contents).ok()
}

fn read_app_json(root: &Path) -> Option<AppJson> {
    let contents = std::fs::read_to_string(root.join("app.json")).ok()?;
    serde_json::from_str(&contents).ok()
}

/// The Bundler version from the `BUNDLED WITH` stanza of `Gemfile.lock`:
///
/// ```text
/// BUNDLED WITH
///    2.1.4
/// ```
fn detect_bundler_version(root: &Path) -> Option<String> {
    let contents = read_first_existing(root, &["Gemfile.lock", "ios/Gemfile.lock"])?;
    let mut lines = contents.lines();
    while let Some(line) = lines.next() {
        if line.trim() == "BUNDLED WITH" {
            let version = lines.next()?.trim();
            if !version.is_empty() {
                return Some(version.to_string());
            }
        }
    }
    None
}

/// The Ruby version the project pins: `.ruby-version` first (a bare version
/// string), else a `ruby "x.y.z"` / `ruby 'x.y.z'` line in the Gemfile.
fn detect_ruby_version(root: &Path) -> Option<String> {
    if let Ok(contents) = std::fs::read_to_string(root.join(".ruby-version")) {
        let version = contents.trim().trim_start_matches("ruby-").trim();
        if !version.is_empty() {
            return Some(version.to_string());
        }
    }
    let gemfile = read_first_existing(root, &["Gemfile", "ios/Gemfile"])?;
    for line in gemfile.lines() {
        let line = line.trim();
        if line.starts_with("ruby ")
            && let Some(version) = extract_quoted(line)
        {
            return Some(version);
        }
    }
    None
}

/// Prefixes that mark a README shell line as a "run/start the app" command,
/// as opposed to prose or unrelated setup.
const README_COMMAND_PREFIXES: &[&str] = &[
    "yarn ",
    "npm ",
    "npx ",
    "pnpm ",
    "bun ",
    "pod ",
    "bundle ",
    "adb ",
    "react-native ",
    "expo ",
    "./scripts/",
    "./gradlew",
    "xcodebuild ",
    "fastlane ",
];

/// Scrape run/start commands out of the project's README fenced code blocks,
/// in document order, so the panel can suggest how to run this specific app.
/// Heuristic and best-effort: unknown READMEs simply yield no hints.
fn detect_readme_run_hints(root: &Path) -> Vec<String> {
    let Some(contents) =
        read_first_existing(root, &["README.md", "readme.md", "README", "Readme.md"])
    else {
        return Vec::new();
    };
    let mut hints = Vec::new();
    let mut in_code_block = false;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }
        if !in_code_block {
            continue;
        }
        let command = trimmed.trim_start_matches("$ ").trim();
        if command.starts_with('#') {
            continue;
        }
        if README_COMMAND_PREFIXES
            .iter()
            .any(|prefix| command.starts_with(prefix))
            && !hints.iter().any(|hint| hint == command)
        {
            hints.push(command.to_string());
            if hints.len() >= 12 {
                break;
            }
        }
    }
    hints
}

fn detect_package_manager(root: &Path) -> PackageManager {
    if root.join("bun.lockb").is_file() || root.join("bun.lock").is_file() {
        PackageManager::Bun
    } else if root.join("pnpm-lock.yaml").is_file() {
        PackageManager::Pnpm
    } else if root.join("yarn.lock").is_file() {
        PackageManager::Yarn
    } else {
        PackageManager::Npm
    }
}

/// Shared schemes only (under `xcshareddata`), which is what
/// `react-native run-ios --scheme` needs; per-user schemes live in
/// `xcuserdata` and aren't portable.
fn detect_ios_schemes(root: &Path) -> Vec<String> {
    let ios = root.join("ios");
    let Ok(entries) = std::fs::read_dir(&ios) else {
        return Vec::new();
    };
    let mut schemes = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_container = path
            .extension()
            .is_some_and(|ext| ext == "xcodeproj" || ext == "xcworkspace");
        if !is_container {
            continue;
        }
        let schemes_dir = path.join("xcshareddata/xcschemes");
        let Ok(scheme_entries) = std::fs::read_dir(&schemes_dir) else {
            continue;
        };
        for scheme in scheme_entries.flatten() {
            let scheme_path = scheme.path();
            if scheme_path.extension().is_some_and(|ext| ext == "xcscheme")
                && let Some(stem) = scheme_path.file_stem()
            {
                let name = stem.to_string_lossy().into_owned();
                if !schemes.contains(&name) {
                    schemes.push(name);
                }
            }
        }
    }
    schemes.sort();
    schemes
}

fn detect_android_application_id(root: &Path) -> Option<String> {
    let contents = read_first_existing(
        root,
        &["android/app/build.gradle", "android/app/build.gradle.kts"],
    )?;
    parse_application_id(&contents)
}

fn parse_application_id(gradle: &str) -> Option<String> {
    for line in gradle.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("applicationId") {
            // Skip `applicationIdSuffix` etc.: the id must be followed by
            // whitespace or `=`, not more identifier characters.
            if rest
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                continue;
            }
            if let Some(value) = extract_quoted(rest) {
                return Some(value);
            }
        }
    }
    None
}

/// Gradle flavors paired with the debug build type, e.g.
/// `productFlavors { tommys { ... } }` -> `["tommysDebug"]`. Best-effort:
/// returns empty when the block can't be found so the panel simply omits the
/// Android variant picker.
fn detect_android_variants(root: &Path) -> Vec<String> {
    let Some(contents) = read_first_existing(
        root,
        &["android/app/build.gradle", "android/app/build.gradle.kts"],
    ) else {
        return Vec::new();
    };
    parse_product_flavors(&contents)
        .into_iter()
        .map(|flavor| format!("{flavor}Debug"))
        .collect()
}

fn parse_product_flavors(gradle: &str) -> Vec<String> {
    let Some(start) = gradle.find("productFlavors") else {
        return Vec::new();
    };
    let after = &gradle[start..];
    let Some(open) = after.find('{') else {
        return Vec::new();
    };
    let mut depth = 0i32;
    let mut flavors = Vec::new();
    let body = &after[open..];
    for line in body.lines() {
        let trimmed = line.trim();
        // A flavor declaration opens its own block at depth 1:
        // `productFlavors {` is depth 0->1, `<flavor> {` is depth 1->2.
        if depth == 1
            && let Some(name) = trimmed.strip_suffix('{')
        {
            let name = name.trim();
            if is_gradle_identifier(name) {
                flavors.push(name.to_string());
            }
        }
        depth += trimmed.matches('{').count() as i32;
        depth -= trimmed.matches('}').count() as i32;
        if depth <= 0 {
            break;
        }
    }
    flavors
}

fn is_gradle_identifier(token: &str) -> bool {
    !token.is_empty()
        && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && token
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
}

fn detect_flavor_application_ids(root: &Path) -> BTreeMap<String, String> {
    read_first_existing(
        root,
        &["android/app/build.gradle", "android/app/build.gradle.kts"],
    )
    .map(|gradle| parse_flavor_application_ids(&gradle))
    .unwrap_or_default()
}

/// Map each product flavor to its `applicationId`, scanning every
/// `productFlavors { ... }` block (a flavor's id may be declared in the
/// top-level block while another block only sets its signing config).
fn parse_flavor_application_ids(gradle: &str) -> BTreeMap<String, String> {
    let mut ids = BTreeMap::new();
    let mut offset = 0;
    while let Some(pos) = gradle[offset..].find("productFlavors") {
        let start = offset + pos + "productFlavors".len();
        offset = start;
        let rest = &gradle[start..];
        // Skip `productFlavors.all { ... }` and similar (not a flavor container).
        if rest.trim_start().starts_with('.') {
            continue;
        }
        let Some(open) = rest.find('{') else { break };
        let mut depth = 1i32;
        let mut current_flavor: Option<String> = None;
        for line in rest[open + 1..].lines() {
            let trimmed = line.trim();
            // A `<flavor> {` opens a flavor block at productFlavors depth (1).
            if depth == 1
                && let Some(name) = trimmed.strip_suffix('{')
                && is_gradle_identifier(name.trim())
            {
                current_flavor = Some(name.trim().to_string());
            }
            if let Some(flavor) = current_flavor.clone()
                && let Some(after) = trimmed.strip_prefix("applicationId")
                && !after
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
                && let Some(app_id) = extract_quoted(after)
            {
                ids.entry(flavor).or_insert(app_id);
            }
            depth += trimmed.matches('{').count() as i32;
            depth -= trimmed.matches('}').count() as i32;
            // Back at productFlavors level: the current flavor block closed.
            if depth <= 1 {
                current_flavor = None;
            }
            if depth <= 0 {
                break;
            }
        }
    }
    ids
}

fn detect_ios_bundle_identifier(root: &Path) -> Option<String> {
    let ios = root.join("ios");
    let entries = std::fs::read_dir(&ios).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "xcodeproj") {
            let pbxproj = path.join("project.pbxproj");
            if let Ok(contents) = std::fs::read_to_string(&pbxproj)
                && let Some(id) = parse_bundle_identifier(&contents)
            {
                return Some(id);
            }
        }
    }
    None
}

fn parse_bundle_identifier(pbxproj: &str) -> Option<String> {
    for line in pbxproj.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("PRODUCT_BUNDLE_IDENTIFIER") {
            let value = rest
                .trim()
                .trim_start_matches('=')
                .trim()
                .trim_end_matches(';')
                .trim()
                .trim_matches('"')
                .trim();
            // Xcode often points this at a build-setting variable; skip those,
            // they aren't a usable bundle id on their own.
            if !value.is_empty() && !value.contains("$(") {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn read_first_existing(root: &Path, relatives: &[&str]) -> Option<String> {
    relatives
        .iter()
        .find_map(|relative| std::fs::read_to_string(root.join(relative)).ok())
}

/// Extract the first single- or double-quoted string from `input`.
/// Split a scheme or gradle-variant name into positional `[app_variant,
/// configuration]` script arguments (lowercased), for projects whose scripts
/// take them positionally (e.g. `yarn ios tommys staging`). A trailing build
/// type (`Debug`/`Release`) is dropped, the name is split on camelCase
/// boundaries, the last token is the configuration, and the rest are joined as
/// the variant. `TommysStaging` and `tommysStagingDebug` both yield
/// `["tommys", "staging"]`; `WashClubStaging` yields `["washclub", "staging"]`.
pub fn split_variant_config(name: &str) -> Vec<String> {
    let base = name
        .strip_suffix("Debug")
        .or_else(|| name.strip_suffix("Release"))
        .unwrap_or(name);
    let tokens = split_camel_case(base);
    match tokens.len() {
        0 => Vec::new(),
        1 => vec![tokens[0].to_lowercase()],
        _ => {
            let (config, variant_tokens) = tokens.split_last().unwrap();
            let variant: String = variant_tokens.iter().map(|t| t.to_lowercase()).collect();
            vec![variant, config.to_lowercase()]
        }
    }
}

fn split_camel_case(value: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in value.chars() {
        if ch.is_uppercase() && !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
        current.push(ch);
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn extract_quoted(input: &str) -> Option<String> {
    for quote in ['"', '\''] {
        if let Some(start) = input.find(quote) {
            let rest = &input[start + 1..];
            if let Some(end) = rest.find(quote) {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

#[derive(Debug, Deserialize)]
struct PackageJson {
    name: Option<String>,
    #[serde(default)]
    scripts: BTreeMap<String, String>,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "devDependencies")]
    dev_dependencies: BTreeMap<String, String>,
}

impl PackageJson {
    fn has_dependency(&self, name: &str) -> bool {
        self.dependencies.contains_key(name) || self.dev_dependencies.contains_key(name)
    }

    fn scripts(&self) -> Vec<ProjectScript> {
        self.scripts
            .iter()
            .map(|(name, command)| ProjectScript {
                name: name.clone(),
                command: command.clone(),
            })
            .collect()
    }
}

#[derive(Debug, Deserialize)]
struct AppJson {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    expo: Option<ExpoSection>,
}

#[derive(Debug, Deserialize)]
struct ExpoSection {
    name: Option<String>,
    android: Option<AndroidSection>,
    ios: Option<IosSection>,
}

#[derive(Debug, Deserialize)]
struct AndroidSection {
    package: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IosSection {
    #[serde(rename = "bundleIdentifier")]
    bundle_identifier: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &Path, relative: &str, body: &str) {
        let path = dir.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(body.as_bytes()).unwrap();
    }

    #[test]
    fn returns_none_for_non_mobile_project() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "package.json", r#"{"name": "web-app"}"#);
        assert!(detect_at(tmp.path()).is_none());
    }

    #[test]
    fn detects_expo_from_app_json() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "app.json",
            r#"{"expo": {"name": "UnBenched", "slug": "unbenched",
                "android": {"package": "com.example.unbenched"},
                "ios": {"bundleIdentifier": "com.example.unbenched"}}}"#,
        );
        let project = detect_at(tmp.path()).unwrap();
        assert_eq!(project.kind, ProjectKind::Expo);
        assert_eq!(project.display_name, "UnBenched");
        assert_eq!(
            project.android_package.as_deref(),
            Some("com.example.unbenched")
        );
        assert!(project.uses_eas);
    }

    #[test]
    fn detects_bare_react_native_from_dependency() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{"name": "TommysExpress",
                "scripts": {"ios": "./scripts/run-ios", "start": "yarn react-native start"},
                "dependencies": {"react-native": "0.85.3"}}"#,
        );
        write(tmp.path(), "yarn.lock", "");
        let project = detect_at(tmp.path()).unwrap();
        assert_eq!(project.kind, ProjectKind::BareReactNative);
        assert_eq!(project.package_manager, PackageManager::Yarn);
        assert!(!project.uses_eas);
        let script_names: Vec<&str> = project.scripts.iter().map(|s| s.name.as_str()).collect();
        assert!(script_names.contains(&"ios"));
        assert!(script_names.contains(&"start"));
    }

    #[test]
    fn detects_bare_react_native_from_native_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "package.json", r#"{"name": "app"}"#);
        std::fs::create_dir_all(tmp.path().join("ios")).unwrap();
        std::fs::create_dir_all(tmp.path().join("android")).unwrap();
        let project = detect_at(tmp.path()).unwrap();
        assert_eq!(project.kind, ProjectKind::BareReactNative);
        assert!(project.has_ios);
        assert!(project.has_android);
    }

    #[test]
    fn package_manager_detection_prefers_specific_lockfiles() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{"name": "app", "dependencies": {"react-native": "0.85.0"}}"#,
        );
        assert_eq!(
            detect_at(tmp.path()).unwrap().package_manager,
            PackageManager::Npm
        );
        write(tmp.path(), "pnpm-lock.yaml", "");
        assert_eq!(
            detect_at(tmp.path()).unwrap().package_manager,
            PackageManager::Pnpm
        );
    }

    #[test]
    fn parses_application_id() {
        assert_eq!(
            parse_application_id("android {\n  applicationId \"com.x.y\"\n}"),
            Some("com.x.y".to_string())
        );
        assert_eq!(
            parse_application_id("applicationId = 'com.a.b'"),
            Some("com.a.b".to_string())
        );
    }

    #[test]
    fn parses_product_flavors() {
        let gradle = r#"
            android {
                productFlavors {
                    tommys {
                        applicationId "com.tommys"
                    }
                    washclub {
                        applicationId "com.washclub"
                    }
                }
            }
        "#;
        assert_eq!(
            parse_product_flavors(gradle),
            vec!["tommys".to_string(), "washclub".to_string()]
        );
    }

    #[test]
    fn parses_bundle_identifier_skips_variables() {
        let pbxproj = "PRODUCT_BUNDLE_IDENTIFIER = \"$(SOMETHING)\";\n\
                       PRODUCT_BUNDLE_IDENTIFIER = com.tommys.app;";
        assert_eq!(
            parse_bundle_identifier(pbxproj),
            Some("com.tommys.app".to_string())
        );
    }

    #[test]
    fn detects_ios_schemes() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{"name": "app", "dependencies": {"react-native": "0.85.0"}}"#,
        );
        write(
            tmp.path(),
            "ios/TommysApp.xcodeproj/xcshareddata/xcschemes/TommysStaging.xcscheme",
            "<Scheme/>",
        );
        write(
            tmp.path(),
            "ios/TommysApp.xcodeproj/xcshareddata/xcschemes/TommysProd.xcscheme",
            "<Scheme/>",
        );
        let project = detect_at(tmp.path()).unwrap();
        assert_eq!(
            project.ios_schemes,
            vec!["TommysProd".to_string(), "TommysStaging".to_string()]
        );
    }

    #[test]
    fn detects_bundler_version_and_gemfile() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{"name": "app", "dependencies": {"react-native": "0.85.0"}}"#,
        );
        write(
            tmp.path(),
            "Gemfile",
            "source 'https://rubygems.org'\ngem 'cocoapods'\n",
        );
        write(
            tmp.path(),
            "Gemfile.lock",
            "GEM\n  specs:\n\nPLATFORMS\n  ruby\n\nBUNDLED WITH\n   2.1.4\n",
        );
        let project = detect_at(tmp.path()).unwrap();
        assert!(project.has_gemfile);
        assert_eq!(project.bundler_version.as_deref(), Some("2.1.4"));
    }

    #[test]
    fn detects_ruby_version_from_dotfile() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{"name": "app", "dependencies": {"react-native": "0.85.0"}}"#,
        );
        write(tmp.path(), ".ruby-version", "3.3.4\n");
        assert_eq!(
            detect_at(tmp.path()).unwrap().ruby_version.as_deref(),
            Some("3.3.4")
        );
    }

    #[test]
    fn scrapes_readme_run_hints_from_code_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{"name": "app", "dependencies": {"react-native": "0.85.0"}}"#,
        );
        write(
            tmp.path(),
            "README.md",
            "# App\n\nSome prose with `inline` text.\n\n## Development\n\n```sh\n$ yarn start\nyarn ios tommys staging\n# a comment\nadb reverse tcp:8081 tcp:8081\n```\n\nMore prose: run yarn foo (not in a block).\n",
        );
        let hints = detect_at(tmp.path()).unwrap().readme_run_hints;
        assert_eq!(
            hints,
            vec![
                "yarn start".to_string(),
                "yarn ios tommys staging".to_string(),
                "adb reverse tcp:8081 tcp:8081".to_string(),
            ]
        );
    }

    #[test]
    fn parses_per_flavor_application_ids() {
        // Mirrors tommys: a signing-only productFlavors block, a
        // `productFlavors.all`, then the real block with applicationIds.
        let gradle = r#"
            android {
                buildTypes {
                    release {
                        productFlavors {
                            tommysStaging { signingConfig signingConfigs.tommysStaging }
                            tommysProd { signingConfig signingConfigs.tommysProduction }
                        }
                    }
                }
                productFlavors.all {
                    buildConfigField "String", "x", "\"y\""
                }
                productFlavors {
                    tommysStaging {
                        dimension "operatorGroup"
                        applicationId "com.tommycarwash.tommysexpress.staging"
                    }
                    tommysProd {
                        dimension "operatorGroup"
                        applicationId "com.superoperator.tommyexpress"
                    }
                }
            }
        "#;
        let ids = parse_flavor_application_ids(gradle);
        assert_eq!(
            ids.get("tommysStaging").map(String::as_str),
            Some("com.tommycarwash.tommysexpress.staging")
        );
        assert_eq!(
            ids.get("tommysProd").map(String::as_str),
            Some("com.superoperator.tommyexpress")
        );
    }

    #[test]
    fn variant_application_id_strips_build_type() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{"name": "app", "dependencies": {"react-native": "0.85.0"}}"#,
        );
        let mut project = detect_at(tmp.path()).unwrap();
        project.android_flavor_application_ids = BTreeMap::from([(
            "tommysStaging".to_string(),
            "com.tommycarwash.tommysexpress.staging".to_string(),
        )]);
        assert_eq!(
            project
                .variant_application_id("tommysStagingDebug")
                .as_deref(),
            Some("com.tommycarwash.tommysexpress.staging")
        );
    }

    #[test]
    fn splits_scheme_and_variant_into_args() {
        assert_eq!(
            split_variant_config("TommysStaging"),
            vec!["tommys", "staging"]
        );
        assert_eq!(
            split_variant_config("tommysStagingDebug"),
            vec!["tommys", "staging"]
        );
        assert_eq!(
            split_variant_config("WashClubStaging"),
            vec!["washclub", "staging"]
        );
        assert_eq!(split_variant_config("TommysProd"), vec!["tommys", "prod"]);
        assert_eq!(split_variant_config("Tommys"), vec!["tommys"]);
    }

    #[test]
    fn run_script_args_shape() {
        assert_eq!(PackageManager::Yarn.run_script_args("ios"), vec!["ios"]);
        assert_eq!(
            PackageManager::Npm.run_script_args("ios"),
            vec!["run", "ios"]
        );
    }
}
