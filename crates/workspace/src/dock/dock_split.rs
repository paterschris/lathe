use std::{sync::Arc, time::Duration};

use db::kvp::KeyValueStore;
use gpui::{
    AnyView, App, AppContext as _, Axis, Context, DragMoveEvent, Empty, MouseButton,
    MouseDownEvent, MouseUpEvent, Pixels, Render, StyleRefinement, Window, div, px, relative,
};
use serde::{Deserialize, Serialize};
use ui::prelude::*;
use util::ResultExt as _;

use super::{Dock, DockPosition, PanelHandle};
use crate::Workspace;

const DOCK_SPLIT_KEY: &str = "dock_split";
const DEFAULT_FRACTION: f32 = 0.5;
/// Keeps either half from being dragged so small it can't be grabbed again.
const MIN_FRACTION: f32 = 0.15;
const PERSIST_DEBOUNCE: Duration = Duration::from_millis(300);

/// A second panel shown alongside the dock's active panel: below it in the
/// left and right docks, beside it in the bottom dock. The active panel keeps
/// every role it had before (dock size, serialized active panel, the target
/// of dock-wide actions), so a dock without a split behaves exactly as it
/// always has.
#[derive(Clone, Copy, Debug)]
pub(super) struct DockSplit {
    pub(super) panel_index: usize,
    /// The active panel's share of the dock's length.
    pub(super) fraction: f32,
}

/// Panels are stored by persistent name: indices shift as panels register.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct PersistedDockSplit {
    primary: String,
    secondary: String,
    fraction: f32,
}

#[derive(Clone, Copy)]
pub(crate) struct DraggedDockSplit(DockPosition);

/// Which half of a dock a panel occupies. `Primary` is the top half of a side
/// dock and the left half of the bottom dock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DockSlot {
    Primary,
    Secondary,
}

impl DockSlot {
    /// Menu label for this slot in a dock at `position`, e.g. "Top Left".
    pub fn label(self, position: DockPosition) -> &'static str {
        match (position, self) {
            (DockPosition::Left, DockSlot::Primary) => "Top Left",
            (DockPosition::Left, DockSlot::Secondary) => "Bottom Left",
            (DockPosition::Right, DockSlot::Primary) => "Top Right",
            (DockPosition::Right, DockSlot::Secondary) => "Bottom Right",
            (DockPosition::Bottom, DockSlot::Primary) => "Bottom, Left Half",
            (DockPosition::Bottom, DockSlot::Secondary) => "Bottom, Right Half",
        }
    }
}

fn storage_key(workspace_id: i64, position: DockPosition) -> String {
    format!("{workspace_id}:{}", position.label())
}

impl Dock {
    pub fn split_panel(&self) -> Option<&Arc<dyn PanelHandle>> {
        let split = self.split?;
        Some(&self.panel_entries.get(split.panel_index)?.panel)
    }

    pub fn split_panel_index(&self) -> Option<usize> {
        self.split.map(|split| split.panel_index)
    }

    /// The half `panel_id` is showing in, if it is on screen in this dock.
    pub fn slot_of(&self, panel_id: gpui::EntityId) -> Option<DockSlot> {
        if !self.is_open {
            None
        } else if self
            .active_panel()
            .is_some_and(|panel| panel.panel_id() == panel_id)
        {
            Some(DockSlot::Primary)
        } else if self
            .split_panel()
            .is_some_and(|panel| panel.panel_id() == panel_id)
        {
            Some(DockSlot::Secondary)
        } else {
            None
        }
    }

    /// Opens the dock with the panel at `panel_index` in `slot`. The other half
    /// keeps what it was showing; a panel already on screen in the other half
    /// swaps places with that half's panel.
    pub fn place_panel(
        &mut self,
        panel_index: usize,
        slot: DockSlot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if panel_index >= self.panel_entries.len() {
            return;
        }
        if let Some(panel) = self.panel_at(panel_index) {
            let panel_id = panel.panel_id();
            self.forget_closed_from_stack(panel_id);
        }
        let was_open = self.is_open;
        self.set_open(true, window, cx);
        let is_active = self.active_panel_index == Some(panel_index) && was_open;
        let is_split = self.split_panel_index() == Some(panel_index);
        match slot {
            DockSlot::Primary if is_split => self.swap_split_halves(cx),
            DockSlot::Primary => self.activate_panel(panel_index, window, cx),
            DockSlot::Secondary if is_active => self.swap_split_halves(cx),
            DockSlot::Secondary => self.open_split(panel_index, window, cx),
        }
    }

