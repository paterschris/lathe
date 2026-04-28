use anyhow::{Context as _, Result};
use fs::Fs;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub const PORTABLE_WORKSPACE_EXTENSION: &str = "lathe-workspace";

const CURRENT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableWorkspace {
    pub version: u32,
    pub folders: Vec<PortableFolder>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableFolder {
    pub path: PathBuf,
}

impl PortableWorkspace {
    /// Persist `absolute_folder_paths` to `file_path` as a portable workspace.
    /// Folder paths within the file's directory tree are stored relative to the
    /// file (so the workspace file can be moved as a unit); paths on different
    /// roots fall back to absolute.
    pub async fn save(
        fs: Arc<dyn Fs>,
        file_path: &Path,
        absolute_folder_paths: &[PathBuf],
    ) -> Result<()> {
        let base_dir = file_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();

        let folders = absolute_folder_paths
            .iter()
            .map(|abs| {
                let stored = relativize(abs, &base_dir).unwrap_or_else(|| abs.clone());
                PortableFolder { path: stored }
            })
            .collect();

        let portable = PortableWorkspace {
            version: CURRENT_VERSION,
            folders,
        };
        let json =
            serde_json::to_string_pretty(&portable).context("serializing portable workspace")?;
        fs.atomic_write(file_path.to_path_buf(), json)
            .await
            .with_context(|| format!("writing portable workspace to {}", file_path.display()))
    }

    /// Read `file_path` and return its folder paths resolved against the file's
    /// directory (so relative entries become absolute).
    pub async fn load(fs: Arc<dyn Fs>, file_path: &Path) -> Result<Vec<PathBuf>> {
        let contents = fs
            .load(file_path)
            .await
            .with_context(|| format!("reading portable workspace from {}", file_path.display()))?;
        let parsed: Self = serde_json::from_str(&contents)
            .with_context(|| format!("parsing portable workspace at {}", file_path.display()))?;
        anyhow::ensure!(
            parsed.version == CURRENT_VERSION,
            "unsupported portable workspace version {} at {} (expected {})",
            parsed.version,
            file_path.display(),
            CURRENT_VERSION,
        );

        let base_dir = file_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();

        Ok(parsed
            .folders
            .into_iter()
            .map(|folder| {
                if folder.path.is_absolute() {
                    folder.path
                } else {
                    base_dir.join(folder.path)
                }
            })
            .collect())
    }
}

pub fn has_portable_workspace_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case(PORTABLE_WORKSPACE_EXTENSION))
}

/// Replace the extension on `path` with `.lathe-workspace`, unless it already
/// has that extension (case-insensitive). Append it if there is no extension.
pub fn ensure_portable_workspace_extension(path: &mut PathBuf) {
    if !has_portable_workspace_extension(path) {
        path.set_extension(PORTABLE_WORKSPACE_EXTENSION);
    }
}

