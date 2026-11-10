//! The UI side of the Lathe extension API: status bar text, notifications,
//! prompts, and delivering buffer events from projects to extensions.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use collections::HashMap;
use extension_host::ExtensionStore;
use extension_host::wasm_host::lathe::{
    LatheBufferEvent, LatheCapability, LatheLocation, LatheStatusItem, LatheSurfaceEvent,
    LatheUiDelegate, NotificationLevel, PickItem, TaskReveal, TaskSpec,
};
use extension_host::wasm_host::lathe_views::LatheView;
use futures::channel::oneshot;
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::AnyElement;
use gpui::{
    App, AppContext as _, AsyncApp, Context, DismissEvent, Entity, Global, IntoElement, Render,
    SharedString, Subscription, Task, WeakEntity, Window,
};
use language::{Buffer, BufferEvent};
use picker::{Picker, PickerDelegate};
use project::Project;
use project::buffer_store::BufferStoreEvent;
use ui::{ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::notifications::{NotificationId, simple_message_notification::MessageNotification};
use workspace::{ItemHandle, MultiWorkspace, StatusItemView, Toast, Workspace};

use crate::lathe_editor::{ExtensionBufferDelegate, ExtensionEditedBuffers};

/// How long typing must pause before extensions get a `changed` event.
pub const CHANGE_DEBOUNCE: Duration = Duration::from_millis(300);

pub fn init(cx: &mut App) {
    let status_texts = cx.new(|_| ExtensionStatusTexts::default());
    cx.set_global(GlobalExtensionStatusTexts(status_texts.clone()));

    let views = cx.new(|_| ExtensionViews {
        surface_sink: Some(Arc::new(send_surface_event)),
        ..ExtensionViews::default()
    });
    cx.set_global(GlobalExtensionViews(views.clone()));

    let store = ExtensionStore::global(cx);
    let delegate = Arc::new(LatheUi {
        status_texts: status_texts.downgrade(),
        views: views.downgrade(),
    });
    store.read(cx).wasm_host.set_lathe_ui(delegate);
    crate::lathe_decorations::init(cx);

    // Drop status text of extensions that were unloaded.
    cx.observe(&store, move |store, cx| {
        let loaded = store
            .read(cx)
            .wasm_extensions
            .iter()
            .map(|(manifest, _)| manifest.id.clone())
            .collect::<collections::HashSet<_>>();
        status_texts.update(cx, |status_texts, cx| {
            let before = status_texts.texts.len();
            status_texts.texts.retain(|id, _| loaded.contains(id));
            if status_texts.texts.len() != before {
                cx.notify();
            }
        });
        views.update(cx, |views, cx| {
            let before = views.views.len();
            views
                .views
                .retain(|(extension_id, _), _| loaded.contains(extension_id));
            views
                .shown
                .retain(|(extension_id, _), _| loaded.contains(extension_id));
            if views.views.len() != before {
                cx.notify();
            }
        });
    })
    .detach();

    let edited_by_extensions = ExtensionEditedBuffers::default();
    crate::lathe_preview::init(edited_by_extensions.clone(), cx);
    let sink = EventSink {
        wants: Arc::new(|capability, cx| {
            ExtensionStore::try_global(cx).is_some_and(|store| wants(store.read(cx), capability))
        }),
        dispatch: Arc::new(dispatch),
        edited_by_extensions,
    };
    cx.observe_new(move |project: &mut Project, _window, cx| {
        if project.is_local() {
            watch_project_buffers(project, sink.clone(), cx);
        }
    })
    .detach();
}

/// Where buffer events go. Production sends them to the extension store;
/// tests record them.
#[derive(Clone)]
pub struct EventSink {
    pub wants: Arc<dyn Fn(LatheCapability, &App) -> bool>,
    pub dispatch: Arc<dyn Fn(LatheBufferEvent, &Entity<Buffer>, &ExtensionEditedBuffers, &mut App)>,
    pub edited_by_extensions: ExtensionEditedBuffers,
}

/// Asks the user to allow an extension's Lathe capabilities. Resolves to
/// whether they agreed. `confirm_label` names the action that follows, such as
/// "Install and Allow".
pub fn ask_for_lathe_consent(
    extension_name: &str,
    capabilities: &[LatheCapability],
    confirm_label: &str,
    window: &mut Window,
    cx: &mut App,
) -> Task<bool> {
    let mut detail = String::from("It will be able to:\n");
    for capability in capabilities {
        detail.push_str("• ");
        detail.push_str(capability_description(*capability));
        detail.push('\n');
    }
    detail.push_str("\nYou can revoke this later from its card on the Extensions page.");
    let answer = window.prompt(
        gpui::PromptLevel::Info,
        &format!("Allow {extension_name} to use Lathe features?"),
        Some(&detail),
        &[confirm_label, "Cancel"],
        cx,
    );
    cx.background_spawn(async move { matches!(answer.await, Ok(0)) })
}

fn capability_description(capability: LatheCapability) -> &'static str {
    match capability {
        LatheCapability::BufferRead => {
            "read the text of the file you run its commands in, or that its events are about"
        }
        LatheCapability::BufferWrite => "edit that file and move the cursor",
        LatheCapability::EventsOpen => "run when you open a file",
        LatheCapability::EventsSave => "run when you save a file",
        LatheCapability::EventsChange => "run as you type",
        LatheCapability::UiStatus => "show text in the status bar and show notifications",
        LatheCapability::UiPrompt => "ask you for input while its commands run",
        LatheCapability::TasksSpawn => "run commands in a terminal on your machine",
        LatheCapability::UiViews => "show its own views inside Lathe",
        LatheCapability::UiDecorations => "highlight and annotate text in the editor",
    }
}

