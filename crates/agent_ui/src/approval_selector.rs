use agent_servers::{AgentServer, CODEX_ID, GEMINI_ID};
use fs::Fs;
use gpui::{Context, Entity, Window, prelude::*};
use std::{rc::Rc, sync::Arc};
use ui::{
    Button, ContextMenu, ContextMenuEntry, PopoverMenu, PopoverMenuHandle, Tooltip, prelude::*,
};

use crate::ui::documentation_aside_side;

/// A single choice in the approval selector. `id` is the value persisted to
/// settings (`agent_servers.<agent>.approval`) and mapped to launch flags in
/// `agent_servers::custom`.
pub struct ApprovalPreset {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
}

const CODEX_PRESETS: &[ApprovalPreset] = &[
    ApprovalPreset {
        id: "default",
        name: "Ask",
        description: "Codex prompts before running commands (its own default).",
    },
    ApprovalPreset {
        id: "read_only",
        name: "Read-only",
        description: "Read-only sandbox; asks before anything that writes.",
    },
    ApprovalPreset {
        id: "auto",
        name: "Auto",
        description: "Runs in the workspace without prompting; only asks on failure.",
    },
    ApprovalPreset {
        id: "full_access",
        name: "Full access",
        description: "No prompts and no sandbox (YOLO). Use with care.",
    },
];

const GEMINI_PRESETS: &[ApprovalPreset] = &[
    ApprovalPreset {
        id: "default",
        name: "Default",
        description: "Prompt for approval on each action.",
    },
    ApprovalPreset {
        id: "auto_edit",
        name: "Auto-edit",
        description: "Auto-approve edits; prompt for other tools.",
    },
    ApprovalPreset {
        id: "yolo",
        name: "YOLO",
        description: "Auto-approve all tools. Use with care.",
    },
];

/// The approval presets offered for an agent, or an empty slice for agents that
/// have no Lathe-managed approval control.
pub fn approval_presets_for_agent(agent_id: &str) -> &'static [ApprovalPreset] {
    match agent_id {
        CODEX_ID => CODEX_PRESETS,
        GEMINI_ID => GEMINI_PRESETS,
        _ => &[],
    }
}

/// Whether a Lathe-managed approval selector should be shown for this agent.
pub fn agent_supports_approval(agent_id: &str) -> bool {
    !approval_presets_for_agent(agent_id).is_empty()
}

/// A dropdown, shown next to the model picker, that picks the approval / sandbox
/// level the agent is launched with. Unlike Claude's mode selector this is not an
/// ACP session mode: Codex and Gemini read this at process launch, so the choice
/// is persisted to settings and takes effect on newly started threads.
pub struct ApprovalSelector {
    agent_server: Rc<dyn AgentServer>,
    fs: Arc<dyn Fs>,
    menu_handle: PopoverMenuHandle<ContextMenu>,
    presets: &'static [ApprovalPreset],
}

impl ApprovalSelector {
    pub fn new(agent_server: Rc<dyn AgentServer>, fs: Arc<dyn Fs>) -> Self {
        let presets = approval_presets_for_agent(agent_server.agent_id().0.as_ref());
        Self {
            agent_server,
            fs,
            menu_handle: PopoverMenuHandle::default(),
            presets,
        }
    }

    fn current_id(&self, cx: &Context<Self>) -> String {
        self.agent_server
            .default_approval(cx)
            .filter(|id| self.presets.iter().any(|preset| preset.id == id.as_str()))
            .unwrap_or_else(|| self.default_id().to_string())
    }