    /// Exchanges the two halves. Both panels stay on screen, so neither one's
    /// active state changes.
    fn swap_split_halves(&mut self, cx: &mut Context<Self>) {
        let (Some(split), Some(active_index)) = (self.split.as_mut(), self.active_panel_index)
        else {
            return;
        };
        self.active_panel_index = Some(split.panel_index);
        split.panel_index = active_index;
        split.fraction = 1. - split.fraction;
        self.pending_split = None;
        self.persist_split(cx);
        cx.notify();
    }

    /// Records where a panel moving in from another dock should land. Applied
    /// by `apply_pending_placement` once the move lands, since a panel's dock
    /// is changed through settings and the move happens after the fact.
    pub fn set_pending_placement(&mut self, panel_id: gpui::EntityId, slot: DockSlot) {
        self.pending_placement = Some((panel_id, slot));
    }

    pub(super) fn has_pending_placement(&self, panel_id: gpui::EntityId) -> bool {
        self.pending_placement
            .is_some_and(|(pending_id, _)| pending_id == panel_id)
    }

    pub(super) fn apply_pending_placement(
        &mut self,
        panel_id: gpui::EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((pending_id, slot)) = self.pending_placement else {
            return;
        };
        if pending_id != panel_id {
            return;
        }
        self.pending_placement = None;
        if let Some(panel_index) = self
            .panel_entries
            .iter()
            .position(|entry| entry.panel.panel_id() == panel_id)
        {
            self.place_panel(panel_index, slot, window, cx);
        }
    }

    pub fn panel_at(&self, index: usize) -> Option<&Arc<dyn PanelHandle>> {
        self.panel_entries.get(index).map(|entry| &entry.panel)
    }

    /// Whether `panel_id` is on screen in this dock, in either half.
    pub fn is_panel_visible(&self, panel_id: gpui::EntityId) -> bool {
        self.is_open
            && (self
                .active_panel()
                .is_some_and(|panel| panel.panel_id() == panel_id)
                || self
                    .split_panel()
                    .is_some_and(|panel| panel.panel_id() == panel_id))
    }

    /// Shows `panel_index` alongside the active panel. Opening the active panel
    /// itself, or a dock with nothing in it yet, falls back to plain activation.
    pub fn open_split(&mut self, panel_index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if panel_index >= self.panel_entries.len() {
            return;
        }
        let Some(active_index) = self.active_panel_index.filter(|_| self.is_open) else {
            self.activate_panel(panel_index, window, cx);
            self.set_open(true, window, cx);
            return;
        };
        if active_index == panel_index || self.split_panel_index() == Some(panel_index) {
            return;
        }
        if let Some(previous) = self.split_panel() {
            previous.set_active(false, window, cx);
        }
        let fraction = self.split.map_or(DEFAULT_FRACTION, |split| split.fraction);
        self.split = Some(DockSplit {
            panel_index,
            fraction,
        });
        self.pending_split = None;
        if let Some(panel) = self.split_panel() {
            panel.set_active(true, window, cx);
        }
        self.persist_split(cx);
        cx.notify();
    }

    pub fn close_split(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.split_panel().cloned() else {
            return;
        };
        panel.set_active(false, window, cx);
        self.split = None;
        self.pending_split = None;
        self.persist_split(cx);
        cx.notify();
    }

    /// The active panel is going away while a split is open, so the split
    /// panel takes over the whole dock instead of the dock closing.
    pub(super) fn promote_split(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(split) = self.split.take() else {
            return false;
        };
        if let Some(previous) = self.active_panel() {
            previous.set_active(false, window, cx);
        }
        self.active_panel_index = Some(split.panel_index);
        self.persist_split(cx);
        cx.notify();
        true
    }

    /// Takes the panel at `panel_index` out of the two-panel stack, leaving the
    /// other panel the whole dock, and remembers which half it was in and how
    /// the dock was divided so `reopen_in_stack` can put it back. Returns false
    /// when the panel isn't half of a split; the caller closes the dock then.
    pub fn close_in_stack(
        &mut self,
        panel_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(split) = self.split.filter(|_| self.is_open) else {
            return false;
        };
        let slot = if split.panel_index == panel_index {
            DockSlot::Secondary
        } else if self.active_panel_index == Some(panel_index) {
            DockSlot::Primary
        } else {
            return false;
        };
        let Some(panel_id) = self.panel_at(panel_index).map(|panel| panel.panel_id()) else {
            return false;
        };
        self.closed_from_stack
            .insert(panel_id, (slot, split.fraction));
        match slot {
            DockSlot::Secondary => self.close_split(window, cx),
            DockSlot::Primary => {
                self.promote_split(window, cx);
            }
        }
        true
    }