/// Asks for consent for an installed extension's ungranted capabilities and
/// grants them if the user agrees.
pub fn request_missing_lathe_capabilities(
    extension_id: Arc<str>,
    window: &mut Window,
    cx: &mut App,
) {
    let store = ExtensionStore::global(cx);
    let (name, missing) = {
        let store = store.read(cx);
        let name = store
            .extension_manifest_for_id(&extension_id)
            .map(|manifest| manifest.name.clone())
            .unwrap_or_else(|| extension_id.to_string());
        (
            name,
            store.lathe_capabilities_needing_consent(&extension_id),
        )
    };
    if missing.is_empty() {
        return;
    }
    let consent = ask_for_lathe_consent(&name, &missing, "Allow", window, cx);
    cx.spawn(async move |cx| {
        if consent.await {
            store
                .update(cx, |store, cx| {
                    store.grant_lathe_capabilities(extension_id, missing, cx)
                })
                .await
                .log_err();
        }
    })
    .detach();
}

/// Adds the extension status bar item to a workspace's status bar.
pub fn add_status_item(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(status_texts) = cx
        .try_global::<GlobalExtensionStatusTexts>()
        .map(|global| global.0.clone())
    else {
        return;
    };
    let item = cx.new(|cx| ExtensionStatusBarItem::new(status_texts, cx));
    workspace.status_bar().update(cx, |status_bar, cx| {
        status_bar.add_right_item(item, window, cx);
    });
}

/// What an extension shows in its status bar slot: plain text from 0.1's
/// `set-status-text`, or 0.2's clickable status item. Setting one replaces
/// the other.
#[derive(Debug, Clone, PartialEq)]
enum StatusEntry {
    Text {
        text: SharedString,
        tooltip: Option<SharedString>,
    },
    Item(LatheStatusItem),
}

#[derive(Default)]
pub struct ExtensionStatusTexts {
    texts: BTreeMap<Arc<str>, StatusEntry>,
}

struct GlobalExtensionStatusTexts(Entity<ExtensionStatusTexts>);

impl Global for GlobalExtensionStatusTexts {}

struct ExtensionStatusBarItem {
    status_texts: Entity<ExtensionStatusTexts>,
    _subscription: Subscription,
}

impl ExtensionStatusBarItem {
    fn new(status_texts: Entity<ExtensionStatusTexts>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&status_texts, |_, _, cx| cx.notify());
        Self {
            status_texts,
            _subscription: subscription,
        }
    }
}

impl Render for ExtensionStatusBarItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex().gap_2().children(
            self.status_texts
                .read(cx)
                .texts
                .iter()
                .map(|(extension_id, entry)| render_status_entry(extension_id, entry)),
        )
    }
}

fn render_status_entry(extension_id: &Arc<str>, entry: &StatusEntry) -> AnyElement {
    let id = SharedString::from(format!("extension-status-{extension_id}"));
    match entry {
        StatusEntry::Text { text, tooltip } => {
            let tooltip = tooltip
                .clone()
                .unwrap_or_else(|| extension_id.to_string().into());
            div()
                .id(id)
                .child(Label::new(text.clone()).size(LabelSize::Small))
                .tooltip(Tooltip::text(tooltip))
                .into_any_element()
        }
        StatusEntry::Item(item) => {
            let tooltip: SharedString = item
                .tooltip
                .as_deref()
                .unwrap_or(extension_id.as_ref())
                .to_string()
                .into();
            let color = crate::lathe_view_renderer::tone_color(item.tone);
            let mut button = Button::new(id, item.label.to_string())
                .label_size(LabelSize::Small)
                .color(color)
                .tooltip(Tooltip::text(tooltip))
                .on_click({
                    let extension_id = extension_id.clone();
                    move |_, _, cx| {
                        send_surface_event(
                            extension_id.clone(),
                            LatheSurfaceEvent::StatusItemClicked,
                            cx,
                        )
                    }
                });
            if let Some(icon) = item
                .icon
                .as_deref()
                .and_then(crate::lathe_view_renderer::icon_name)
            {
                button = button.start_icon(Icon::new(icon).size(IconSize::Small).color(color));
            }
            button.into_any_element()
        }
    }
}

/// Sends a surface event to an extension, logging failures.
fn send_surface_event(extension_id: Arc<str>, event: LatheSurfaceEvent, cx: &mut App) {
    let Some(store) = ExtensionStore::try_global(cx) else {
        return;
    };
    let task = store.update(cx, |store, cx| {
        store.dispatch_lathe_surface_event(&extension_id, event, None, cx)
    });
    cx.background_spawn(async move { task.await.log_err() })
        .detach();
}

impl StatusItemView for ExtensionStatusBarItem {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    // Each extension decides whether its text shows; uninstalling the extension
    // or the extension clearing its text hides it.
    fn hide_setting(&self, _cx: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}

struct LatheUi {
    status_texts: WeakEntity<ExtensionStatusTexts>,
    views: WeakEntity<ExtensionViews>,
}

/// Every view extensions have sent, keyed by (extension id, view id). Surfaces
/// that show views (panels, previews) read from here and redraw on notify.
/// Where `view-shown`/`view-hidden` go. Production sends them to the
/// extension; tests record them.
pub type SurfaceEventSink = Arc<dyn Fn(Arc<str>, LatheSurfaceEvent, &mut App)>;

#[derive(Default)]
pub struct ExtensionViews {
    views: BTreeMap<(Arc<str>, Arc<str>), LatheView>,
    /// How many surfaces show each view.
    shown: HashMap<(Arc<str>, Arc<str>), usize>,
    surface_sink: Option<SurfaceEventSink>,
}

impl ExtensionViews {
    #[cfg(test)]
    pub(crate) fn with_surface_sink(sink: SurfaceEventSink) -> Self {
        Self {
            surface_sink: Some(sink),
            ..Self::default()
        }
    }