/// Compute `target` expressed relative to `base`, walking up with `..` as
/// needed. Returns `None` when the paths share no common root (e.g. different
/// Windows drives) or either is not absolute.
fn relativize(target: &Path, base: &Path) -> Option<PathBuf> {
    if !target.is_absolute() || !base.is_absolute() {
        return None;
    }

    let target_components: Vec<_> = target.components().collect();
    let base_components: Vec<_> = base.components().collect();

    let common_len = target_components
        .iter()
        .zip(base_components.iter())
        .take_while(|(a, b)| a == b)
        .count();

    if common_len == 0 {
        return None;
    }

    let mut relative = PathBuf::new();
    for _ in common_len..base_components.len() {
        relative.push("..");
    }
    for component in &target_components[common_len..] {
        relative.push(component.as_os_str());
    }
    if relative.as_os_str().is_empty() {
        relative.push(".");
    }
    Some(relative)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_match_is_case_insensitive() {
        assert!(has_portable_workspace_extension(Path::new(
            "foo.lathe-workspace"
        )));
        assert!(has_portable_workspace_extension(Path::new(
            "foo.LATHE-WORKSPACE"
        )));
        assert!(!has_portable_workspace_extension(Path::new("foo.json")));
        assert!(!has_portable_workspace_extension(Path::new("foo")));
    }

    #[test]
    fn ensure_extension_replaces_when_wrong() {
        let mut p = PathBuf::from("/tmp/foo.txt");
        ensure_portable_workspace_extension(&mut p);
        assert_eq!(p, PathBuf::from("/tmp/foo.lathe-workspace"));
    }

    #[test]
    fn ensure_extension_appends_when_missing() {
        let mut p = PathBuf::from("/tmp/foo");
        ensure_portable_workspace_extension(&mut p);
        assert_eq!(p, PathBuf::from("/tmp/foo.lathe-workspace"));
    }

    #[test]
    fn ensure_extension_keeps_correct() {
        let mut p = PathBuf::from("/tmp/foo.lathe-workspace");
        ensure_portable_workspace_extension(&mut p);
        assert_eq!(p, PathBuf::from("/tmp/foo.lathe-workspace"));
    }

    #[test]
    fn ensure_extension_normalizes_uppercase_to_canonical() {
        // Already a portable workspace by extension, so we leave the user's
        // casing alone — round-tripping does not rewrite the filename.
        let mut p = PathBuf::from("/tmp/foo.LATHE-WORKSPACE");
        ensure_portable_workspace_extension(&mut p);
        assert_eq!(p, PathBuf::from("/tmp/foo.LATHE-WORKSPACE"));
    }

    #[cfg(unix)]
    #[test]
    fn relativize_descends_when_target_under_base() {
        let rel = relativize(Path::new("/a/b/c"), Path::new("/a")).unwrap();
        assert_eq!(rel, PathBuf::from("b/c"));
    }

    #[cfg(unix)]
    #[test]
    fn relativize_ascends_when_target_above_base() {
        let rel = relativize(Path::new("/a"), Path::new("/a/b/c")).unwrap();
        assert_eq!(rel, PathBuf::from("../.."));
    }

    #[cfg(unix)]
    #[test]
    fn relativize_handles_siblings() {
        let rel = relativize(Path::new("/a/b"), Path::new("/a/c")).unwrap();
        assert_eq!(rel, PathBuf::from("../b"));
    }

    #[cfg(unix)]
    #[test]
    fn relativize_returns_dot_for_equal_paths() {
        let rel = relativize(Path::new("/a/b"), Path::new("/a/b")).unwrap();
        assert_eq!(rel, PathBuf::from("."));
    }

    #[cfg(unix)]
    #[test]
    fn relativize_rejects_relative_inputs() {
        assert!(relativize(Path::new("a"), Path::new("/b")).is_none());
        assert!(relativize(Path::new("/a"), Path::new("b")).is_none());
    }

    #[gpui::test]
    async fn save_writes_relative_paths_for_siblings(cx: &mut gpui::TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/work", serde_json::json!({"proj-a": {}, "proj-b": {}}))
            .await;

        let file_path = Path::new("/work/saved.lathe-workspace");
        PortableWorkspace::save(
            fs.clone(),
            file_path,
            &[
                PathBuf::from("/work/proj-a"),
                PathBuf::from("/work/proj-b"),
            ],
        )
        .await
        .unwrap();

        let raw = fs.load(file_path).await.unwrap();
        let parsed: PortableWorkspace = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.version, CURRENT_VERSION);
        assert_eq!(
            parsed.folders,
            vec![
                PortableFolder {
                    path: PathBuf::from("proj-a")
                },
                PortableFolder {
                    path: PathBuf::from("proj-b")
                },
            ]
        );

        let resolved = PortableWorkspace::load(fs.clone(), file_path).await.unwrap();
        assert_eq!(
            resolved,
            vec![
                PathBuf::from("/work/proj-a"),
                PathBuf::from("/work/proj-b"),
            ]
        );
    }

    #[gpui::test]
    async fn load_resolves_relative_paths_against_file_dir(cx: &mut gpui::TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/group", serde_json::json!({})).await;
        fs.insert_tree("/other/proj", serde_json::json!({})).await;

        let file_path = Path::new("/group/ws.lathe-workspace");
        let raw = serde_json::json!({
            "version": 1,
            "folders": [
                { "path": "../other/proj" },
                { "path": "/absolute/elsewhere" }
            ]
        })
        .to_string();
        fs.atomic_write(file_path.to_path_buf(), raw).await.unwrap();

        let resolved = PortableWorkspace::load(fs.clone(), file_path).await.unwrap();
        assert_eq!(
            resolved,
            vec![
                PathBuf::from("/group/../other/proj"),
                PathBuf::from("/absolute/elsewhere"),
            ]
        );
    }

    #[gpui::test]
    async fn load_rejects_unknown_version(cx: &mut gpui::TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/x", serde_json::json!({})).await;
        let file_path = Path::new("/x/ws.lathe-workspace");
        fs.atomic_write(
            file_path.to_path_buf(),
            r#"{"version":99,"folders":[]}"#.to_string(),
        )
        .await
        .unwrap();

        let err = PortableWorkspace::load(fs.clone(), file_path)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unsupported portable workspace version"));
    }
}