    /// Puts a panel closed by `close_in_stack` back in the half it came from,
    /// with the dock divided as it was. Only applies while the dock is open on
    /// a single other panel; anything else is an ordinary activation.
    pub fn reopen_in_stack(
        &mut self,
        panel_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(panel_id) = self.panel_at(panel_index).map(|panel| panel.panel_id()) else {
            return false;
        };
        let Some(&(slot, fraction)) = self.closed_from_stack.get(&panel_id) else {
            return false;
        };
        let Some(active_index) = self.active_panel_index else {
            return false;
        };
        if !self.is_open || self.split.is_some() || active_index == panel_index {
            return false;
        }
        self.closed_from_stack.remove(&panel_id);
        let fraction = fraction.clamp(MIN_FRACTION, 1. - MIN_FRACTION);
        let split_index = match slot {
            DockSlot::Secondary => panel_index,
            DockSlot::Primary => {
                self.active_panel_index = Some(panel_index);
                active_index
            }
        };
        self.split = Some(DockSplit {
            panel_index: split_index,
            fraction,
        });
        self.pending_split = None;
        if let Some(panel) = self.panel_at(panel_index) {
            panel.set_active(true, window, cx);
        }
        self.persist_split(cx);
        cx.notify();
        true
    }

    pub fn split_fraction(&self) -> Option<f32> {
        self.split.map(|split| split.fraction)
    }

    pub(super) fn forget_closed_from_stack(&mut self, panel_id: gpui::EntityId) {
        self.closed_from_stack.remove(&panel_id);
    }

    /// Keeps the split pointing at the same panel after the entry at
    /// `removed_index` is taken out of `panel_entries`.
    pub(super) fn split_panel_removed(&mut self, removed_index: usize, cx: &mut Context<Self>) {
        let Some(split) = self.split.as_mut() else {
            return;
        };
        if split.panel_index == removed_index {
            self.split = None;
            self.persist_split(cx);
        } else if split.panel_index > removed_index {
            split.panel_index -= 1;
        }
    }

    pub(super) fn split_panel_inserted(&mut self, inserted_index: usize) {
        if let Some(split) = self.split.as_mut()
            && split.panel_index >= inserted_index
        {
            split.panel_index += 1;
        }
    }

    pub(crate) fn set_split_fraction(&mut self, fraction: f32, cx: &mut Context<Self>) {
        let Some(split) = self.split.as_mut() else {
            return;
        };
        let fraction = fraction.clamp(MIN_FRACTION, 1. - MIN_FRACTION);
        if split.fraction != fraction {
            split.fraction = fraction;
            self.persist_split(cx);
            cx.notify();
        }
    }

    fn persisted_split(&self) -> Option<PersistedDockSplit> {
        let split = self.split?;
        Some(PersistedDockSplit {
            primary: self.active_panel()?.persistent_name().to_string(),
            secondary: self
                .panel_entries
                .get(split.panel_index)?
                .panel
                .persistent_name()
                .to_string(),
            fraction: split.fraction,
        })
    }

