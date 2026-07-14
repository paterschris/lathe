use gpui::{
    App, Context, Empty, Entity, Focusable, IntoElement, Render, Subscription, WeakEntity, Window,
};
use project::git_store::{GitStore, GitStoreEvent, Repository};
use workspace::{HideStatusItem, StatusItemView, Workspace, item::ItemHandle};

use ui::{ContextMenu, PopoverMenuHandle};

use crate::render_remote_button;

pub struct BranchStatusIndicator {
    workspace: WeakEntity<Workspace>,
    active_repository: Option<Entity<Repository>>,
    _git_store_subscription: Subscription,
}

impl BranchStatusIndicator {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().read(cx);
        let git_store = project.git_store().clone();
        let active_repository = git_store.read(cx).active_repository();

        let subscription = cx.subscribe(&git_store, Self::on_git_store_event);

        Self {
            workspace: workspace.weak_handle(),
            active_repository,
            _git_store_subscription: subscription,
        }
    }

    fn on_git_store_event(
        &mut self,
        git_store: Entity<GitStore>,
        event: &GitStoreEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            GitStoreEvent::ActiveRepositoryChanged(_) => {
                self.active_repository = git_store.read(cx).active_repository();
                cx.notify();
            }
            GitStoreEvent::RepositoryUpdated(_, _, is_active) if *is_active => {
                cx.notify();
            }
            GitStoreEvent::RepositoryRemoved(_) | GitStoreEvent::RepositoryAdded => {
                self.active_repository = git_store.read(cx).active_repository();
                cx.notify();
            }
            _ => {}
        }
    }
}

impl Render for BranchStatusIndicator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(branch) = self
            .active_repository
            .as_ref()
            .and_then(|repo| repo.read(cx).branch.clone())
        else {
            return Empty.into_any_element();
        };

        let focus_handle = self
            .workspace
            .upgrade()
            .map(|workspace| workspace.focus_handle(cx));

        match render_remote_button(
            "branch-status-indicator",
            &branch,
            focus_handle,
            true,
            None,
            PopoverMenuHandle::<ContextMenu>::default(),
        ) {
            Some(button) => button.into_any_element(),
            None => Empty.into_any_element(),
        }
    }
}

impl StatusItemView for BranchStatusIndicator {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}
