//! Dock panels from Lathe extensions.
//!
//! Each dock side gets one container panel. Extension panels registered for
//! that side are tabs inside it, and the container's button only shows while
//! it has any. One container per side, rather than one dock panel per
//! extension panel, because the workspace keys dock state and sizes by a
//! panel type's static name, and every panel needs its own activation
//! priority.

use std::marker::PhantomData;
use std::str::FromStr as _;
use std::sync::Arc;

use extension_host::ExtensionStore;
use extension_host::wasm_host::lathe::{LatheDockSide, LathePanel};
use gpui::{
    Action, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Global, IntoElement,
    Pixels, Render, SharedString, Subscription, Window, actions, px,
};
use schemars::JsonSchema;
use serde::Deserialize;
use ui::{Button, IconName, Label, LabelSize, prelude::*, v_flex};
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

use crate::lathe_preview::ObserveAvailability;
use crate::lathe_ui::GlobalExtensionViews;
use crate::lathe_view_renderer::ExtensionViewHost;

/// Opens an extension's panel, or closes it if it's showing.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema, gpui::Action)]
#[action(namespace = extension)]
#[serde(deny_unknown_fields)]
pub struct TogglePanel {
    /// The id of the extension that registered the panel.
    pub extension_id: String,
    /// The panel's id within that extension.
    pub panel: String,
}

actions!(
    extension,
    [
        /// Toggles the panel of extension panels on the left dock.
        ToggleLeftPanels,
        /// Toggles the panel of extension panels on the right dock.
        ToggleRightPanels,
        /// Toggles the panel of extension panels on the bottom dock.
        ToggleBottomPanels
    ]
);

/// A panel an extension registered, with the extension's name for titles.
#[derive(Clone, Debug, PartialEq)]
pub struct AvailablePanel {
    pub extension_id: Arc<str>,
    pub extension_name: SharedString,
    pub panel: LathePanel,
}

impl AvailablePanel {
    fn is(&self, extension_id: &str, panel_id: &str) -> bool {
        self.extension_id.as_ref() == extension_id && self.panel.id.as_ref() == panel_id
    }

    fn action(&self) -> TogglePanel {
        TogglePanel {
            extension_id: self.extension_id.to_string(),
            panel: self.panel.id.to_string(),
        }
    }
}

/// What panels need from the extension store. Tests swap in their own.
#[derive(Clone)]
pub struct PanelBackend {
    pub panels: Arc<dyn Fn(&App) -> Vec<AvailablePanel>>,
    pub observe: ObserveAvailability,
}

impl Global for PanelBackend {}

pub(crate) fn init(cx: &mut App) {
    cx.set_global(PanelBackend {
        panels: Arc::new(|cx| {
            let Some(store) = ExtensionStore::try_global(cx) else {
                return Vec::new();
            };
            store
                .read(cx)
                .lathe_panels()
                .map(|(manifest, panel)| AvailablePanel {
                    extension_id: manifest.id.clone(),
                    extension_name: manifest.name.clone().into(),
                    panel: panel.clone(),
                })
                .collect()
        }),
        observe: Arc::new(|callback, cx| {
            let store = ExtensionStore::try_global(cx)?;
            Some(cx.observe(&store, move |_, cx| callback(cx)))
        }),
    });
    register(cx);
}

/// Registers the palette entries, actions and the containers in every
/// workspace. Separate from [`init`] so tests can install their own backend.
pub(crate) fn register(cx: &mut App) {
    command_palette_hooks::register_extra_commands(cx, |cx| {
        let Some(backend) = cx.try_global::<PanelBackend>() else {
            return Vec::new();
        };
        (backend.panels)(cx)
            .into_iter()
            .map(|available| command_palette_hooks::ExtraCommand {
                name: format!(
                    "{}: Toggle {}",
                    available.extension_name, available.panel.title
                )
                .into(),
                action: Box::new(available.action()),
            })
            .collect()
    });
    command_palette_hooks::CommandPaletteFilter::update_global(cx, |filter, _| {
        filter.hide_action_types(&[
            std::any::TypeId::of::<TogglePanel>(),
            std::any::TypeId::of::<ToggleLeftPanels>(),
            std::any::TypeId::of::<ToggleRightPanels>(),
            std::any::TypeId::of::<ToggleBottomPanels>(),
        ]);
    });
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        add_container::<LeftSide>(workspace, window, cx);
        add_container::<RightSide>(workspace, window, cx);
        add_container::<BottomSide>(workspace, window, cx);
        workspace.register_action(toggle_panel);
        workspace.register_action(|workspace, _: &ToggleLeftPanels, window, cx| {
            workspace.toggle_panel_focus::<ExtensionPanels<LeftSide>>(window, cx);
        });
        workspace.register_action(|workspace, _: &ToggleRightPanels, window, cx| {
            workspace.toggle_panel_focus::<ExtensionPanels<RightSide>>(window, cx);
        });
        workspace.register_action(|workspace, _: &ToggleBottomPanels, window, cx| {
            workspace.toggle_panel_focus::<ExtensionPanels<BottomSide>>(window, cx);
        });
    })
    .detach();
}