    /// Debounced, since dragging the divider changes the fraction every frame.
    /// Uses the id cached by `load_persisted_split` rather than reading the
    /// workspace, which is often mid-update when the split changes.
    fn persist_split(&mut self, cx: &mut Context<Self>) {
        let Some(workspace_id) = self.split_workspace_id else {
            return;
        };
        let key = storage_key(i64::from(workspace_id), self.position);
        let split = self.persisted_split();
        let kvp = KeyValueStore::global(cx);
        self._persist_split_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(PERSIST_DEBOUNCE).await;
            let scope = kvp.scoped(DOCK_SPLIT_KEY);
            let result = match split {
                Some(split) => match serde_json::to_string(&split) {
                    Ok(json) => scope.write(key, json).await,
                    Err(error) => Err(error.into()),
                },
                None => scope.delete(key).await,
            };
            result.log_err();
        }));
    }

    /// Reads this workspace's saved split once. It is applied as soon as both
    /// of its panels have registered and the saved primary is the active panel.
    pub(crate) fn load_persisted_split(&mut self, workspace: &Workspace, cx: &mut Context<Self>) {
        let Some(workspace_id) = workspace.database_id() else {
            return;
        };
        if self.split_workspace_id == Some(workspace_id) {
            return;
        }
        self.split_workspace_id = Some(workspace_id);
        self.pending_split = KeyValueStore::global(cx)
            .scoped(DOCK_SPLIT_KEY)
            .read(&storage_key(i64::from(workspace_id), self.position))
            .log_err()
            .flatten()
            .and_then(|json| serde_json::from_str::<PersistedDockSplit>(&json).log_err());
    }

    pub(super) fn apply_pending_split(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_split.as_ref() else {
            return;
        };
        if self.split.is_some()
            || self
                .active_panel()
                .is_none_or(|panel| panel.persistent_name() != pending.primary)
        {
            return;
        }
        let Some(panel_index) = self
            .panel_entries
            .iter()
            .position(|entry| entry.panel.persistent_name() == pending.secondary)
        else {
            return;
        };
        if Some(panel_index) == self.active_panel_index {
            self.pending_split = None;
            return;
        }
        self.split = Some(DockSplit {
            panel_index,
            fraction: pending.fraction.clamp(MIN_FRACTION, 1. - MIN_FRACTION),
        });
        self.pending_split = None;
        if self.is_open
            && let Some(panel) = self.split_panel()
        {
            panel.set_active(true, window, cx);
        }
        cx.notify();
    }

    /// The dock's content: the active panel alone, or the active panel and the
    /// split panel separated by a draggable divider.
    pub(super) fn render_panels(&self, active: AnyView, cx: &mut Context<Self>) -> AnyElement {
        let panel_view =
            |view: AnyView| view.cached(StyleRefinement::default().v_flex().size_full());
        let Some((split, split_view)) = self
            .split
            .and_then(|split| Some((split, self.split_panel()?.to_any())))
        else {
            return panel_view(active).into_any_element();
        };

        let position = self.position;
        // Side docks are tall and narrow, so their panels stack; the bottom
        // dock is wide and short, so its panels sit side by side.
        let stacked = position.axis() == Axis::Horizontal;
        let line = cx.theme().colors().border;
        let line_hover = cx.theme().colors().border_focused;

        let handle = div()
            .id("dock-split-handle")
            .group("dock-split-handle")
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .map(|this| {
                if stacked {
                    this.h(px(5.)).w_full().cursor_row_resize()
                } else {
                    this.w(px(5.)).h_full().cursor_col_resize()
                }
            })
            .child(
                div()
                    .bg(line)
                    .group_hover("dock-split-handle", move |style| style.bg(line_hover))
                    .map(|this| {
                        if stacked {
                            this.h_px().w_full()
                        } else {
                            this.w_px().h_full()
                        }
                    }),
            )
            .on_drag(DraggedDockSplit(position), |_, _, _, cx| cx.new(|_| Empty))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _: &MouseDownEvent, _, cx| cx.stop_propagation()),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|dock, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        dock.set_split_fraction(DEFAULT_FRACTION, cx);
                        cx.stop_propagation();
                    }
                }),
            );

        div()
            .id("dock-split")
            .flex()
            .size_full()
            .map(|this| {
                if stacked {
                    this.flex_col()
                } else {
                    this.flex_row()
                }
            })
            .on_drag_move(cx.listener(
                move |dock, event: &DragMoveEvent<DraggedDockSplit>, _, cx| {
                    if event.drag(cx).0 != position {
                        return;
                    }
                    let (offset, length): (Pixels, Pixels) = if stacked {
                        (
                            event.event.position.y - event.bounds.top(),
                            event.bounds.size.height,
                        )
                    } else {
                        (
                            event.event.position.x - event.bounds.left(),
                            event.bounds.size.width,
                        )
                    };
                    if length > px(0.) {
                        dock.set_split_fraction(offset / length, cx);
                    }
                },
            ))
            .child(
                div()
                    .flex_none()
                    .overflow_hidden()
                    .map(|this| {
                        if stacked {
                            this.w_full().h(relative(split.fraction))
                        } else {
                            this.h_full().w(relative(split.fraction))
                        }
                    })
                    .child(panel_view(active)),
            )
            .child(handle)
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .map(|this| {
                        if stacked {
                            this.w_full().min_h_0()
                        } else {
                            this.h_full().min_w_0()
                        }
                    })
                    .child(panel_view(split_view)),
            )
            .into_any_element()
    }
}

/// A panel being dragged from its status-bar button to a spot in a dock.
#[derive(Clone)]
pub(crate) struct DraggedPanel {
    pub(crate) panel_id: gpui::EntityId,
    pub(crate) icon: IconName,
    pub(crate) label: SharedString,
}

impl Render for DraggedPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_1()
            .px_2()
            .py_1()
            .rounded_md()
            .elevation_2(cx)
            .child(Icon::new(self.icon).size(IconSize::Small))
            .child(Label::new(self.label.clone()).size(LabelSize::Small))
    }
}

/// Which panel is being dragged, so the workspace knows to show drop zones.
/// GPUI does not expose the type of the active drag, so this is set when a
/// panel drag starts and cleared once no drag is active.
#[derive(Default)]
pub(crate) struct ActivePanelDrag(Option<gpui::EntityId>);