    /// Records that a surface started showing a view. The first one tells
    /// the extension the view is shown.
    pub(crate) fn attach(&mut self, extension_id: Arc<str>, view_id: Arc<str>, cx: &mut App) {
        let count = self
            .shown
            .entry((extension_id.clone(), view_id.clone()))
            .or_default();
        *count += 1;
        if *count == 1
            && let Some(sink) = &self.surface_sink
        {
            sink(extension_id, LatheSurfaceEvent::ViewShown(view_id), cx);
        }
    }

    /// Records that a surface stopped showing a view. The last one tells the
    /// extension the view is hidden.
    pub(crate) fn detach(&mut self, extension_id: Arc<str>, view_id: Arc<str>, cx: &mut App) {
        let key = (extension_id.clone(), view_id.clone());
        let Some(count) = self.shown.get_mut(&key) else {
            return;
        };
        *count -= 1;
        if *count == 0 {
            self.shown.remove(&key);
            if let Some(sink) = &self.surface_sink {
                sink(extension_id, LatheSurfaceEvent::ViewHidden(view_id), cx);
            }
        }
    }

    /// Stores or replaces a view. Callers notify.
    pub(crate) fn set_view(&mut self, extension_id: Arc<str>, view_id: Arc<str>, view: LatheView) {
        self.views.insert((extension_id, view_id), view);
    }

    pub(crate) fn remove(&mut self, extension_id: Arc<str>, view_id: Arc<str>) -> bool {
        self.views.remove(&(extension_id, view_id)).is_some()
    }

    pub fn view(&self, extension_id: &str, view_id: &str) -> Option<&LatheView> {
        self.views
            .iter()
            .find(|((extension, view), _)| {
                extension.as_ref() == extension_id && view.as_ref() == view_id
            })
            .map(|(_, view)| view)
    }
}

pub struct GlobalExtensionViews(pub Entity<ExtensionViews>);

impl Global for GlobalExtensionViews {}

/// The workspace of the focused window, or of the first open window.
fn active_workspace(
    cx: &mut App,
) -> Option<(gpui::WindowHandle<MultiWorkspace>, Entity<Workspace>)> {
    let windows = cx
        .active_window()
        .into_iter()
        .chain(cx.windows())
        .filter_map(|window| window.downcast::<MultiWorkspace>())
        .collect::<Vec<_>>();
    windows.into_iter().find_map(|window| {
        let workspace = window.read(cx).ok()?.workspace().clone();
        Some((window, workspace))
    })
}

struct ExtensionNotification;

impl LatheUiDelegate for LatheUi {
    fn set_status_text(
        &self,
        extension_id: Arc<str>,
        text: Option<String>,
        tooltip: Option<String>,
        cx: &mut AsyncApp,
    ) {
        let Some(status_texts) = self.status_texts.upgrade() else {
            return;
        };
        status_texts.update(cx, |status_texts, cx| {
            match text {
                Some(text) => {
                    status_texts.texts.insert(
                        extension_id,
                        StatusEntry::Text {
                            text: text.into(),
                            tooltip: tooltip.map(Into::into),
                        },
                    );
                }
                None => {
                    status_texts.texts.remove(&extension_id);
                }
            }
            cx.notify();
        });
    }

    fn show_notification(
        &self,
        extension_name: Arc<str>,
        level: NotificationLevel,
        message: String,
        cx: &mut AsyncApp,
    ) {
        cx.update(|cx| {
            let Some((_, workspace)) = active_workspace(cx) else {
                log::info!("{extension_name}: {message}");
                return;
            };
            let message = format!("{extension_name}: {message}");
            workspace.update(cx, |workspace, cx| match level {
                NotificationLevel::Info => {
                    workspace.show_toast(
                        Toast::new(
                            NotificationId::composite::<ExtensionNotification>(SharedString::from(
                                extension_name.to_string(),
                            )),
                            message,
                        )
                        .autohide(),
                        cx,
                    );
                }
                NotificationLevel::Warning | NotificationLevel::Error => {
                    workspace.show_notification(
                        NotificationId::composite::<ExtensionNotification>(SharedString::from(
                            extension_name.to_string(),
                        )),
                        cx,
                        |cx| cx.new(|cx| MessageNotification::new(message, cx)),
                    );
                }
            });
        });
    }

    fn prompt_input(
        &self,
        extension_name: Arc<str>,
        label: String,
        placeholder: Option<String>,
        cx: &mut AsyncApp,
    ) -> Result<oneshot::Receiver<Option<String>>> {
        let placeholder = placeholder.unwrap_or_else(|| label.clone());
        open_picker(
            ExtensionPromptDelegate::input(format!("{extension_name}: {placeholder}")),
            cx,
        )
    }

    fn update_view(
        &self,
        extension_id: Arc<str>,
        view_id: Arc<str>,
        view: LatheView,
        cx: &mut AsyncApp,
    ) {
        let Some(views) = self.views.upgrade() else {
            return;
        };
        views.update(cx, |views, cx| {
            views.set_view(extension_id, view_id, view);
            cx.notify();
        });
    }

    fn set_status_item(
        &self,
        extension_id: Arc<str>,
        item: Option<LatheStatusItem>,
        cx: &mut AsyncApp,
    ) {
        let Some(status_texts) = self.status_texts.upgrade() else {
            return;
        };
        status_texts.update(cx, |status_texts, cx| {
            match item {
                Some(item) => {
                    status_texts
                        .texts
                        .insert(extension_id, StatusEntry::Item(item));
                }
                None => {
                    status_texts.texts.remove(&extension_id);
                }
            }
            cx.notify();
        });
    }