    fn default_id(&self) -> &'static str {
        self.presets.first().map(|preset| preset.id).unwrap_or("")
    }

    fn set_approval(&mut self, id: &'static str, cx: &mut Context<Self>) {
        self.agent_server
            .set_default_approval(Some(id.to_string()), self.fs.clone(), cx);
        cx.notify();
    }

    fn build_context_menu(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ContextMenu> {
        let weak_self = cx.weak_entity();
        let current_id = self.current_id(cx);
        let presets = self.presets;

        ContextMenu::build(window, cx, move |mut menu, _window, cx| {
            let side = documentation_aside_side(cx);

            for preset in presets {
                let is_selected = current_id == preset.id;
                let description = preset.description;
                let entry = ContextMenuEntry::new(preset.name)
                    .toggleable(IconPosition::End, is_selected)
                    .documentation_aside(side, move |_| Label::new(description).into_any_element());

                menu.push_item(entry.handler({
                    let weak_self = weak_self.clone();
                    move |_window, cx| {
                        weak_self
                            .update(cx, |this, cx| {
                                this.set_approval(preset.id, cx);
                            })
                            .ok();
                    }
                }));
            }

            menu.key_context("ApprovalSelector")
        })
    }
}

impl Render for ApprovalSelector {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let current_id = self.current_id(cx);
        let current_name = self
            .presets
            .iter()
            .find(|preset| current_id == preset.id)
            .map(|preset| preset.name)
            .unwrap_or("Unknown");

        let this = cx.weak_entity();

        let icon = if self.menu_handle.is_deployed() {
            IconName::ChevronUp
        } else {
            IconName::ChevronDown
        };

        // Guardrails-off levels flip the metaphor to an open lock so the riskiest
        // selection (full access / YOLO) reads at a glance.
        let lock_icon = if matches!(current_id.as_str(), "full_access" | "yolo") {
            IconName::LockOff
        } else {
            IconName::Lock
        };

        let trigger_button = Button::new("approval-selector-trigger", current_name)
            .label_size(LabelSize::Small)
            .color(Color::Muted)
            .start_icon(
                Icon::new(lock_icon)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .end_icon(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted));

        PopoverMenu::new("approval-selector")
            .trigger_with_tooltip(
                trigger_button,
                Tooltip::text("Approval level (applies to new threads)"),
            )
            .anchor(gpui::Anchor::BottomRight)
            .with_handle(self.menu_handle.clone())
            .offset(gpui::Point {
                x: px(0.0),
                y: px(-2.0),
            })
            .menu(move |window, cx| {
                this.update(cx, |this, cx| this.build_context_menu(window, cx))
                    .ok()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acp_thread::AgentConnection;
    use fs::FakeFs;
    use gpui::{App, Task, TestAppContext};
    use parking_lot::Mutex;
    use project::{AgentId, Project};
    use std::any::Any;

    #[gpui::test]
    async fn set_approval_persists_via_agent_server(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        let agent_server = Rc::new(TestAgentServer::default());
        let saved = agent_server.saved.clone();

        cx.update(|cx| {
            let selector = cx.new(|_| ApprovalSelector::new(agent_server.clone(), fs.clone()));
            selector.update(cx, |selector, cx| {
                selector.set_approval("full_access", cx);
            });
        });

        assert_eq!(saved.lock().as_slice(), &[Some("full_access".to_string())]);
    }

    #[derive(Default)]
    struct TestAgentServer {
        saved: Arc<Mutex<Vec<Option<String>>>>,
    }

    impl AgentServer for TestAgentServer {
        fn logo(&self) -> IconName {
            IconName::ZedAssistant
        }

        fn agent_id(&self) -> AgentId {
            AgentId::new(CODEX_ID)
        }

        fn connect(
            &self,
            _delegate: agent_servers::AgentServerDelegate,
            _project: Entity<Project>,
            _cx: &mut App,
        ) -> Task<anyhow::Result<Rc<dyn AgentConnection>>> {
            Task::ready(Err(anyhow::anyhow!("test agent server cannot connect")))
        }

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }

        fn set_default_approval(&self, level: Option<String>, _fs: Arc<dyn Fs>, _cx: &mut App) {
            self.saved.lock().push(level);
        }
    }
}