fn add_container<S: Side>(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(backend) = cx.try_global::<PanelBackend>().cloned() else {
        return;
    };
    let container = cx.new(|cx| ExtensionPanels::<S>::new(backend, window, cx));
    workspace.add_panel(container, window, cx);
}

fn toggle_panel(
    workspace: &mut Workspace,
    action: &TogglePanel,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let available = cx.try_global::<PanelBackend>().and_then(|backend| {
        (backend.panels)(cx)
            .into_iter()
            .find(|available| available.is(&action.extension_id, &action.panel))
    });
    let Some(available) = available else {
        workspace.show_error(
            format!(
                "{}'s panel {:?} isn't available: the extension isn't loaded, or isn't \
                 allowed to show views",
                action.extension_id, action.panel
            ),
            cx,
        );
        return;
    };
    match available.panel.dock {
        LatheDockSide::Left => toggle_in::<LeftSide>(workspace, &available, window, cx),
        LatheDockSide::Right => toggle_in::<RightSide>(workspace, &available, window, cx),
        LatheDockSide::Bottom => toggle_in::<BottomSide>(workspace, &available, window, cx),
    }
}

/// Closes the panel if it's showing; otherwise selects its tab, opens the
/// dock and focuses it. Running it from the palette means the palette has
/// focus, so this goes by what's showing rather than by focus.
fn toggle_in<S: Side>(
    workspace: &mut Workspace,
    available: &AvailablePanel,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(container) = workspace.panel::<ExtensionPanels<S>>(cx) else {
        return;
    };
    let showing = container.read(cx).active && container.read(cx).is_selected(available);
    if showing {
        workspace.close_panel::<ExtensionPanels<S>>(window, cx);
        return;
    }
    container.update(cx, |container, cx| {
        container.select(
            available.extension_id.clone(),
            available.panel.id.clone(),
            window,
            cx,
        )
    });
    workspace.focus_panel::<ExtensionPanels<S>>(window, cx);
}

/// A dock side, as a type, so each side's container has its own static names.
pub trait Side: 'static {
    const DOCK: LatheDockSide;
    const POSITION: DockPosition;
    const NAME: &'static str;
    const PRIORITY: u32;
    fn toggle_action() -> Box<dyn Action>;
}

pub struct LeftSide;
pub struct RightSide;
pub struct BottomSide;

// Activation priorities must be unique across all panels (the dock panics in
// debug builds otherwise). Zed's and Lathe's panels use 0 to 9 and 200.
impl Side for LeftSide {
    const DOCK: LatheDockSide = LatheDockSide::Left;
    const POSITION: DockPosition = DockPosition::Left;
    const NAME: &'static str = "LatheExtensionPanelsLeft";
    const PRIORITY: u32 = 100;
    fn toggle_action() -> Box<dyn Action> {
        Box::new(ToggleLeftPanels)
    }
}

impl Side for RightSide {
    const DOCK: LatheDockSide = LatheDockSide::Right;
    const POSITION: DockPosition = DockPosition::Right;
    const NAME: &'static str = "LatheExtensionPanelsRight";
    const PRIORITY: u32 = 101;
    fn toggle_action() -> Box<dyn Action> {
        Box::new(ToggleRightPanels)
    }
}

impl Side for BottomSide {
    const DOCK: LatheDockSide = LatheDockSide::Bottom;
    const POSITION: DockPosition = DockPosition::Bottom;
    const NAME: &'static str = "LatheExtensionPanelsBottom";
    const PRIORITY: u32 = 102;
    fn toggle_action() -> Box<dyn Action> {
        Box::new(ToggleBottomPanels)
    }
}

