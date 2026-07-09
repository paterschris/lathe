use std::sync::Arc;

use agent_settings::{AgentSettings, WindowLayout};
use db::kvp::Dismissable;
use fs::Fs;
use gpui::{
    App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, Render, Window, prelude::*,
};
use notifications::status_toast::StatusToast;
use release_channel::ReleaseChannel;
use semver::Version;
use ui::{AnnouncementToast, ListBulletItem, ParallelAgentsIllustration, prelude::*};
use workspace::{
    FocusWorkspaceSidebar, Workspace,
    notifications::{Notification, SuppressEvent},
};
use zed_actions::assistant::FocusAgent;

#[derive(Clone)]
pub(super) struct AnnouncementContent {
    heading: SharedString,
    description: SharedString,
    bullet_items: Vec<SharedString>,
    primary_action_label: SharedString,
    primary_action_url: Option<SharedString>,
    primary_action_callback: Option<Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>>,
    secondary_action_url: Option<SharedString>,
    on_dismiss: Option<Arc<dyn Fn(&mut App) + Send + Sync>>,
}

struct ParallelAgentAnnouncement;

impl Dismissable for ParallelAgentAnnouncement {
    const KEY: &'static str = "parallel-agent-announcement";
}

pub(super) fn announcement_for_version(
    version: &Version,
    cx: &App,
) -> Option<AnnouncementContent> {
    let version_with_parallel_agents = match ReleaseChannel::global(cx) {
        ReleaseChannel::Stable | ReleaseChannel::Beta => Version::new(0, 233, 0),
        ReleaseChannel::Dev | ReleaseChannel::Nightly | ReleaseChannel::Preview => {
            Version::new(0, 232, 0)
        }
    };

    if *version >= version_with_parallel_agents && !ParallelAgentAnnouncement::dismissed(cx) {
        let fs = <dyn Fs>::global(cx);
        Some(AnnouncementContent {
            heading: "Introducing Parallel Agents".into(),
            description: "Run multiple threads of your favorite agents simultaneously across projects in a new workspace layout, tailored for agentic workflows.".into(),
            bullet_items: vec![
                "Use your favorite agents in parallel".into(),
                "Optionally isolate agents using worktrees".into(),
                "Combine multiple projects in one window".into(),
            ],
            primary_action_label: "Try Agentic Layout".into(),
            primary_action_url: None,
            primary_action_callback: Some(Arc::new(move |window, cx| {
                let get_layout = AgentSettings::get_layout(cx);
                let already_agent_layout = matches!(get_layout, WindowLayout::Agent(_));

                let update;
                if !already_agent_layout {
                    update = Some(AgentSettings::set_layout(
                        WindowLayout::Agent(None),
                        fs.clone(),
                        cx,
                    ));
                } else {
                    update = None;
                }

                let revert_fs = fs.clone();
                window
                    .spawn(cx, async move |cx| {
                        if let Some(update) = update {
                            update.await.ok();
                        }

                        cx.update(|window, cx| {
                            if !already_agent_layout {
                                if let Some(workspace) = Workspace::for_window(window, cx) {
                                    let toast = StatusToast::new(
                                        "You are in the new agentic layout!",
                                        cx,
                                        move |this, _cx| {
                                            this.icon(
                                                Icon::new(IconName::Check)
                                                    .size(IconSize::Small)
                                                    .color(Color::Success),
                                            )
                                            .action("Revert", move |_window, cx| {
                                                let _ = AgentSettings::set_layout(
                                                    get_layout.clone(),
                                                    revert_fs.clone(),
                                                    cx,
                                                );
                                            })
                                            .auto_dismiss(false)
                                            .dismiss_button(true)
                                        },
                                    );

                                    workspace.update(cx, |workspace, cx| {
                                        workspace.toggle_status_toast(toast, cx);
                                    });
                                }
                            }

                            window.dispatch_action(Box::new(FocusWorkspaceSidebar), cx);
                            window.dispatch_action(Box::new(FocusAgent), cx);
                        })
                    })
                    .detach();
            })),
            on_dismiss: Some(Arc::new(|cx| {
                ParallelAgentAnnouncement::set_dismissed(true, cx)
            })),
            secondary_action_url: Some("https://zed.dev/blog/".into()),
        })
    } else {
        None
    }
}

pub(super) struct AnnouncementToastNotification {
    focus_handle: FocusHandle,
    content: AnnouncementContent,
}

impl AnnouncementToastNotification {
    pub(super) fn new(content: AnnouncementContent, cx: &mut App) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content,
        }
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
        if let Some(on_dismiss) = &self.content.on_dismiss {
            on_dismiss(cx);
        }
    }
}

impl Focusable for AnnouncementToastNotification {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for AnnouncementToastNotification {}
impl EventEmitter<SuppressEvent> for AnnouncementToastNotification {}
impl Notification for AnnouncementToastNotification {}

impl Render for AnnouncementToastNotification {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        AnnouncementToast::new()
            .illustration(ParallelAgentsIllustration::new())
            .heading(self.content.heading.clone())
            .description(self.content.description.clone())
            .bullet_items(
                self.content
                    .bullet_items
                    .iter()
                    .map(|item| ListBulletItem::new(item.clone())),
            )
            .primary_action_label(self.content.primary_action_label.clone())
            .primary_on_click(cx.listener({
                let url = self.content.primary_action_url.clone();
                let callback = self.content.primary_action_callback.clone();
                move |this, _, window, cx| {
                    telemetry::event!("Parallel Agent Announcement Main Click");
                    if let Some(callback) = &callback {
                        callback(window, cx);
                    }
                    if let Some(url) = &url {
                        cx.open_url(url);
                    }
                    this.dismiss(cx);
                }
            }))
            .secondary_on_click(cx.listener({
                let url = self.content.secondary_action_url.clone();
                move |_, _, _window, cx| {
                    telemetry::event!("Parallel Agent Announcement Secondary Click");
                    if let Some(url) = &url {
                        cx.open_url(url);
                    }
                }
            }))
            .dismiss_on_click(cx.listener(|this, _, _window, cx| {
                telemetry::event!("Parallel Agent Announcement Dismiss");
                this.dismiss(cx);
            }))
    }
}
