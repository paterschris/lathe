//! Lathe-owned extensions to `Workspace`.
//!
//! Child module of [`super`] (`workspace`), so it reaches `Workspace`'s private
//! fields and methods and Lathe feature code can live outside the upstream-owned
//! `workspace.rs`. The methods below are inherent `impl super::Workspace`
//! methods, so upstream and cross-crate callers invoke them unchanged.

use super::*;

impl super::Workspace {
    fn collect_portable_workspace_folders(&self, cx: &App) -> Result<Vec<PathBuf>> {
        let mut local_folder_paths: Vec<PathBuf> = Vec::new();
        let mut remote_folder_count: usize = 0;
        for worktree in self.visible_worktrees(cx) {
            let worktree = worktree.read(cx);
            if let Some(local) = worktree.as_local() {
                local_folder_paths.push(local.abs_path().to_path_buf());
            } else {
                remote_folder_count += 1;
            }
        }

        if remote_folder_count > 0 {
            anyhow::bail!(
                "Cannot save portable workspace: {remote_folder_count} remote folder(s) are open. \
                 Portable workspace files only support local folders for now. \
                 Close the remote folders, or save them as a workspace from their own session."
            );
        }
        if local_folder_paths.is_empty() {
            anyhow::bail!("No project folders to save.");
        }
        Ok(local_folder_paths)
    }

    pub fn save_workspace_as(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        use crate::portable_workspace::{
            PORTABLE_WORKSPACE_EXTENSION, PortableWorkspace, ensure_portable_workspace_extension,
        };

        let local_folder_paths = match self.collect_portable_workspace_folders(cx) {
            Ok(paths) => paths,
            Err(err) => return Task::ready(Err(err)),
        };

        let suggested_name = local_folder_paths
            .first()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(|n| format!("{n}.{PORTABLE_WORKSPACE_EXTENSION}"));

        let fs = self.app_state.fs.clone();
        let lister = DirectoryLister::Local(self.project.clone(), fs.clone());
        let prompt = self.prompt_for_new_path(lister, suggested_name, window, cx);

        cx.spawn_in(window, async move |this, cx| {
            let Some(chosen) = prompt.await.ok().flatten() else {
                return Ok(());
            };
            let Some(mut path) = chosen.into_iter().next() else {
                return Ok(());
            };
            ensure_portable_workspace_extension(&mut path);
            PortableWorkspace::save(fs, &path, &local_folder_paths).await?;
            this.update(cx, move |workspace, _cx| {
                workspace.portable_workspace_path = Some(path);
            })?;
            Ok(())
        })
    }

    pub fn save_workspace(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        use crate::portable_workspace::PortableWorkspace;

        let Some(path) = self.portable_workspace_path.clone() else {
            return self.save_workspace_as(window, cx);
        };

        let local_folder_paths = match self.collect_portable_workspace_folders(cx) {
            Ok(paths) => paths,
            Err(err) => return Task::ready(Err(err)),
        };

        let fs = self.app_state.fs.clone();
        cx.background_spawn(
            async move { PortableWorkspace::save(fs, &path, &local_folder_paths).await },
        )
    }

    pub fn portable_workspace_path(&self) -> Option<&Path> {
        self.portable_workspace_path.as_deref()
    }

    pub fn set_portable_workspace_path(&mut self, path: PathBuf) {
        self.portable_workspace_path = Some(path);
    }

    pub fn bound_collab_account_id(&self) -> Option<&str> {
        self.bound_collab_account_id.as_deref()
    }

    pub fn set_bound_collab_account_id(
        &mut self,
        account_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.bound_collab_account_id == account_id {
            return;
        }
        self.bound_collab_account_id = account_id.clone();
        cx.notify();
        let Some(database_id) = self.database_id else {
            return;
        };
        let db = WorkspaceDb::global(cx);
        cx.background_spawn(async move { db.set_collab_account_id(database_id, account_id).await })
            .detach_and_log_err(cx);
    }