    fn open_location(&self, location: LatheLocation, cx: &mut AsyncApp) -> Result<()> {
        cx.update(|cx| {
            let (window, workspace) =
                active_workspace(cx).context("no open window to open the file in")?;
            let abs_path =
                resolve_location_path(workspace.read(cx).project().read(cx), &location.path, cx)
                    .with_context(|| {
                        format!("no folder open in the window contains {:?}", location.path)
                    })?;
            window.update(cx, |_, window, cx| {
                let open = workspace.update(cx, |workspace, cx| {
                    workspace.open_abs_path(
                        abs_path,
                        workspace::OpenOptions {
                            visible: Some(workspace::OpenVisible::None),
                            focus: Some(true),
                            ..Default::default()
                        },
                        window,
                        cx,
                    )
                });
                let Some(row) = location.row else {
                    open.detach_and_log_err(cx);
                    return;
                };
                let column = location.column.unwrap_or(0);
                window
                    .spawn(cx, async move |cx| {
                        let item = open.await?;
                        if let Some(editor) =
                            cx.update(|_, cx| item.act_as::<editor::Editor>(cx))?
                        {
                            editor.update_in(cx, |editor, window, cx| {
                                let point = editor
                                    .buffer()
                                    .read(cx)
                                    .as_singleton()
                                    .map(|buffer| {
                                        buffer.read(cx).snapshot().clip_point(
                                            language::Point::new(row, column),
                                            text::Bias::Left,
                                        )
                                    })
                                    .unwrap_or_default();
                                editor.go_to_singleton_buffer_point(point, window, cx);
                            })?;
                        }
                        anyhow::Ok(())
                    })
                    .detach_and_log_err(cx);
            })?;
            anyhow::Ok(())
        })
    }

    fn remove_view(&self, extension_id: Arc<str>, view_id: Arc<str>, cx: &mut AsyncApp) {
        let Some(views) = self.views.upgrade() else {
            return;
        };
        views.update(cx, |views, cx| {
            if views.remove(extension_id, view_id) {
                cx.notify();
            }
        });
    }

    fn spawn_task(&self, task: TaskSpec, cx: &mut AsyncApp) -> Result<()> {
        cx.update(|cx| {
            let (window, workspace) =
                active_workspace(cx).context("no open window to run the task in")?;
            let cwd = task.cwd.clone().map(std::path::PathBuf::from).or_else(|| {
                let project = workspace.read(cx).project().read(cx);
                project
                    .visible_worktrees(cx)
                    .next()
                    .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            });
            let spawn = spawn_in_terminal(task, cwd);
            window.update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.spawn_in_terminal(spawn, window, cx).detach();
                });
            })?;
            anyhow::Ok(())
        })
    }

    fn quick_pick(
        &self,
        extension_name: Arc<str>,
        label: String,
        items: Vec<PickItem>,
        cx: &mut AsyncApp,
    ) -> Result<oneshot::Receiver<Option<String>>> {
        open_picker(
            ExtensionPromptDelegate::pick(format!("{extension_name}: {label}"), items),
            cx,
        )
    }
}

/// An absolute path for `path`: itself if absolute, otherwise the first open
/// folder that has an entry at that relative path, or failing that the first
/// open folder joined with it.
fn resolve_location_path(project: &Project, path: &str, cx: &App) -> Option<std::path::PathBuf> {
    let path = std::path::Path::new(path);
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }
    let path_style = project.path_style(cx);
    let relative = util::rel_path::RelPath::new(path, path_style).ok()?;
    let worktrees = project.visible_worktrees(cx).collect::<Vec<_>>();
    let containing = worktrees
        .iter()
        .find(|worktree| worktree.read(cx).entry_for_path(&relative).is_some())
        .or_else(|| worktrees.first())?;
    Some(containing.read(cx).abs_path().join(path))
}

fn spawn_in_terminal(task: TaskSpec, cwd: Option<std::path::PathBuf>) -> task::SpawnInTerminal {
    let command_label = std::iter::once(task.command.as_str())
        .chain(task.args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    task::SpawnInTerminal {
        // Same label, same id: a run waits for the previous one and reuses its
        // terminal instead of opening a new tab on every save.
        id: task::TaskId(format!("lathe-extension-task:{}", task.label)),
        full_label: task.label.clone(),
        label: task.label,
        command: Some(task.command),
        args: task.args,
        command_label,
        cwd,
        env: task.env.into_iter().collect(),
        use_new_terminal: false,
        allow_concurrent_runs: false,
        reveal: match task.reveal {
            TaskReveal::Always => task::RevealStrategy::Always,
            TaskReveal::NoFocus => task::RevealStrategy::NoFocus,
            TaskReveal::Never => task::RevealStrategy::Never,
        },
        show_summary: true,
        show_command: true,
        show_rerun: true,
        ..Default::default()
    }
}

fn open_picker(
    mut delegate: ExtensionPromptDelegate,
    cx: &mut AsyncApp,
) -> Result<oneshot::Receiver<Option<String>>> {
    let (sender, receiver) = oneshot::channel();
    delegate.answer = Some(sender);
    cx.update(|cx| {
        let (window, workspace) = active_workspace(cx).context("no open window to ask in")?;
        window.update(cx, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    Picker::uniform_list(delegate, window, cx)
                });
            });
        })?;
        anyhow::Ok(())
    })?;
    Ok(receiver)
}

/// A picker that either returns the chosen item's id, or in input mode
/// returns the typed text. Dropping it without an answer yields `None`.
struct ExtensionPromptDelegate {
    title: Arc<str>,
    items: Vec<PickItem>,
    input_mode: bool,
    matches: Vec<StringMatch>,
    selected_index: usize,
    query: String,
    answer: Option<oneshot::Sender<Option<String>>>,
}

