use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use gpui::{App, AppContext as _, Entity, Task};
use language::{Buffer, ContextProvider};
use project::Fs;

use crate::language_settings::LanguageSettings;
use task::{RevealStrategy, TaskTemplate, TaskTemplates};

/// Name of the file that marks the root of a PlatformIO project.
const PROJECT_FILE: &str = "platformio.ini";

/// Task variable pointing at a specific `pio` executable, for installations that
/// are not on `$PATH`.
const PLATFORMIO_PATH_TASK_VARIABLE: &str = "PLATFORMIO_PATH";

/// Provides PlatformIO tasks to any buffer that lives underneath a
/// `platformio.ini`. Registered for C, C++ and `platformio.ini` itself, so the
/// tasks are reachable from the files an embedded project is actually edited in.
pub(crate) struct PlatformIoContextProvider {
    fs: Arc<dyn Fs>,
}

impl PlatformIoContextProvider {
    pub fn new(fs: Arc<dyn Fs>) -> Self {
        Self { fs }
    }
}

impl ContextProvider for PlatformIoContextProvider {
    fn associated_tasks(
        &self,
        buffer: Option<Entity<Buffer>>,
        cx: &App,
    ) -> Task<Option<TaskTemplates>> {
        let Some(buffer) = buffer else {
            return Task::ready(None);
        };
        let buffer = buffer.read(cx);

        let language_name = buffer.language().map(|language| language.name());
        let pio_override = LanguageSettings::resolve(Some(buffer), language_name.as_ref(), cx)
            .tasks
            .variables
            .get(PLATFORMIO_PATH_TASK_VARIABLE)
            .cloned();

        let Some(file) = project::File::from_dyn(buffer.file()) else {
            return Task::ready(None);
        };
        let Some(worktree_root) = file.worktree.read(cx).root_dir() else {
            return Task::ready(None);
        };
        let file_abs_path = worktree_root.join(file.path.as_std_path());

        let fs = self.fs.clone();
        cx.background_spawn(async move {
            platformio_tasks(fs.as_ref(), &file_abs_path, &worktree_root, pio_override).await
        })
    }
}

async fn platformio_tasks(
    fs: &dyn Fs,
    file_abs_path: &Path,
    worktree_root: &Path,
    pio_override: Option<String>,
) -> Option<TaskTemplates> {
    let project_dir = find_project_dir(fs, file_abs_path, worktree_root).await?;
    let contents = fs.load(&project_dir.join(PROJECT_FILE)).await.ok()?;
    let environments = parse_environments(&contents);
    let pio = resolve_pio_command(fs, pio_override).await;

    Some(task_templates(&pio, &project_dir, &environments))
}

/// Walks up from `file_abs_path` looking for the nearest `platformio.ini`,
/// stopping at `worktree_root` so we never escape the project the user opened.
async fn find_project_dir(
    fs: &dyn Fs,
    file_abs_path: &Path,
    worktree_root: &Path,
) -> Option<PathBuf> {
    let mut dir = file_abs_path.parent()?;
    loop {
        if fs.is_file(&dir.join(PROJECT_FILE)).await {
            return Some(dir.to_owned());
        }
        if dir == worktree_root {
            return None;
        }
        dir = dir.parent()?;
    }
}

/// PlatformIO's own installer does not put `pio` on `$PATH`, so fall back to the
/// virtualenv it creates before giving up and hoping the shell can find it.
async fn resolve_pio_command(fs: &dyn Fs, pio_override: Option<String>) -> String {
    if let Some(pio_override) = pio_override {
        return pio_override;
    }

    #[cfg(windows)]
    let penv_pio = util::paths::home_dir()
        .join(".platformio")
        .join("penv")
        .join("Scripts")
        .join("pio.exe");
    #[cfg(not(windows))]
    let penv_pio = util::paths::home_dir()
        .join(".platformio")
        .join("penv")
        .join("bin")
        .join("pio");

    if fs.is_file(&penv_pio).await {
        return penv_pio.to_string_lossy().into_owned();
    }
    "pio".to_owned()
}

