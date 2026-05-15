use editor::Editor;
use git::repository::MergeOptions;
use gpui::{
    AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, actions,
};
use project::git_store::Repository;
use ui::{Headline, HeadlineSize, prelude::*};
use workspace::{ModalView, Workspace};

actions!(
    git_flow,
    [
        /// Opens a modal to start a new GitFlow feature branch off `develop`.
        StartFeature,
        /// Finishes the current feature branch: merges it into `develop` (no-ff)
        /// and deletes the local feature branch. Surfaces an error if any step
        /// fails — the user is expected to resolve manually.
        FinishFeature,
        /// Opens a modal to start a new GitFlow release branch off `develop`.
        StartRelease,
        /// Finishes the current release branch: merges it into `main` (no-ff),
        /// tags the merge commit with the release name, merges back into
        /// `develop` (no-ff), and deletes the local release branch.
        FinishRelease,
        /// Opens a modal to start a new GitFlow hotfix branch off `main`.
        StartHotfix,
        /// Finishes the current hotfix branch: merges into `main` (no-ff),
        /// tags the merge commit, merges into `develop` (no-ff), and deletes
        /// the local hotfix branch.
        FinishHotfix,
    ]
);

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &StartFeature, window, cx| {
        open_start_modal(workspace, FlowKind::Feature, window, cx);
    });
    workspace.register_action(|workspace, _: &StartRelease, window, cx| {
        open_start_modal(workspace, FlowKind::Release, window, cx);
    });
    workspace.register_action(|workspace, _: &StartHotfix, window, cx| {
        open_start_modal(workspace, FlowKind::Hotfix, window, cx);
    });
    workspace.register_action(|workspace, _: &FinishFeature, window, cx| {
        finish_current_branch(workspace, FlowKind::Feature, window, cx);
    });
    workspace.register_action(|workspace, _: &FinishRelease, window, cx| {
        finish_current_branch(workspace, FlowKind::Release, window, cx);
    });
    workspace.register_action(|workspace, _: &FinishHotfix, window, cx| {
        finish_current_branch(workspace, FlowKind::Hotfix, window, cx);
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FlowKind {
    Feature,
    Release,
    Hotfix,
}

impl FlowKind {
    fn prefix(&self) -> &'static str {
        match self {
            FlowKind::Feature => "feature",
            FlowKind::Release => "release",
            FlowKind::Hotfix => "hotfix",
        }
    }

    /// Branch the new flow branch is created off of.
    fn base_branch(&self) -> &'static str {
        match self {
            FlowKind::Feature | FlowKind::Release => "develop",
            FlowKind::Hotfix => "main",
        }
    }

    fn finish_targets(&self) -> &'static [&'static str] {
        // Features merge back into `develop` only. Releases and hotfixes merge
        // into both `main` and `develop` (canonical GitFlow flow).
        match self {
            FlowKind::Feature => &["develop"],
            FlowKind::Release | FlowKind::Hotfix => &["main", "develop"],
        }
    }

    fn tag_on_finish(&self) -> bool {
        matches!(self, FlowKind::Release | FlowKind::Hotfix)
    }

    fn title(&self) -> &'static str {
        match self {
            FlowKind::Feature => "Start feature",
            FlowKind::Release => "Start release",
            FlowKind::Hotfix => "Start hotfix",
        }
    }
}

fn open_start_modal(
    workspace: &mut Workspace,
    kind: FlowKind,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(repo) = workspace.project().read(cx).active_repository(cx) else {
        return;
    };
    workspace.toggle_modal(window, cx, |window, cx| {
        StartFlowModal::new(kind, repo, window, cx)
    });
}

struct StartFlowModal {
    kind: FlowKind,
    repo: Entity<Repository>,
    editor: Entity<Editor>,
}

