//! Detect Expo / React Native projects in the active workspace.
//!
//! The signal we use is `app.json` at a worktree root with an `expo` key.
//! That covers Expo managed and bare projects, plus most RN apps that have
//! been "evicted" to bare workflows but still keep an app.json for tooling.
//!
//! We deliberately read the file synchronously from disk in a background
//! task. Lathe's worktree-stored file tree could give us the file content,
//! but parsing app.json shape is volatile (Expo SDK upgrades change it) and
//! we don't want to pay the live-doc cost for what is fundamentally a
//! project-config sniff.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// What we know about the detected Expo project. The package name is the
/// most important field, because it's the key for adb am start and logcat
/// PID filtering; everything else is for UI display.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExpoProject {
    pub root: PathBuf,
    pub slug: String,
    pub display_name: String,
    /// Android application id from `expo.android.package`. Optional because
    /// brand-new Expo projects often don't have it set yet; we surface that
    /// gap to the UI so the user knows what to add to `app.json`.
    pub android_package: Option<String>,
}

/// Look for `<root>/app.json` and parse it for the `expo` block. Returns
/// `None` if the file is missing, unparseable, or has no `expo` field.
/// I/O is synchronous; call from a background task.
pub fn detect_at(root: &Path) -> Option<ExpoProject> {
    let app_json_path = root.join("app.json");
    let contents = std::fs::read_to_string(&app_json_path).ok()?;
    let parsed: AppJson = serde_json::from_str(&contents).ok()?;
    let expo = parsed.expo?;

    let display_name = expo.name.clone().unwrap_or_else(|| expo.slug.clone());

    Some(ExpoProject {
        root: root.to_path_buf(),
        slug: expo.slug,
        display_name,
        android_package: expo.android.and_then(|a| a.package),
    })
}

#[derive(Debug, Deserialize)]
struct AppJson {
    expo: Option<ExpoSection>,
}

#[derive(Debug, Deserialize)]
struct ExpoSection {
    slug: String,
    name: Option<String>,
    android: Option<AndroidSection>,
}

#[derive(Debug, Deserialize)]
struct AndroidSection {
    package: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_app_json(dir: &Path, body: &str) {
        let mut file = std::fs::File::create(dir.join("app.json")).unwrap();
        file.write_all(body.as_bytes()).unwrap();
    }

    #[test]
    fn returns_none_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(detect_at(tmp.path()).is_none());
    }

    #[test]
    fn returns_none_when_no_expo_key() {
        let tmp = tempfile::tempdir().unwrap();
        write_app_json(tmp.path(), r#"{"name": "regular-app"}"#);
        assert!(detect_at(tmp.path()).is_none());
    }

    #[test]
    fn parses_minimal_expo_project() {
        let tmp = tempfile::tempdir().unwrap();
        write_app_json(tmp.path(), r#"{"expo": {"slug": "myapp"}}"#);
        let project = detect_at(tmp.path()).unwrap();
        assert_eq!(project.slug, "myapp");
        assert_eq!(project.display_name, "myapp");
        assert!(project.android_package.is_none());
    }

    #[test]
    fn parses_full_expo_block() {
        let tmp = tempfile::tempdir().unwrap();
        write_app_json(
            tmp.path(),
            r#"{
                "expo": {
                    "name": "UnBenched",
                    "slug": "unbenched",
                    "android": {
                        "package": "com.paterschris.unbenched"
                    }
                }
            }"#,
        );
        let project = detect_at(tmp.path()).unwrap();
        assert_eq!(project.slug, "unbenched");
        assert_eq!(project.display_name, "UnBenched");
        assert_eq!(
            project.android_package.as_deref(),
            Some("com.paterschris.unbenched")
        );
    }

    #[test]
    fn ignores_unparseable_app_json() {
        let tmp = tempfile::tempdir().unwrap();
        write_app_json(tmp.path(), "this is not json {");
        assert!(detect_at(tmp.path()).is_none());
    }

    #[test]
    fn tolerates_extra_fields() {
        let tmp = tempfile::tempdir().unwrap();
        write_app_json(
            tmp.path(),
            r#"{
                "expo": {
                    "name": "UnBenched",
                    "slug": "unbenched",
                    "version": "1.0.0",
                    "orientation": "portrait",
                    "icon": "./assets/icon.png",
                    "android": {
                        "package": "com.example.unbenched",
                        "adaptiveIcon": {
                            "foregroundImage": "./assets/foreground.png"
                        }
                    },
                    "extra": {
                        "eas": {"projectId": "00000000"}
                    }
                }
            }"#,
        );
        assert!(detect_at(tmp.path()).is_some());
    }
}