/// The `[env:*]` sections of a `platformio.ini`, ordered so that anything listed
/// in `[platformio] default_envs` comes first.
fn parse_environments(contents: &str) -> Vec<String> {
    let mut environments = Vec::new();
    let mut default_environments = Vec::new();
    let mut section = String::new();
    // `platformio.ini` continues a value onto following lines as long as they
    // are indented, so `default_envs` may span several lines.
    let mut in_default_envs = false;

    for line in contents.lines() {
        let is_indented = line.starts_with(' ') || line.starts_with('\t');
        let line = match line.split_once(|c| c == ';' || c == '#') {
            Some((before_comment, _)) => before_comment,
            None => line,
        };
        let line = line.trim();

        if line.is_empty() {
            continue;
        }

        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.trim().to_owned();
            in_default_envs = false;
            if let Some(environment) = section.strip_prefix("env:") {
                let environment = environment.trim();
                if !environment.is_empty() {
                    environments.push(environment.to_owned());
                }
            }
            continue;
        }

        if in_default_envs && is_indented {
            default_environments.extend(split_environment_list(line));
            continue;
        }
        in_default_envs = false;

        if section != "platformio" {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            if key.trim() == "default_envs" {
                default_environments.extend(split_environment_list(value));
                in_default_envs = true;
            }
        }
    }

    default_environments.retain(|environment| environments.contains(environment));
    let remaining = environments
        .into_iter()
        .filter(|environment| !default_environments.contains(environment))
        .collect::<Vec<_>>();
    default_environments.extend(remaining);
    default_environments
}

fn split_environment_list(value: &str) -> impl Iterator<Item = String> + '_ {
    value
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
}