impl gpui::Global for ActivePanelDrag {}

impl ActivePanelDrag {
    pub(crate) fn start(panel_id: gpui::EntityId, cx: &mut App) {
        cx.set_global(Self(Some(panel_id)));
    }
}

impl Workspace {
    /// Moves a panel to one half of a dock, from wherever it is now. A panel
    /// already in the target dock is rearranged in place; one in another dock
    /// changes docks first and lands in `slot` once the move completes.
    pub fn move_panel_to(
        &mut self,
        panel_id: gpui::EntityId,
        position: DockPosition,
        slot: DockSlot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((source_position, panel_index, panel)) =
            self.all_docks().into_iter().find_map(|dock| {
                let dock = dock.read(cx);
                let index = dock
                    .panel_entries
                    .iter()
                    .position(|entry| entry.panel.panel_id() == panel_id)?;
                Some((
                    dock.position,
                    index,
                    dock.panel_entries[index].panel.clone(),
                ))
            })
        else {
            return;
        };
        if !panel.position_is_valid(position, cx) {
            return;
        }
        let target = self.dock_at_position(position).clone();
        if source_position == position {
            target.update(cx, |dock, cx| {
                dock.place_panel(panel_index, slot, window, cx)
            });
        } else {
            target.update(cx, |dock, _| dock.set_pending_placement(panel_id, slot));
            // Deferred out of this workspace update: panels that keep a
            // per-workspace placement (agent, project, outline) record it by
            // updating the workspace, which can't happen while it is leased.
            window.defer(cx, move |window, cx| {
                panel.set_position(position, window, cx)
            });
        }
        self.serialize_workspace(window, cx);
    }

    /// Drop targets for a panel being dragged from the status bar: the two
    /// halves of each dock it can live in, laid out where those halves sit.
    pub(crate) fn render_panel_drop_zones(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dragged_panel = cx.try_global::<ActivePanelDrag>().and_then(|drag| drag.0)?;
        if !cx.has_active_drag() {
            // The drag ended without a drop. Cleared on the next frame rather
            // than here, since globals can't change mid-render.
            cx.defer(|cx| cx.set_global(ActivePanelDrag(None)));
            return None;
        }
        let panel = self
            .all_docks()
            .into_iter()
            .find_map(|dock| dock.read(cx).panel_for_id(dragged_panel).cloned())?;
        let colors = cx.theme().colors();
        let zone_bg = colors.panel_background.opacity(0.85);
        let zone_border = colors.border_focused;
        let hover_bg = colors.drop_target_background;

        let zone = |position: DockPosition, slot: DockSlot, cx: &mut Context<Self>| {
            let valid = panel.position_is_valid(position, cx);
            div()
                .id(SharedString::from(format!(
                    "panel-drop-{}-{slot:?}",
                    position.label()
                )))
                .flex_1()
                .m_1()
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .border_2()
                .border_color(zone_border)
                .bg(zone_bg)
                .when(!valid, |this| this.opacity(0.35))
                .child(
                    Label::new(slot.label(position))
                        .size(LabelSize::Small)
                        .color(if valid {
                            Color::Default
                        } else {
                            Color::Disabled
                        }),
                )
                .when(valid, |this| {
                    this.drag_over::<DraggedPanel>(move |style, _, _, _| style.bg(hover_bg))
                        .on_drop(cx.listener(
                            move |workspace, dragged: &DraggedPanel, window, cx| {
                                cx.set_global(ActivePanelDrag(None));
                                workspace.move_panel_to(
                                    dragged.panel_id,
                                    position,
                                    slot,
                                    window,
                                    cx,
                                );
                            },
                        ))
                })
        };

        let side = |position: DockPosition, cx: &mut Context<Self>| {
            v_flex()
                .w(px(220.))
                .h_full()
                .child(zone(position, DockSlot::Primary, cx))
                .child(zone(position, DockSlot::Secondary, cx))
        };
        let left = side(DockPosition::Left, cx);
        let right = side(DockPosition::Right, cx);
        let bottom = h_flex()
            .h(px(140.))
            .w_full()
            .child(zone(DockPosition::Bottom, DockSlot::Primary, cx))
            .child(zone(DockPosition::Bottom, DockSlot::Secondary, cx));

        Some(
            h_flex()
                .absolute()
                .inset_0()
                .p_1()
                .child(left)
                .child(
                    v_flex()
                        .flex_1()
                        .h_full()
                        .child(div().flex_1())
                        .child(bottom),
                )
                .child(right)
                .into_any_element(),
        )
    }
}