impl ExtensionPromptDelegate {
    fn input(title: String) -> Self {
        Self {
            title: title.into(),
            items: Vec::new(),
            input_mode: true,
            matches: Vec::new(),
            selected_index: 0,
            query: String::new(),
            answer: None,
        }
    }

    fn pick(title: String, items: Vec<PickItem>) -> Self {
        let matches = items
            .iter()
            .enumerate()
            .map(|(index, item)| StringMatch {
                candidate_id: index,
                score: 0.0,
                positions: Vec::new(),
                string: item.label.clone(),
            })
            .collect();
        Self {
            title: title.into(),
            items,
            input_mode: false,
            matches,
            selected_index: 0,
            query: String::new(),
            answer: None,
        }
    }

    fn answer(&mut self, value: Option<String>) {
        if let Some(answer) = self.answer.take() {
            answer.send(value).ok();
        }
    }
}

impl PickerDelegate for ExtensionPromptDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "extension-prompt"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        self.title.clone()
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.query = query.clone();
        if self.input_mode {
            return Task::ready(());
        }
        let background = cx.background_executor().clone();
        let candidates = self
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| StringMatchCandidate::new(index, &item.label))
            .collect::<Vec<_>>();
        cx.spawn_in(window, async move |this, cx| {
            let matches = if query.is_empty() {
                candidates
                    .into_iter()
                    .map(|candidate| StringMatch {
                        candidate_id: candidate.id,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    100,
                    &Default::default(),
                    background,
                )
                .await
            };
            this.update(cx, |this, _| {
                this.delegate.matches = matches;
                this.delegate.selected_index = 0;
            })
            .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let value = if self.input_mode {
            Some(self.query.clone())
        } else {
            self.matches
                .get(self.selected_index)
                .and_then(|string_match| self.items.get(string_match.candidate_id))
                .map(|item| item.id.clone())
        };
        if value.is_none() {
            return;
        }
        self.answer(value);
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {
        self.answer(None);
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        if self.input_mode {
            Some("Press Enter to submit".into())
        } else {
            Some("No matches".into())
        }
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let string_match = self.matches.get(ix)?;
        let item = self.items.get(string_match.candidate_id)?;
        Some(
            ListItem::new(("extension-prompt-item", ix))
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .justify_between()
                        .child(Label::new(item.label.clone()))
                        .children(item.detail.clone().map(|detail| {
                            Label::new(detail)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                        })),
                ),
        )
    }
}

fn wants(store: &ExtensionStore, capability: LatheCapability) -> bool {
    store
        .wasm_extensions
        .iter()
        .any(|(_, extension)| extension.lathe_capabilities.contains(&capability))
}

fn dispatch(
    event: LatheBufferEvent,
    buffer: &Entity<Buffer>,
    edited_by_extensions: &ExtensionEditedBuffers,
    cx: &mut App,
) {
    let Some(store) = ExtensionStore::try_global(cx) else {
        return;
    };
    let delegate = Arc::new(ExtensionBufferDelegate::new(
        buffer.downgrade(),
        edited_by_extensions.clone(),
    ));
    let task = store.update(cx, |store, cx| {
        store.dispatch_lathe_buffer_event(event, delegate, cx)
    });
    cx.spawn(async move |cx| {
        let paused = task.await;
        if paused.is_empty() {
            return;
        }
        cx.update(|cx| {
            let Some((_, workspace)) = active_workspace(cx) else {
                return;
            };
            for (extension_id, extension_name) in paused {
                workspace.update(cx, |workspace, cx| {
                    workspace.show_notification(
                        NotificationId::composite::<ExtensionNotification>(SharedString::from(
                            format!("paused-{extension_id}"),
                        )),
                        cx,
                        |cx| {
                            cx.new(|cx| {
                                MessageNotification::new(
                                    format!(
                                        "{extension_name} kept taking too long to respond to file events. \
                                         Its events are paused until it is reloaded."
                                    ),
                                    cx,
                                )
                            })
                        },
                    );
                });
            }
        });
    })
    .detach();
}

pub fn watch_project_buffers(project: &mut Project, sink: EventSink, cx: &mut Context<Project>) {
    let buffer_store = project.buffer_store().clone();
    // Shared between the buffer store subscription and each buffer's
    // subscription, all on the main thread.
    let watched = Rc::new(RefCell::new(HashMap::<
        text::BufferId,
        (Subscription, Option<Task<()>>),
    >::default()));

    cx.subscribe(&buffer_store, move |_project, _buffer_store, event, cx| {
        match event {
            BufferStoreEvent::BufferAdded(buffer) => {
                // Only files: untitled buffers and other scratch text are not
                // reported.
                if buffer.read(cx).file().is_none() {
                    return;
                }
                send(&sink, LatheBufferEvent::Opened, buffer, cx);
                let buffer_id = buffer.read(cx).remote_id();
                let subscription = cx.subscribe(buffer, {
                    let watched = watched.clone();
                    let sink = sink.clone();
                    move |_project, buffer, event, cx| {
                        on_buffer_event(&buffer, event, &watched, &sink, cx)
                    }
                });
                watched.borrow_mut().insert(buffer_id, (subscription, None));
            }
            BufferStoreEvent::BufferDropped(buffer_id) => {
                watched.borrow_mut().remove(buffer_id);
                sink.edited_by_extensions.forget(*buffer_id);
            }
            _ => {}
        }
    })
    .detach();
}

fn send(sink: &EventSink, event: LatheBufferEvent, buffer: &Entity<Buffer>, cx: &mut App) {
    if (sink.wants)(event.capability(), cx) {
        (sink.dispatch)(event, buffer, &sink.edited_by_extensions, cx);
    }
}

fn on_buffer_event(
    buffer: &Entity<Buffer>,
    event: &BufferEvent,
    watched: &Rc<RefCell<HashMap<text::BufferId, (Subscription, Option<Task<()>>)>>>,
    sink: &EventSink,
    cx: &mut Context<Project>,
) {
    match event {
        BufferEvent::Saved => send(sink, LatheBufferEvent::Saved, buffer, cx),
        BufferEvent::Edited { .. } => {
            if !(sink.wants)(LatheCapability::EventsChange, cx) {
                return;
            }
            let buffer_id = buffer.read(cx).remote_id();
            let weak_buffer = buffer.downgrade();
            let sink = sink.clone();
            let debounce = cx.spawn(async move |_project, cx| {
                cx.background_executor().timer(CHANGE_DEBOUNCE).await;
                cx.update(|cx| {
                    let Some(buffer) = weak_buffer.upgrade() else {
                        return;
                    };
                    if sink
                        .edited_by_extensions
                        .is_unchanged_since_extension_edit(buffer.read(cx))
                    {
                        return;
                    }
                    send(&sink, LatheBufferEvent::Changed, &buffer, cx);
                });
            });
            if let Some((_, pending)) = watched.borrow_mut().get_mut(&buffer_id) {
                *pending = Some(debounce);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lathe_editor::ExtensionBufferDelegate;
    use extension_host::wasm_host::lathe::LatheEditorDelegate as _;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::Path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
        });
    }

    #[derive(Clone, Default)]
    struct Recorded(Arc<parking_lot::Mutex<Vec<(LatheBufferEvent, String)>>>);

    impl Recorded {
        fn take(&self) -> Vec<(LatheBufferEvent, String)> {
            std::mem::take(&mut *self.0.lock())
        }
    }

    /// A sink that records events, and on `changed` edits the buffer the way an
    /// extension's handler would, through the extension buffer delegate.
    fn recording_sink(recorded: Recorded, wants_changes: bool) -> EventSink {
        EventSink {
            wants: Arc::new(move |capability, _| {
                wants_changes || capability != LatheCapability::EventsChange
            }),
            dispatch: Arc::new(move |event, buffer, edited_by_extensions, cx| {
                recorded.0.lock().push((event, buffer.read(cx).text()));
                if event == LatheBufferEvent::Changed {
                    let delegate = ExtensionBufferDelegate::new(
                        buffer.downgrade(),
                        edited_by_extensions.clone(),
                    );
                    cx.spawn(async move |cx| {
                        let snapshot = delegate.snapshot(cx).unwrap().unwrap();
                        let end = snapshot.text.len();
                        delegate
                            .edit(snapshot.version, vec![(end..end, "!".into())], cx)
                            .unwrap();
                    })
                    .detach();
                }
            }),
            edited_by_extensions: ExtensionEditedBuffers::default(),
        }
    }

    #[gpui::test]
    async fn test_project_buffer_events(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/root", json!({ "notes.txt": "hello" }))
            .await;
        let project = Project::test(fs.clone(), [Path::new("/root")], cx).await;

        let recorded = Recorded::default();
        project.update(cx, |project, cx| {
            watch_project_buffers(project, recording_sink(recorded.clone(), true), cx)
        });

        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer("/root/notes.txt", cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            recorded.take(),
            vec![(LatheBufferEvent::Opened, "hello".into())]
        );

        buffer.update(cx, |buffer, cx| buffer.edit([(5..5, " w")], None, cx));
        cx.executor().advance_clock(CHANGE_DEBOUNCE / 2);
        buffer.update(cx, |buffer, cx| buffer.edit([(7..7, "orld")], None, cx));
        cx.executor().advance_clock(CHANGE_DEBOUNCE / 2);
        cx.run_until_parked();
        assert_eq!(recorded.take(), vec![], "still typing");

        cx.executor().advance_clock(CHANGE_DEBOUNCE);
        cx.run_until_parked();
        assert_eq!(
            recorded.take(),
            vec![(LatheBufferEvent::Changed, "hello world".into())],
            "one event once typing paused"
        );
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "hello world!"
        );

        cx.executor().advance_clock(CHANGE_DEBOUNCE * 2);
        cx.run_until_parked();
        assert_eq!(
            recorded.take(),
            vec![],
            "the extension's own edit is not reported back as a change"
        );

        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, ">")], None, cx));
        cx.executor().advance_clock(CHANGE_DEBOUNCE);
        cx.run_until_parked();
        assert_eq!(
            recorded.take(),
            vec![(LatheBufferEvent::Changed, ">hello world!".into())],
            "a user edit after the extension's edit is reported"
        );

        project
            .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
            .await
            .unwrap();
        cx.run_until_parked();
        let saved = recorded.take();
        assert!(
            saved.iter().any(
                |(event, text)| *event == LatheBufferEvent::Saved && text.starts_with(">hello")
            ),
            "{saved:?}"
        );
    }

    #[gpui::test]
    async fn test_no_change_tracking_without_listeners(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/root", json!({ "notes.txt": "hello" }))
            .await;
        let project = Project::test(fs.clone(), [Path::new("/root")], cx).await;
        let recorded = Recorded::default();
        project.update(cx, |project, cx| {
            watch_project_buffers(project, recording_sink(recorded.clone(), false), cx)
        });
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer("/root/notes.txt", cx)
            })
            .await
            .unwrap();
        recorded.take();

        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "x")], None, cx));
        cx.executor().advance_clock(CHANGE_DEBOUNCE * 2);
        cx.run_until_parked();
        assert_eq!(recorded.take(), vec![]);
    }

    #[gpui::test]
    async fn test_prompt_delegate_answers(cx: &mut TestAppContext) {
        init_test(cx);
        let items = vec![
            PickItem {
                id: "a".into(),
                label: "Apple".into(),
                detail: None,
            },
            PickItem {
                id: "b".into(),
                label: "Banana".into(),
                detail: Some("yellow".into()),
            },
        ];

        let answer = |mut delegate: ExtensionPromptDelegate,
                      act: fn(
            &mut Picker<ExtensionPromptDelegate>,
            &mut Window,
            &mut Context<Picker<ExtensionPromptDelegate>>,
        ),
                      cx: &mut TestAppContext| {
            let (sender, receiver) = oneshot::channel();
            delegate.answer = Some(sender);
            let window = cx.add_window(|window, cx| Picker::uniform_list(delegate, window, cx));
            window
                .update(cx, |picker, window, cx| act(picker, window, cx))
                .unwrap();
            cx.run_until_parked();
            window
                .update(cx, |_, window, _| window.remove_window())
                .unwrap();
            cx.run_until_parked();
            receiver
        };

        let receiver = answer(
            ExtensionPromptDelegate::pick("Fruit".into(), items.clone()),
            |picker, window, cx| {
                picker.delegate.set_selected_index(1, window, cx);
                picker.delegate.confirm(false, window, cx);
            },
            cx,
        );
        assert_eq!(receiver.await.unwrap(), Some("b".into()));

        let receiver = answer(
            ExtensionPromptDelegate::input("Name".into()),
            |picker, window, cx| {
                picker
                    .delegate
                    .update_matches("Ada".into(), window, cx)
                    .detach();
                picker.delegate.confirm(false, window, cx);
            },
            cx,
        );
        assert_eq!(receiver.await.unwrap(), Some("Ada".into()));

        let receiver = answer(
            ExtensionPromptDelegate::pick("Fruit".into(), items),
            |picker, window, cx| picker.delegate.dismissed(window, cx),
            cx,
        );
        assert_eq!(receiver.await.unwrap(), None);

        let receiver = answer(
            ExtensionPromptDelegate::input("Name".into()),
            |_, _, _| {},
            cx,
        );
        assert_eq!(
            receiver.await.unwrap_or(None),
            None,
            "closing the prompt without answering yields None"
        );
    }

    #[gpui::test]
    async fn test_status_texts(cx: &mut TestAppContext) {
        init_test(cx);
        let status_texts = cx.new(|_| ExtensionStatusTexts::default());
        let views = cx.new(|_| ExtensionViews::default());
        let ui = LatheUi {
            status_texts: status_texts.downgrade(),
            views: views.downgrade(),
        };
        let mut async_cx = cx.to_async();
        ui.set_status_text("one".into(), Some("12 words".into()), None, &mut async_cx);
        ui.set_status_text(
            "two".into(),
            Some("ok".into()),
            Some("tip".into()),
            &mut async_cx,
        );
        ui.set_status_text("one".into(), None, None, &mut async_cx);
        status_texts.read_with(cx, |status_texts, _| {
            assert_eq!(
                status_texts.texts.iter().collect::<Vec<_>>(),
                vec![(
                    &Arc::<str>::from("two"),
                    &StatusEntry::Text {
                        text: "ok".into(),
                        tooltip: Some("tip".into()),
                    }
                )]
            );
        });

        let item = LatheStatusItem {
            label: "3 TODOs".into(),
            icon: Some("check".into()),
            tooltip: None,
            tone: extension_host::wasm_host::lathe_views::Tone::Warning,
        };
        ui.set_status_item("two".into(), Some(item.clone()), &mut async_cx);
        status_texts.read_with(cx, |status_texts, _| {
            assert_eq!(
                status_texts.texts.get("two"),
                Some(&StatusEntry::Item(item.clone())),
                "a status item replaces the extension's status text"
            );
        });
        ui.set_status_item("two".into(), None, &mut async_cx);
        status_texts.read_with(cx, |status_texts, _| assert!(status_texts.texts.is_empty()));
    }

    #[gpui::test]
    fn test_view_shown_and_hidden(cx: &mut TestAppContext) {
        let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let views = cx.new(|_| {
            ExtensionViews::with_surface_sink({
                let events = events.clone();
                Arc::new(move |extension_id, event, _| {
                    events.lock().push((extension_id.to_string(), event))
                })
            })
        });
        let attach = |cx: &mut TestAppContext| {
            views.update(cx, |views, cx| {
                views.attach("ext".into(), "main".into(), cx)
            })
        };
        let detach = |cx: &mut TestAppContext| {
            views.update(cx, |views, cx| {
                views.detach("ext".into(), "main".into(), cx)
            })
        };
        attach(cx);
        attach(cx);
        detach(cx);
        assert_eq!(
            std::mem::take(&mut *events.lock()),
            vec![(
                "ext".to_string(),
                LatheSurfaceEvent::ViewShown("main".into())
            )],
            "only the first surface showing a view counts, and one is still open"
        );
        detach(cx);
        detach(cx);
        assert_eq!(
            std::mem::take(&mut *events.lock()),
            vec![(
                "ext".to_string(),
                LatheSurfaceEvent::ViewHidden("main".into())
            )],
            "hidden once, when the last surface closes; extra detaches are ignored"
        );
    }
}