/// The extension panels registered for one dock side, as tabs.
pub struct ExtensionPanels<S: Side> {
    backend: PanelBackend,
    panels: Vec<AvailablePanel>,
    /// (extension id, panel id) of the selected tab.
    selected: Option<(Arc<str>, Arc<str>)>,
    /// The selected panel's view, only while the dock shows this container,
    /// so extensions hear `view-hidden` when it's closed.
    host: Option<Entity<ExtensionViewHost>>,
    active: bool,
    position: DockPosition,
    focus_handle: FocusHandle,
    _availability: Option<Subscription>,
    _side: PhantomData<S>,
}

impl<S: Side> ExtensionPanels<S> {
    fn new(backend: PanelBackend, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let this = cx.weak_entity();
        let window_handle = window.window_handle();
        let availability = (backend.observe)(
            Box::new(move |cx| {
                window_handle
                    .update(cx, |_, window, cx| {
                        this.update(cx, |this, cx| this.refresh(window, cx)).ok();
                    })
                    .ok();
            }),
            cx,
        );
        let mut panels = Self {
            backend,
            panels: Vec::new(),
            selected: None,
            host: None,
            active: false,
            position: S::POSITION,
            focus_handle: cx.focus_handle(),
            _availability: availability,
            _side: PhantomData,
        };
        panels.refresh(window, cx);
        panels
    }

    /// Re-reads which panels are registered for this side.
    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let panels = (self.backend.panels)(cx)
            .into_iter()
            .filter(|available| available.panel.dock == S::DOCK)
            .collect::<Vec<_>>();
        let selection_gone = self
            .selected
            .as_ref()
            .is_some_and(|(extension_id, panel_id)| {
                !panels
                    .iter()
                    .any(|available| available.is(extension_id, panel_id))
            });
        self.panels = panels;
        if self.selected.is_none() || selection_gone {
            self.selected = self
                .panels
                .first()
                .map(|available| (available.extension_id.clone(), available.panel.id.clone()));
            self.update_host(window, cx);
        }
        cx.notify();
    }

    fn is_selected(&self, available: &AvailablePanel) -> bool {
        self.selected
            .as_ref()
            .is_some_and(|(extension_id, panel_id)| available.is(extension_id, panel_id))
    }

    fn select(
        &mut self,
        extension_id: Arc<str>,
        panel_id: Arc<str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected.as_ref() == Some(&(extension_id.clone(), panel_id.clone())) {
            return;
        }
        self.selected = Some((extension_id, panel_id));
        self.update_host(window, cx);
        cx.notify();
    }

    /// Shows the selected panel's view while the container is visible, and
    /// nothing otherwise.
    fn update_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let wanted = self.selected.clone().filter(|_| self.active);
        let current = self.host.as_ref().map(|host| host.read(cx).key());
        if current.as_ref() == wanted.as_ref() {
            return;
        }
        self.host = match (wanted, cx.try_global::<GlobalExtensionViews>()) {
            (Some((extension_id, panel_id)), Some(views)) => {
                let views = views.0.clone();
                Some(cx.new(|cx| {
                    ExtensionViewHost::for_extension(extension_id, panel_id, views, window, cx)
                }))
            }
            _ => None,
        };
    }

    fn selected_panel(&self) -> Option<&AvailablePanel> {
        let (extension_id, panel_id) = self.selected.as_ref()?;
        self.panels
            .iter()
            .find(|available| available.is(extension_id, panel_id))
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_1()
            .flex_wrap()
            .children(self.panels.iter().map(|available| {
                let selected = self.is_selected(available);
                let mut button = Button::new(
                    SharedString::from(format!(
                        "extension-panel-tab-{}-{}",
                        available.extension_id, available.panel.id
                    )),
                    available.panel.title.to_string(),
                )
                .label_size(LabelSize::Small)
                .toggle_state(selected)
                .on_click(cx.listener({
                    let extension_id = available.extension_id.clone();
                    let panel_id = available.panel.id.clone();
                    move |this, _, window, cx| {
                        this.select(extension_id.clone(), panel_id.clone(), window, cx)
                    }
                }));
                if let Some(icon) = available
                    .panel
                    .icon
                    .as_deref()
                    .and_then(|icon| IconName::from_str(icon).ok())
                {
                    button = button.start_icon(Icon::new(icon).size(IconSize::Small));
                }
                button
            }))
    }
}

impl<S: Side> EventEmitter<PanelEvent> for ExtensionPanels<S> {}