    pub(super) fn apply_bound_collab_account(
        &self,
        database_id: WorkspaceId,
        cx: &mut Context<Self>,
    ) {
        let db = WorkspaceDb::global(cx);
        let client = self.client().clone();
        cx.spawn(async move |this, cx| {
            let bound_id = db.collab_account_id(database_id).await.ok().flatten();
            this.update(cx, |this, cx| {
                if this.bound_collab_account_id != bound_id {
                    this.bound_collab_account_id = bound_id.clone();
                    cx.notify();
                }
            })
            .ok();
            let Some(bound_id) = bound_id else {
                return;
            };
            if client.active_account_id().as_deref() == Some(bound_id.as_str()) {
                return;
            }
            if !client
                .list_accounts()
                .iter()
                .any(|account| account.id == bound_id)
            {
                return;
            }
            client.switch_account(bound_id, cx).await.log_err();
        })
        .detach();
    }

    pub fn any_item_awaiting_input(&self, cx: &App) -> bool {
        let dock_panes = self
            .all_docks()
            .into_iter()
            .flat_map(|dock| dock.read(cx).panel_panes(cx));
        for pane in self.panes.iter().cloned().chain(dock_panes) {
            for item in pane.read(cx).items() {
                if item.is_awaiting_input(cx) {
                    return true;
                }
            }
        }
        // Also surface panel-level awaiting-input states (e.g. the agent
        // panel when the active ACP thread has finished generating and is
        // ready for the next user message). Panels don't expose their
        // contents through the pane/item walker above.
        for dock in self.all_docks() {
            if dock
                .read(cx)
                .iter_panels()
                .any(|panel| panel.is_awaiting_input(cx))
            {
                return true;
            }
        }
        false
    }

    pub fn awaiting_input_count(&self, cx: &App) -> usize {
        let dock_panes = self
            .all_docks()
            .into_iter()
            .flat_map(|dock| dock.read(cx).panel_panes(cx));
        let mut count = 0;
        for pane in self.panes.iter().cloned().chain(dock_panes) {
            for item in pane.read(cx).items() {
                if item.is_awaiting_input(cx) {
                    count += 1;
                }
            }
        }
        for dock in self.all_docks() {
            count += dock
                .read(cx)
                .iter_panels()
                .filter(|panel| panel.is_awaiting_input(cx))
                .count();
        }
        count
    }

    pub fn first_awaiting_input_tooltip(&self, cx: &App) -> &'static str {
        let dock_panes = self
            .all_docks()
            .into_iter()
            .flat_map(|dock| dock.read(cx).panel_panes(cx));
        for pane in self.panes.iter().cloned().chain(dock_panes) {
            for item in pane.read(cx).items() {
                if item.is_awaiting_input(cx) {
                    return item.awaiting_input_tooltip(cx);
                }
            }
        }
        for dock in self.all_docks() {
            if let Some(tooltip) = dock
                .read(cx)
                .iter_panels()
                .find(|panel| panel.is_awaiting_input(cx))
                .map(|panel| panel.awaiting_input_tooltip(cx))
            {
                return tooltip;
            }
        }
        "Terminal awaiting input"
    }

    pub fn focus_first_awaiting_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let dock_panes: Vec<_> = self
            .all_docks()
            .into_iter()
            .flat_map(|dock| dock.read(cx).panel_panes(cx))
            .collect();
        for pane in self.panes.iter().cloned().chain(dock_panes) {
            let awaiting_index = pane
                .read(cx)
                .items()
                .position(|item| item.is_awaiting_input(cx));
            if let Some(index) = awaiting_index {
                pane.update(cx, |pane, cx| {
                    pane.activate_item(index, true, true, window, cx);
                });
                return true;
            }
        }
        // Panel-level fallback: open the first dock panel whose own state
        // says it's awaiting input.
        for dock in self.all_docks() {
            let panel_index = dock
                .read(cx)
                .iter_panels()
                .position(|panel| panel.is_awaiting_input(cx));
            if let Some(index) = panel_index {
                dock.update(cx, |dock, cx| {
                    dock.activate_panel(index, window, cx);
                });
                return true;
            }
        }
        false
    }
}
