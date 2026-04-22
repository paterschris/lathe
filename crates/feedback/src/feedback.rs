use gpui::{App, ClipboardItem, PromptLevel, actions};
use system_specs::{CopySystemSpecsIntoClipboard, SystemSpecs};
use util::ResultExt;
use workspace::Workspace;
use zed_actions::feedback::{FileBugReport, RequestFeature};

actions!(
    zed,
    [
        /// Opens the Lathe repository on GitHub.
        OpenZedRepo,
    ]
);

const LATHE_REPO_URL: &str = "https://github.com/paterschris/lathe";

const REQUEST_FEATURE_URL: &str = "https://github.com/paterschris/lathe/issues/new";

fn file_bug_report_url(specs: &SystemSpecs) -> String {
    format!(
        concat!(
            "https://github.com/paterschris/lathe/issues/new",
            "?",
            "body={}"
        ),
        urlencoding::encode(&format!("## Description\n\n\n\n## System Information\n\n{specs}"))
    )
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace
            .register_action(|_, _: &CopySystemSpecsIntoClipboard, window, cx| {
                let specs = SystemSpecs::new(window, cx);

                cx.spawn_in(window, async move |_, cx| {
                    let specs = specs.await.to_string();

                    cx.update(|_, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(specs.clone()))
                    })
                    .log_err();

                    cx.prompt(
                        PromptLevel::Info,
                        "Copied into clipboard",
                        Some(&specs),
                        &["OK"],
                    )
                    .await
                })
                .detach();
            })
            .register_action(|_, _: &RequestFeature, _, cx| {
                cx.open_url(REQUEST_FEATURE_URL);
            })
            .register_action(move |_, _: &FileBugReport, window, cx| {
                let specs = SystemSpecs::new(window, cx);
                cx.spawn_in(window, async move |_, cx| {
                    let specs = specs.await;
                    cx.update(|_, cx| {
                        cx.open_url(&file_bug_report_url(&specs));
                    })
                    .log_err();
                })
                .detach();
            })
            .register_action(move |_, _: &OpenZedRepo, _, cx| {
                cx.open_url(LATHE_REPO_URL);
            });
    })
    .detach();
}