impl<S: Side> Focusable for ExtensionPanels<S> {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl<S: Side> Render for ExtensionPanels<S> {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = match self.panels.as_slice() {
            [] => None,
            [only] => Some(
                Label::new(only.panel.title.to_string())
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element(),
            ),
            _ => Some(self.render_tabs(cx).into_any_element()),
        };
        let has_view = self
            .selected
            .as_ref()
            .is_some_and(|(extension_id, panel_id)| {
                cx.try_global::<GlobalExtensionViews>()
                    .is_some_and(|views| views.0.read(cx).view(extension_id, panel_id).is_some())
            });
        let content = match (&self.host, self.selected_panel()) {
            (Some(host), Some(_)) if has_view => host.clone().into_any_element(),
            (_, Some(available)) => Label::new(format!(
                "{} hasn't shown anything here yet.",
                available.extension_name
            ))
            .color(Color::Muted)
            .into_any_element(),
            (_, None) => Label::new("No extension panels on this side.")
                .color(Color::Muted)
                .into_any_element(),
        };
        v_flex()
            .key_context("ExtensionPanels")
            .track_focus(&self.focus_handle)
            .size_full()
            .p_2()
            .gap_2()
            .children(header)
            .child(v_flex().flex_1().min_h_0().child(content))
    }
}