#[cfg(test)]
mod task_tests {
    use super::*;
    use crate::lathe_editor::ExtensionBufferDelegate;
    use extension_host::wasm_host::lathe::LatheEditorDelegate as _;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::{Path, PathBuf};

    #[test]
    fn test_spawn_in_terminal_conversion() {
        let spawn = spawn_in_terminal(
            TaskSpec {
                label: "Run On Save: cargo fmt".into(),
                command: "cargo".into(),
                args: vec!["fmt".into(), "--".into(), "src/main.rs".into()],
                cwd: None,
                env: vec![("RUST_LOG".into(), "info".into())],
                reveal: TaskReveal::Never,
            },
            Some(PathBuf::from("/project")),
        );
        assert_eq!(spawn.label, "Run On Save: cargo fmt");
        assert_eq!(spawn.full_label, "Run On Save: cargo fmt");
        assert_eq!(spawn.command.as_deref(), Some("cargo"));
        assert_eq!(spawn.command_label, "cargo fmt -- src/main.rs");
        assert_eq!(spawn.cwd.as_deref(), Some(Path::new("/project")));
        assert_eq!(spawn.env.get("RUST_LOG").map(String::as_str), Some("info"));
        assert_eq!(spawn.reveal, task::RevealStrategy::Never);
        assert!(
            !spawn.allow_concurrent_runs,
            "a rerun waits for the previous one"
        );
        assert!(!spawn.use_new_terminal, "a rerun reuses the terminal");
        assert_eq!(
            spawn.id,
            spawn_in_terminal(
                TaskSpec {
                    label: "Run On Save: cargo fmt".into(),
                    command: "other".into(),
                    args: vec![],
                    cwd: None,
                    env: vec![],
                    reveal: TaskReveal::Always,
                },
                None,
            )
            .id,
            "the same label maps to the same task id"
        );
    }