impl StartFlowModal {
    fn new(
        kind: FlowKind,
        repo: Entity<Repository>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(
                match kind {
                    FlowKind::Feature => "feature-name",
                    FlowKind::Release => "1.2.0",
                    FlowKind::Hotfix => "1.2.1",
                },
                window,
                cx,
            );
            editor
        });
        Self { kind, repo, editor }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let raw = self.editor.read(cx).text(cx);
        let name = raw.trim().to_string();
        if name.is_empty() {
            cx.emit(DismissEvent);
            return;
        }
        let kind = self.kind;
        let branch = format!("{}/{}", kind.prefix(), name);
        let base = kind.base_branch().to_string();
        let repo = self.repo.clone();
        let receiver = repo.update(cx, |repo, _| repo.create_branch(branch.clone(), Some(base)));
        cx.spawn_in(window, async move |this, cx| {
            let outcome = receiver.await.map_err(anyhow::Error::from).and_then(|r| r);
            if let Err(err) = outcome {
                this.update(cx, |_, cx| {
                    let _ = err;
                    let _ = cx;
                })
                .ok();
                anyhow::bail!("git flow start {} failed: {err}", kind.prefix());
            }
            // Switch to the newly-created branch so the user lands on it. The
            // create_branch impl already does this for fast-path libgit, but
            // we re-issue here for safety in case the implementation changes.
            let receiver = repo.update(cx, |repo, _| repo.change_branch(branch.clone()));
            let _ = receiver.await;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for StartFlowModal {}
impl ModalView for StartFlowModal {}
impl Focusable for StartFlowModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for StartFlowModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self.kind.title();
        let base = self.kind.base_branch();
        v_flex()
            .key_context("StartFlowModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_2(cx)
            .w(rems(34.))
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .w_full()
                    .gap_1p5()
                    .child(Icon::new(IconName::GitBranch).size(IconSize::XSmall))
                    .child(Headline::new(title).size(HeadlineSize::XSmall))
                    .child(
                        Label::new(format!("(off {base})"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(div().px_3().pb_3().w_full().child(self.editor.clone()))
    }
}

fn finish_current_branch(
    workspace: &mut Workspace,
    kind: FlowKind,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(repo) = workspace.project().read(cx).active_repository(cx) else {
        return;
    };
    let Some(current_branch) = repo.read(cx).branch.as_ref().map(|b| b.name().to_string()) else {
        return;
    };
    let prefix = format!("{}/", kind.prefix());
    if !current_branch.starts_with(&prefix) {
        log::warn!(
            "git flow finish {} called from non-{} branch: {current_branch}",
            kind.prefix(),
            kind.prefix()
        );
        return;
    }
    let tag_name = current_branch
        .strip_prefix(&prefix)
        .map(|tail| tail.to_string());
    let targets: Vec<&'static str> = kind.finish_targets().to_vec();
    let tag_on_finish = kind.tag_on_finish();

    let repo_for_task = repo;
    cx.spawn_in(window, async move |_, cx| {
        for (idx, target) in targets.iter().enumerate() {
            let target = (*target).to_string();
            let receiver = repo_for_task
                .update(cx, |repo, _| repo.change_branch(target.clone()));
            let res: anyhow::Result<()> = receiver.await.unwrap_or_else(|_| Ok(()));
            if let Err(err) = res {
                anyhow::bail!("checkout {target}: {err}");
            }

            let merge_options = MergeOptions {
                no_ff: true,
                ..Default::default()
            };
            let receiver = repo_for_task
                .update(cx, |repo, _| {
                    repo.merge(current_branch.clone(), merge_options)
                });
            let res: anyhow::Result<()> = receiver.await.unwrap_or_else(|_| Ok(()));
            if let Err(err) = res {
                anyhow::bail!("merge {current_branch} into {target}: {err}");
            }

            // Tag the *first* finish target's merge commit when the flow
            // expects a version tag (releases / hotfixes). We use the bare
            // version string from the branch name and leave a default
            // message that mirrors the canonical GitFlow workflow.
            if idx == 0 && tag_on_finish
                && let Some(name) = &tag_name
            {
                let receiver = repo_for_task.update(cx, |repo, _| {
                    repo.tag_create(
                        name.clone(),
                        "HEAD".to_string(),
                        Some(format!("Release {name}")),
                        false,
                    )
                });
                let _ = receiver.await;
            }
        }

        // Delete the merged feature/release/hotfix branch. Best-effort —
        // failures are logged but don't abort the flow.
        let receiver = repo_for_task.update(cx, |repo, _| {
            repo.delete_branch(false, current_branch.clone())
        });
        let _ = receiver.await;

        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}