impl<S: Side> Panel for ExtensionPanels<S> {
    fn persistent_name() -> &'static str {
        S::NAME
    }

    fn panel_key() -> &'static str {
        S::NAME
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        match self.position {
            DockPosition::Bottom => px(240.),
            DockPosition::Left | DockPosition::Right => px(300.),
        }
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        match self.panels.as_slice() {
            [] => None,
            [only] => Some(
                only.panel
                    .icon
                    .as_deref()
                    .and_then(|icon| IconName::from_str(icon).ok())
                    .unwrap_or(IconName::Blocks),
            ),
            _ => Some(IconName::Blocks),
        }
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Extension Panels")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        S::toggle_action()
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.active = active;
        self.update_host(window, cx);
        cx.notify();
    }

    fn activation_priority(&self) -> u32 {
        S::PRIORITY
    }

    fn enabled(&self, _: &App) -> bool {
        !self.panels.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lathe_ui::ExtensionViews;
    use extension_host::wasm_host::lathe::LatheSurfaceEvent;
    use gpui::{TestAppContext, VisualTestContext};
    use parking_lot::Mutex;
    use project::Project;
    use workspace::AppState;

    type Shown = Arc<Mutex<Vec<LatheSurfaceEvent>>>;

    struct Fake {
        panels: Arc<Mutex<Vec<AvailablePanel>>>,
        notifier: Entity<()>,
        shown: Shown,
    }

    impl Fake {
        fn set(&self, panels: Vec<AvailablePanel>, cx: &mut VisualTestContext) {
            *self.panels.lock() = panels;
            self.notifier.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
        }

        fn take_shown(&self) -> Vec<LatheSurfaceEvent> {
            std::mem::take(&mut *self.shown.lock())
        }
    }

    fn panel(id: &str, title: &str, dock: LatheDockSide) -> AvailablePanel {
        AvailablePanel {
            extension_id: "tables".into(),
            extension_name: "Tables".into(),
            panel: LathePanel {
                id: id.into(),
                title: title.into(),
                icon: Some("list_tree".into()),
                dock,
            },
        }
    }

    async fn open(cx: &mut TestAppContext) -> (Entity<Workspace>, Fake, &mut VisualTestContext) {
        let shown = Shown::default();
        let fake = cx.update(|cx| {
            let app_state = AppState::test(cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
            let views = cx.new(|_| {
                ExtensionViews::with_surface_sink({
                    let shown = shown.clone();
                    Arc::new(move |_, event, _| shown.lock().push(event))
                })
            });
            cx.set_global(GlobalExtensionViews(views));
            let fake = Fake {
                panels: Arc::default(),
                notifier: cx.new(|_| ()),
                shown,
            };
            let panels = fake.panels.clone();
            let notifier = fake.notifier.clone();
            cx.set_global(PanelBackend {
                panels: Arc::new(move |_| panels.lock().clone()),
                observe: Arc::new(move |callback, cx| {
                    Some(cx.observe(&notifier, move |_, cx| callback(cx)))
                }),
            });
            register(cx);
            (app_state, fake)
        });
        let (app_state, fake) = fake;
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            Workspace::new(None, project, app_state.clone(), window, cx)
        });
        cx.run_until_parked();
        (workspace, fake, cx)
    }

    fn toggle(workspace: &Entity<Workspace>, id: &str, cx: &mut VisualTestContext) {
        let action = TogglePanel {
            extension_id: "tables".into(),
            panel: id.into(),
        };
        workspace.update_in(cx, |workspace, window, cx| {
            toggle_panel(workspace, &action, window, cx)
        });
        cx.run_until_parked();
    }

    fn right(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<ExtensionPanels<RightSide>> {
        workspace.read_with(cx, |workspace, cx| {
            workspace
                .panel::<ExtensionPanels<RightSide>>(cx)
                .expect("the right container is added to every workspace")
        })
    }

    fn icon_and_enabled(
        container: &Entity<ExtensionPanels<RightSide>>,
        cx: &mut VisualTestContext,
    ) -> (Option<IconName>, bool) {
        cx.update(|window, cx| {
            let container = container.read(cx);
            (container.icon(window, cx), container.enabled(cx))
        })
    }

    fn right_dock_open(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
        workspace.read_with(cx, |workspace, cx| {
            workspace.right_dock().read(cx).is_open()
        })
    }

    #[gpui::test]
    async fn test_containers_appear_with_panels(cx: &mut TestAppContext) {
        let (workspace, fake, cx) = open(cx).await;
        let container = right(&workspace, cx);
        assert_eq!(
            icon_and_enabled(&container, cx),
            (None, false),
            "no button while no extension has a panel on this side"
        );

        fake.set(vec![panel("counter", "Counter", LatheDockSide::Right)], cx);
        assert_eq!(
            icon_and_enabled(&container, cx),
            (Some(IconName::ListTree), true)
        );
        workspace.read_with(cx, |workspace, cx| {
            let left = workspace.panel::<ExtensionPanels<LeftSide>>(cx).unwrap();
            assert!(
                left.read(cx).panels.is_empty(),
                "a right panel stays on the right"
            );
        });

        fake.set(Vec::new(), cx);
        assert_eq!(
            icon_and_enabled(&container, cx),
            (None, false),
            "the button goes away when the extension unloads"
        );
    }

    #[gpui::test]
    async fn test_toggle_select_and_close(cx: &mut TestAppContext) {
        let (workspace, fake, cx) = open(cx).await;
        fake.set(
            vec![
                panel("counter", "Counter", LatheDockSide::Right),
                panel("files", "Files", LatheDockSide::Right),
            ],
            cx,
        );
        assert!(
            fake.take_shown().is_empty(),
            "nothing is shown until opened"
        );

        toggle(&workspace, "files", cx);
        assert!(right_dock_open(&workspace, cx));
        let container = right(&workspace, cx);
        assert_eq!(
            container.read_with(cx, |container, _| container.selected.clone()),
            Some(("tables".into(), "files".into()))
        );
        assert_eq!(
            fake.take_shown(),
            vec![LatheSurfaceEvent::ViewShown("files".into())]
        );

        toggle(&workspace, "counter", cx);
        let mut events = fake.take_shown();
        events.sort_by_key(|event| format!("{event:?}"));
        assert_eq!(
            events,
            vec![
                LatheSurfaceEvent::ViewHidden("files".into()),
                LatheSurfaceEvent::ViewShown("counter".into()),
            ],
            "switching tabs hides one view and shows the other"
        );

        // What running the toggle from the palette looks like: focus has
        // moved away from the panel, which is still showing.
        workspace.update_in(cx, |workspace, window, cx| {
            let center = workspace.active_pane().read(cx).focus_handle(cx);
            window.focus(&center, cx);
        });
        cx.run_until_parked();
        toggle(&workspace, "counter", cx);
        assert!(
            !right_dock_open(&workspace, cx),
            "toggling a showing panel closes it, focused or not"
        );
        assert_eq!(
            fake.take_shown(),
            vec![LatheSurfaceEvent::ViewHidden("counter".into())],
            "closing the dock hides the view"
        );

        toggle(&workspace, "counter", cx);
        assert_eq!(
            fake.take_shown(),
            vec![LatheSurfaceEvent::ViewShown("counter".into())]
        );
        fake.set(vec![panel("files", "Files", LatheDockSide::Right)], cx);
        let mut events = fake.take_shown();
        events.sort_by_key(|event| format!("{event:?}"));
        assert_eq!(
            events,
            vec![
                LatheSurfaceEvent::ViewHidden("counter".into()),
                LatheSurfaceEvent::ViewShown("files".into()),
            ],
            "when the selected panel goes away, the next one is selected"
        );
    }

    #[gpui::test]
    async fn test_unknown_panel_is_an_error(cx: &mut TestAppContext) {
        let (workspace, _fake, cx) = open(cx).await;
        toggle(&workspace, "missing", cx);
        assert!(!right_dock_open(&workspace, cx));
    }
}