    #[gpui::test]
    async fn test_snapshot_file_location(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/work/app", json!({ "src": { "main.rs": "fn main() {}" } }))
            .await;
        let project = Project::test(fs.clone(), [Path::new("/work/app")], cx).await;
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer("/work/app/src/main.rs", cx)
            })
            .await
            .unwrap();

        let delegate =
            ExtensionBufferDelegate::new(buffer.downgrade(), ExtensionEditedBuffers::default());
        let snapshot = delegate.snapshot(&mut cx.to_async()).unwrap().unwrap();
        assert_eq!(snapshot.path.as_deref(), Some("src/main.rs"));
        assert_eq!(snapshot.abs_path.as_deref(), Some("/work/app/src/main.rs"));
        assert_eq!(snapshot.worktree_root.as_deref(), Some("/work/app"));

        let scratch = cx.new(|cx| Buffer::local("scratch", cx));
        let delegate =
            ExtensionBufferDelegate::new(scratch.downgrade(), ExtensionEditedBuffers::default());
        let snapshot = delegate.snapshot(&mut cx.to_async()).unwrap().unwrap();
        assert_eq!(
            (snapshot.path, snapshot.abs_path, snapshot.worktree_root),
            (None, None, None)
        );
    }
}

#[cfg(test)]
mod settings_trust_tests {
    use super::*;
    use crate::lathe_editor::ExtensionBufferDelegate;
    use collections::HashSet;
    use extension_host::extension_settings::ExtensionSettings;
    use extension_host::wasm_host::lathe::LatheEditorDelegate as _;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use project::trusted_worktrees::{self, DbTrustedPaths, PathTrust, TrustedWorktrees};
    use serde_json::json;
    use settings::{Settings as _, SettingsLocation, SettingsStore};
    use std::path::Path;

