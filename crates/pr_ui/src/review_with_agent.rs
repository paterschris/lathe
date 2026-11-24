//! Picks an agent to review a pull request in a new agent thread.

use std::sync::Arc;

use gpui::{
    App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Task, WeakEntity,
};
use picker::{Picker, PickerDelegate};
use project::{AgentRegistryStore, Project};
use ui::{ListItem, ListItemSpacing, prelude::*};
use workspace::{ModalView, Workspace};
use zed_actions::agent::ReviewPullRequestWithAgent;

/// What the review prompt says about the pull request.
pub struct PullRequestToReview {
    pub number: u32,
    pub title: SharedString,
    pub url: String,
    pub source_branch: SharedString,
    pub target_branch: SharedString,
    /// Known once the detail has loaded; the list doesn't carry it.
    pub head_sha: Option<SharedString>,
    pub repository_name: SharedString,
    pub host_name: SharedString,
}

impl PullRequestToReview {
    /// The first message of the review thread. Kept in step with the Lathe
    /// Companion app's prompt: plain git works on every host without a host
    /// login, and the agent is told to leave the working tree and host alone.
    fn prompt(&self) -> String {
        let head = self
            .head_sha
            .as_ref()
            .map(|sha| {
                let short = sha.get(..12).unwrap_or(sha.as_ref());
                format!("; the head commit is {short}")
            })
            .unwrap_or_default();
        let number = self.number;
        let title = &self.title;
        let url = &self.url;
        let repository = &self.repository_name;
        let source = &self.source_branch;
        let target = &self.target_branch;
        let host = &self.host_name;
        [
            format!("Review pull request #{number} \"{title}\" in the {repository} repository ({url})."),
            format!("It merges {source} into {target}{head}."),
            String::new(),
            format!(
                "Fetch both branches from the remote and review the change with `git diff {target}...{source}` against the fetched remote branches. Don't check out the branch or change my working tree; if you need to build or run tests, use a separate git worktree and remove it when you're done."
            ),
            String::new(),
            format!(
                "Report bugs, risks and suggestions grouped by file, with line numbers, most important first. End with whether you'd approve it. Don't post anything to {host} unless I ask."
            ),
        ]
        .join("\n")
    }
}

/// Opens the agent picker for reviewing `pull_request`.
pub fn open(
    workspace: &mut Workspace,
    pull_request: PullRequestToReview,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    workspace.toggle_modal(window, cx, move |window, cx| {
        ReviewWithAgentModal::new(pull_request, &project, window, cx)
    });
}

pub struct ReviewWithAgentModal {
    picker: Entity<Picker<ReviewWithAgentDelegate>>,
}

impl ReviewWithAgentModal {
    fn new(
        pull_request: PullRequestToReview,
        project: &Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let agents = available_agents(project, cx);
        let delegate = ReviewWithAgentDelegate {
            modal: cx.entity().downgrade(),
            filtered: (0..agents.len()).collect(),
            agents,
            selected_index: 0,
            pull_request,
        };
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        Self { picker }
    }
}

impl EventEmitter<DismissEvent> for ReviewWithAgentModal {}

impl Focusable for ReviewWithAgentModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl ModalView for ReviewWithAgentModal {}

impl Render for ReviewWithAgentModal {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("ReviewWithAgentModal")
            .w(rems(30.))
            .child(self.picker.clone())
    }
}

struct AgentChoice {
    /// `None` is Lathe's built-in agent.
    id: Option<String>,
    name: SharedString,
    icon: Option<SharedString>,
}

/// Lathe's built-in agent first, then the project's external (ACP) agents by
/// name, named and iconed as the agent panel's new-thread menu does. A
/// collaborator's project only runs the built-in agent.
fn available_agents(project: &Entity<Project>, cx: &App) -> Vec<AgentChoice> {
    let mut agents = vec![AgentChoice {
        id: None,
        name: "Lathe Agent".into(),
        icon: None,
    }];
    let project = project.read(cx);
    if project.is_via_collab() {
        return agents;
    }
    let store = project.agent_server_store().read(cx);
    let registry = AgentRegistryStore::try_global(cx);
    let registry = registry.as_ref().map(|registry| registry.read(cx));
    let mut external = store
        .external_agents()
        .map(|agent_id| {
            let registered = registry.and_then(|registry| registry.agent(agent_id));
            AgentChoice {
                id: Some(agent_id.0.to_string()),
                name: store
                    .agent_display_name(agent_id)
                    .or_else(|| registered.map(|agent| agent.name().clone()))
                    .unwrap_or_else(|| agent_id.0.clone()),
                icon: store
                    .agent_icon(agent_id)
                    .or_else(|| registered.and_then(|agent| agent.icon_path().cloned())),
            }
        })
        .collect::<Vec<_>>();
    external.sort_by_key(|agent| agent.name.to_lowercase());
    agents.extend(external);
    agents
}

struct ReviewWithAgentDelegate {
    modal: WeakEntity<ReviewWithAgentModal>,
    agents: Vec<AgentChoice>,
    /// Indices into `agents` surviving the current filter.
    filtered: Vec<usize>,
    selected_index: usize,
    pull_request: PullRequestToReview,
}

impl PickerDelegate for ReviewWithAgentDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "review with agent"
    }

    fn match_count(&self) -> usize {
        self.filtered.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix.min(self.filtered.len().saturating_sub(1));
        cx.notify();
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        format!("Review #{} with an agent…", self.pull_request.number).into()
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_lowercase();
        self.filtered = self
            .agents
            .iter()
            .enumerate()
            .filter(|(_, agent)| query.is_empty() || agent.name.to_lowercase().contains(&query))
            .map(|(ix, _)| ix)
            .collect();
        self.selected_index = self
            .selected_index
            .min(self.filtered.len().saturating_sub(1));
        cx.notify();
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(agent) = self
            .filtered
            .get(self.selected_index)
            .and_then(|&ix| self.agents.get(ix))
        else {
            return;
        };
        let action = ReviewPullRequestWithAgent {
            agent: agent.id.clone(),
            title: format!("Review #{}", self.pull_request.number).into(),
            prompt: self.pull_request.prompt(),
        };
        // Dispatched before the modal closes, while focus is still inside the
        // workspace that registers the handler.
        window.dispatch_action(Box::new(action), cx);
        self.modal.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.modal.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let agent = self.agents.get(*self.filtered.get(ix)?)?;
        let icon = match &agent.icon {
            Some(path) => Icon::from_external_svg(path.clone()),
            None if agent.id.is_none() => Icon::new(IconName::ZedAgent),
            None => Icon::new(IconName::Sparkle),
        };
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(icon.color(Color::Muted).size(IconSize::Small))
                .child(Label::new(agent.name.clone())),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_names_the_change_and_shortens_the_head() {
        let pull_request = PullRequestToReview {
            number: 42,
            title: "Fix the thing".into(),
            url: "https://github.com/owner/lathe/pull/42".into(),
            source_branch: "feature".into(),
            target_branch: "main".into(),
            head_sha: Some("0123456789abcdef0123".into()),
            repository_name: "lathe".into(),
            host_name: "GitHub".into(),
        };
        let prompt = pull_request.prompt();
        assert!(prompt.starts_with(
            "Review pull request #42 \"Fix the thing\" in the lathe repository (https://github.com/owner/lathe/pull/42).\nIt merges feature into main; the head commit is 0123456789ab.\n"
        ));
        assert!(prompt.contains("`git diff main...feature`"));
        assert!(prompt.ends_with("Don't post anything to GitHub unless I ask."));
    }
}