fn task_templates(pio: &str, project_dir: &Path, environments: &[String]) -> TaskTemplates {
    let cwd = project_dir.to_string_lossy().into_owned();
    let template = |label: String, args: Vec<&str>| TaskTemplate {
        label,
        command: pio.to_owned(),
        args: args.into_iter().map(str::to_owned).collect(),
        cwd: Some(cwd.clone()),
        reveal: RevealStrategy::Always,
        ..TaskTemplate::default()
    };

    let mut templates = Vec::new();

    for environment in environments {
        let env = environment.as_str();
        templates.extend([
            template(
                format!("PlatformIO: Build ({environment})"),
                vec!["run", "-e", env],
            ),
            template(
                format!("PlatformIO: Upload ({environment})"),
                vec!["run", "-t", "upload", "-e", env],
            ),
            template(
                format!("PlatformIO: Upload and Monitor ({environment})"),
                vec!["run", "-t", "upload", "-t", "monitor", "-e", env],
            ),
            TaskTemplate {
                // The serial monitor is interactive and long-lived, so it gets a
                // terminal of its own rather than replacing the build output.
                use_new_terminal: true,
                ..template(
                    format!("PlatformIO: Monitor ({environment})"),
                    vec!["device", "monitor", "-e", env],
                )
            },
            template(
                format!("PlatformIO: Test ({environment})"),
                vec!["test", "-e", env],
            ),
            template(
                format!("PlatformIO: Check ({environment})"),
                vec!["check", "-e", env],
            ),
            template(
                format!("PlatformIO: Clean ({environment})"),
                vec!["run", "-t", "clean", "-e", env],
            ),
        ]);
    }

    templates.extend([
        template(
            "PlatformIO: Build (all environments)".to_owned(),
            vec!["run"],
        ),
        template(
            "PlatformIO: Clean (all environments)".to_owned(),
            vec!["run", "-t", "fullclean"],
        ),
        // Regenerates compile_commands.json, which is how clangd learns the
        // include paths and defines for the selected board.
        template(
            "PlatformIO: Rebuild IntelliSense index".to_owned(),
            vec!["run", "-t", "compiledb"],
        ),
        template(
            "PlatformIO: List devices".to_owned(),
            vec!["device", "list"],
        ),
    ]);

    TaskTemplates(templates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;

    /// Converts a Unix-style path to a platform-specific path, so the fake
    /// filesystem tests also work on Windows.
    fn unix_path_to_platform(path: &str) -> String {
        #[cfg(windows)]
        {
            if path.starts_with('/') {
                format!("C:{}", path.replace('/', "\\"))
            } else {
                path.replace('/', "\\")
            }
        }
        #[cfg(not(windows))]
        {
            path.to_string()
        }
    }

    const BLINK_INI: &str =
        "[platformio]\ndefault_envs = uno\n\n[env:uno]\nboard = uno\n\n[env:due]\nboard = due\n";

    async fn tasks_for(
        cx: &mut TestAppContext,
        tree_root: &str,
        tree: serde_json::Value,
        worktree_root: &str,
        file: &str,
    ) -> Option<TaskTemplates> {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(unix_path_to_platform(tree_root), tree).await;
        platformio_tasks(
            fs.as_ref(),
            Path::new(&unix_path_to_platform(file)),
            Path::new(&unix_path_to_platform(worktree_root)),
            Some("pio".to_owned()),
        )
        .await
    }

    #[gpui::test]
    async fn test_tasks_are_generated_for_a_project_at_the_worktree_root(cx: &mut TestAppContext) {
        let templates = tasks_for(
            cx,
            "/blink",
            json!({ "platformio.ini": BLINK_INI, "src": { "main.cpp": "" } }),
            "/blink",
            "/blink/src/main.cpp",
        )
        .await
        .expect("a project at the worktree root should produce tasks");

        let labels = templates
            .0
            .iter()
            .map(|template| template.label.as_str())
            .collect::<Vec<_>>();
        // `default_envs` puts uno ahead of due.
        assert_eq!(labels[0], "PlatformIO: Build (uno)");
        assert!(labels.contains(&"PlatformIO: Build (due)"));
        assert!(labels.contains(&"PlatformIO: Rebuild IntelliSense index"));

        let project_dir = unix_path_to_platform("/blink");
        assert!(
            templates
                .0
                .iter()
                .all(|template| template.cwd.as_deref() == Some(project_dir.as_str()))
        );
    }

    #[gpui::test]
    async fn test_tasks_are_generated_for_a_project_in_a_subdirectory(cx: &mut TestAppContext) {
        let templates = tasks_for(
            cx,
            "/repo",
            json!({
                "README.md": "",
                "firmware": { "platformio.ini": BLINK_INI, "src": { "main.cpp": "" } },
            }),
            "/repo",
            "/repo/firmware/src/main.cpp",
        )
        .await
        .expect("a project below the worktree root should produce tasks");

        let project_dir = unix_path_to_platform("/repo/firmware");
        assert!(
            templates
                .0
                .iter()
                .all(|template| template.cwd.as_deref() == Some(project_dir.as_str()))
        );
    }

    #[gpui::test]
    async fn test_no_tasks_without_a_platformio_ini(cx: &mut TestAppContext) {
        let templates = tasks_for(
            cx,
            "/plain",
            json!({ "src": { "main.cpp": "" } }),
            "/plain",
            "/plain/src/main.cpp",
        )
        .await;
        assert!(
            templates.is_none(),
            "a plain C++ project should not be offered PlatformIO tasks"
        );
    }

    #[gpui::test]
    async fn test_search_does_not_escape_the_worktree_root(cx: &mut TestAppContext) {
        let templates = tasks_for(
            cx,
            "/outer",
            json!({
                "platformio.ini": BLINK_INI,
                "nested": { "src": { "main.cpp": "" } },
            }),
            "/outer/nested",
            "/outer/nested/src/main.cpp",
        )
        .await;
        assert!(
            templates.is_none(),
            "a platformio.ini above the worktree root should be ignored"
        );
    }

    #[test]
    fn test_parse_environments_orders_default_envs_first() {
        let environments = parse_environments(indoc::indoc! {r#"
            [platformio]
            default_envs = nodemcuv2

            [env:esp32dev]
            platform = espressif32

            [env:nodemcuv2]
            platform = espressif8266
        "#});
        assert_eq!(environments, vec!["nodemcuv2", "esp32dev"]);
    }

    #[test]
    fn test_parse_environments_handles_continuations_and_comments() {
        let environments = parse_environments(indoc::indoc! {r#"
            ; a leading comment
            [platformio]
            default_envs =
                uno
                due
            src_dir = src

            [common]
            framework = arduino

            [env:uno]
            board = uno

            [env:due]
            board = due

            [env:mega]  # trailing comment
            board = megaatmega2560
        "#});
        assert_eq!(environments, vec!["uno", "due", "mega"]);
    }

    #[test]
    fn test_parse_environments_ignores_unknown_default_envs() {
        let environments = parse_environments(indoc::indoc! {r#"
            [platformio]
            default_envs = does_not_exist

            [env:real]
            board = uno
        "#});
        assert_eq!(environments, vec!["real"]);
    }

    #[test]
    fn test_parse_environments_without_platformio_section() {
        let environments = parse_environments(indoc::indoc! {r#"
            [env:a]
            board = uno

            [env:b]
            board = due
        "#});
        assert_eq!(environments, vec!["a", "b"]);
    }

    #[test]
    fn test_platformio_queries_compile_against_the_ini_grammar() {
        crate::language("platformio", tree_sitter_ini::LANGUAGE.into());
    }

    #[test]
    fn test_task_templates_are_scoped_to_the_project_dir() {
        let templates = task_templates("pio", Path::new("/projects/blink"), &["uno".to_owned()]);
        assert!(templates.0.iter().all(|template| template.cwd.as_deref()
            == Some("/projects/blink")
            && template.command == "pio"));

        let upload = templates
            .0
            .iter()
            .find(|template| template.label == "PlatformIO: Upload (uno)")
            .expect("upload task should exist");
        assert_eq!(upload.args, ["run", "-t", "upload", "-e", "uno"]);
    }
}