    fn run_on_save_settings(
        delegate: &ExtensionBufferDelegate,
        cx: &mut TestAppContext,
    ) -> Option<serde_json::Value> {
        let location = delegate.settings_location(&mut cx.to_async());
        cx.update(|cx| {
            let location = location
                .as_ref()
                .map(|(worktree_id, path)| SettingsLocation {
                    worktree_id: *worktree_id,
                    path: path.as_ref(),
                });
            ExtensionSettings::get(location, cx)
                .extension_settings
                .get("run-on-save")
                .cloned()
        })
    }

    #[gpui::test]
    async fn test_project_extension_settings_need_trust(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            ExtensionSettings::register(cx);
            trusted_worktrees::init(DbTrustedPaths::default(), cx);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |content| {
                    content
                        .project
                        .extension_settings
                        .insert("run-on-save".into(), json!({ "rules": ["from user"] }));
                });
            });
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            json!({
                ".zed": {
                    "settings.json": r#"{ "extension_settings": { "run-on-save": { "rules": ["from project"] } } }"#,
                },
                "notes.md": "hello",
            }),
        )
        .await;
        let project =
            Project::test_with_worktree_trust(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer("/project/notes.md", cx)
            })
            .await
            .unwrap();
        let delegate =
            ExtensionBufferDelegate::new(buffer.downgrade(), ExtensionEditedBuffers::default());

        assert_eq!(
            run_on_save_settings(&delegate, cx),
            Some(json!({ "rules": ["from user"] })),
            "an untrusted project's settings file is not applied"
        );

        let worktree_store = project.read_with(cx, |project, _| project.worktree_store());
        let worktree_id = worktree_store.read_with(cx, |store, cx| {
            store.worktrees().next().unwrap().read(cx).id()
        });
        let trusted = cx.update(|cx| TrustedWorktrees::try_get_global(cx).unwrap());
        trusted.update(cx, |trusted, cx| {
            trusted.trust(
                &worktree_store,
                HashSet::from_iter([PathTrust::Worktree(worktree_id)]),
                cx,
            )
        });
        cx.run_until_parked();

        assert_eq!(
            run_on_save_settings(&delegate, cx),
            Some(json!({ "rules": ["from project"] })),
            "once trusted, the project's value overrides the user's for its files"
        );

        let scratch = cx.new(|cx| Buffer::local("scratch", cx));
        let scratch_delegate =
            ExtensionBufferDelegate::new(scratch.downgrade(), ExtensionEditedBuffers::default());
        assert_eq!(
            run_on_save_settings(&scratch_delegate, cx),
            Some(json!({ "rules": ["from user"] })),
            "buffers outside the project only see the user's settings"
        );
    }
}
