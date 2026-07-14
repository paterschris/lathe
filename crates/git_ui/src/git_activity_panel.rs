use crate::new_panel_settings::GitActivityPanelSettings;
use gpui::{
    Action, AsyncWindowContext, Entity, EventEmitter, FocusHandle, Focusable, SharedString,
    Subscription, WeakEntity, actions,
};
use project::{
    Project,
    git_store::{GitStore, GitStoreEvent, JobInfo},
};
use settings::Settings;
use std::time::Duration;
use ui::{CommonAnimationExt, prelude::*};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    git_activity_panel,
    [
        /// Toggles focus on the git activity panel.
        ToggleFocus,
    ]
);

const GIT_ACTIVITY_PANEL_KEY: &str = "GitActivityPanel";

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
        workspace.toggle_panel_focus::<GitActivityPanel>(window, cx);
    });
}

/// Live view of every in-flight git operation across the workspace's
/// repositories. Subscribes to `GitStoreEvent::JobsUpdated` and re-reads each
/// repo's `active_jobs()` whenever that fires; rows show one entry per
/// in-flight command with its repository, status line, and elapsed time.
pub struct GitActivityPanel {
    project: Entity<Project>,
    git_store: Entity<GitStore>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone)]
struct ActivityRow {
    repo_label: SharedString,
    job: JobInfo,
}

impl GitActivityPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            Self::new(workspace, window, cx)
        })
    }

    pub fn new(
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let project = workspace.project().clone();
        let git_store = project.read(cx).git_store().clone();
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();
            let subscriptions = vec![cx.subscribe(&git_store, Self::on_git_store_event)];
            Self {
                project,
                git_store,
                focus_handle,
                _subscriptions: subscriptions,
            }
        })
    }

    fn on_git_store_event(
        &mut self,
        _: Entity<GitStore>,
        event: &GitStoreEvent,
        cx: &mut Context<Self>,
    ) {
        if matches!(
            event,
            GitStoreEvent::JobsUpdated
                | GitStoreEvent::RepositoryAdded
                | GitStoreEvent::RepositoryRemoved(_)
        ) {
            cx.notify();
        }
    }

    fn collect_rows(&self, cx: &App) -> Vec<ActivityRow> {
        let mut rows = Vec::new();
        for repo in self.git_store.read(cx).repositories().values() {
            let repo_ref = repo.read(cx);
            let label = repo_ref.display_name();
            for job in repo_ref.active_jobs() {
                rows.push(ActivityRow {
                    repo_label: label.clone(),
                    job,
                });
            }
        }
        rows.sort_by_key(|row| row.job.start);
        rows
    }

    fn render_row(&self, ix: usize, row: &ActivityRow, cx: &Context<Self>) -> impl IntoElement {
        let elapsed = row.job.start.elapsed();
        let elapsed_label = format_elapsed(elapsed);

        h_flex()
            .id(("git-activity-row", ix))
            .h(rems(2.4))
            .px_2()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Icon::new(IconName::ArrowCircle)
                    .size(IconSize::Small)
                    .color(Color::Accent)
                    .with_rotate_animation(2),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .flex_1()
                    .gap_0p5()
                    .child(
                        Label::new(row.job.message.clone())
                            .size(LabelSize::Small)
                            .truncate(),
                    )
                    .child(
                        Label::new(row.repo_label.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
            .child(
                Label::new(elapsed_label)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
    }
}

fn format_elapsed(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        let mins = secs / 60;
        let rem = secs % 60;
        format!("{mins}m {rem}s")
    }
}

impl Focusable for GitActivityPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for GitActivityPanel {}

impl Render for GitActivityPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Subscribing to `JobsUpdated` redraws when jobs start/finish, but
        // elapsed-time labels only refresh on those redraws. For a beta this
        // is fine — start/finish events fire often enough that the elapsed
        // counter rarely lags by more than a second.
        let rows = self.collect_rows(cx);
        let panel_bg = cx.theme().colors().panel_background;
        let _ = &self.project;

        let header = h_flex()
            .h(rems(2.))
            .px_2()
            .gap_1p5()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Icon::new(IconName::ArrowCircle)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(Label::new("Git Activity").size(LabelSize::Small))
            .child(
                Label::new(format!("({})", rows.len()))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );

        let body = if rows.is_empty() {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(
                    Label::new("No active git operations")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element()
        } else {
            v_flex()
                .children(
                    rows.iter()
                        .enumerate()
                        .map(|(ix, row)| self.render_row(ix, row, cx)),
                )
                .into_any_element()
        };

        v_flex()
            .key_context("GitActivityPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(panel_bg)
            .child(header)
            .child(body)
    }
}

impl Panel for GitActivityPanel {
    fn persistent_name() -> &'static str {
        GIT_ACTIVITY_PANEL_KEY
    }

    fn panel_key() -> &'static str {
        GIT_ACTIVITY_PANEL_KEY
    }

    fn position(&self, _: &Window, cx: &App) -> DockPosition {
        GitActivityPanelSettings::get_global(cx).dock
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(
            position,
            DockPosition::Bottom | DockPosition::Left | DockPosition::Right
        )
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, cx: &App) -> Pixels {
        GitActivityPanelSettings::get_global(cx).default_width
    }

    fn icon(&self, _: &Window, cx: &App) -> Option<ui::IconName> {
        GitActivityPanelSettings::get_global(cx)
            .button
            .then_some(ui::IconName::ArrowCircle)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Git Activity")
    }

    fn icon_label(&self, _: &Window, cx: &App) -> Option<String> {
        let mut count = 0usize;
        for repo in self.git_store.read(cx).repositories().values() {
            count += repo.read(cx).active_jobs().len();
        }
        (count > 0).then(|| count.to_string())
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn starts_open(&self, _: &Window, _: &App) -> bool {
        false
    }

    fn activation_priority(&self) -> u32 {
        // Must be unique across all registered panels (dock.rs panics in debug
        // builds otherwise). 6 collides with OutlinePanel; 9 is a free slot.
        9
    }
}
