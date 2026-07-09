//! Lathe-owned extensions to `Workspace`.
//!
//! Child module of [`super`] (`workspace`), so it reaches `Workspace`'s private
//! fields and methods and Lathe feature code can live outside the upstream-owned
//! `workspace.rs`. The methods below are inherent `impl super::Workspace`
//! methods, so upstream and cross-crate callers invoke them unchanged.

use super::*;

impl super::Workspace {
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
