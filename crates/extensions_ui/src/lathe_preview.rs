//! File previews from Lathe extensions. An extension registers a preview for
//! file name endings; "<Extension>: <Preview>" in the command palette, or the
//! button in the editor's toolbar, opens it in a tab beside the file's
//! editor, the way Markdown previews open. The text editor stays available,
//! since a preview can't edit the file.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context as _, Result};
use editor::Editor;
use extension_host::ExtensionStore;
use extension_host::wasm_host::lathe::{LatheEditorDelegate, LathePreview, LatheSurfaceEvent};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Global, IntoElement, Render,
    SharedString, Subscription, Task, WeakEntity, Window,
};
use language::{Buffer, BufferEvent};
use project::{Project, ProjectPath};
use schemars::JsonSchema;
use serde::Deserialize;
use ui::{Icon, IconName, Label, prelude::*, v_flex};
use util::ResultExt as _;
use workspace::item::{Item, ItemEvent, SerializableItem};
use workspace::{ItemId, Workspace, WorkspaceId};

use crate::lathe_editor::{ExtensionBufferDelegate, ExtensionEditedBuffers, file_location};
use crate::lathe_ui::{CHANGE_DEBOUNCE, GlobalExtensionViews};
use crate::lathe_view_renderer::ExtensionViewHost;

/// Opens a preview an extension registered, beside the active file's editor.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema, gpui::Action)]
#[action(namespace = extension)]
#[serde(deny_unknown_fields)]
pub struct OpenPreview {
    /// The id of the extension that registered the preview.
    pub extension_id: String,
    /// The preview's id within that extension.
    pub preview: String,
}

/// A preview an extension registered, with the extension's name for titles.
#[derive(Clone, Debug)]
pub struct AvailablePreview {
    pub extension_id: Arc<str>,
    pub extension_name: SharedString,
    pub preview: LathePreview,
}

impl AvailablePreview {
    /// "<Extension>: <Preview>", as shown in the palette and the toolbar.
    pub fn label(&self) -> SharedString {
        format!("{}: {}", self.extension_name, self.preview.title).into()
    }

    pub fn action(&self) -> OpenPreview {
        OpenPreview {
            extension_id: self.extension_id.to_string(),
            preview: self.preview.id.to_string(),
        }
    }
}

type SendSurfaceEvent = Arc<
    dyn Fn(
        Arc<str>,
        LatheSurfaceEvent,
        Option<Arc<dyn LatheEditorDelegate>>,
        &mut App,
    ) -> Task<Result<()>>,
>;

/// Calls back whenever the set of available previews may have changed.
type ObserveAvailability = Arc<dyn Fn(Box<dyn Fn(&mut App)>, &mut App) -> Option<Subscription>>;

/// What previews need from the extension store. Tests swap in their own.
#[derive(Clone)]
pub struct PreviewBackend {
    pub previews: Arc<dyn Fn(&App) -> Vec<AvailablePreview>>,
    pub send: SendSurfaceEvent,
    pub observe: ObserveAvailability,
    pub edited_by_extensions: ExtensionEditedBuffers,
}

impl Global for PreviewBackend {}

pub(crate) fn init(edited_by_extensions: ExtensionEditedBuffers, cx: &mut App) {
    cx.set_global(PreviewBackend {
        previews: Arc::new(|cx| {
            let Some(store) = ExtensionStore::try_global(cx) else {
                return Vec::new();
            };
            store
                .read(cx)
                .lathe_previews()
                .map(|(manifest, preview)| AvailablePreview {
                    extension_id: manifest.id.clone(),
                    extension_name: manifest.name.clone().into(),
                    preview: preview.clone(),
                })
                .collect()
        }),
        send: Arc::new(|extension_id, event, buffer, cx| {
            let Some(store) = ExtensionStore::try_global(cx) else {
                return Task::ready(Err(anyhow::anyhow!("extensions are not available")));
            };
            store.update(cx, |store, cx| {
                store.dispatch_lathe_surface_event(&extension_id, event, buffer, cx)
            })
        }),
        observe: Arc::new(|callback, cx| {
            let store = ExtensionStore::try_global(cx)?;
            Some(cx.observe(&store, move |_, cx| callback(cx)))
        }),
        edited_by_extensions,
    });
    register(cx);
}

/// Registers the palette entries, the workspace action and restoring open
/// previews. Separate from [`init`] so tests can install their own backend.
pub(crate) fn register(cx: &mut App) {
    command_palette_hooks::register_extra_commands(cx, |cx| {
        let Some(backend) = cx.try_global::<PreviewBackend>() else {
            return Vec::new();
        };
        (backend.previews)(cx)
            .into_iter()
            .map(|available| command_palette_hooks::ExtraCommand {
                name: available.label(),
                action: Box::new(available.action()),
            })
            .collect()
    });
    command_palette_hooks::CommandPaletteFilter::update_global(cx, |filter, _| {
        filter.hide_action_types(&[std::any::TypeId::of::<OpenPreview>()]);
    });
    workspace::register_serializable_item::<ExtensionPreview>(cx);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(open_preview);
    })
    .detach();
}

/// The previews that can open the file `editor` shows, for the toolbar.
pub fn matching_previews(editor: &Entity<Editor>, cx: &App) -> Vec<AvailablePreview> {
    let Some(backend) = cx.try_global::<PreviewBackend>() else {
        return Vec::new();
    };
    let Some(file_name) = editor_file_name(editor, cx) else {
        return Vec::new();
    };
    (backend.previews)(cx)
        .into_iter()
        .filter(|available| available.preview.matches(&file_name))
        .collect()
}

fn editor_file_name(editor: &Entity<Editor>, cx: &App) -> Option<String> {
    let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
    let buffer = buffer.read(cx);
    buffer.file().map(|file| file.file_name(cx).to_string())
}

fn open_preview(
    workspace: &mut Workspace,
    action: &OpenPreview,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Err(error) = try_open_preview(workspace, action, window, cx) {
        workspace.show_error(format!("{error:#}"), cx);
    }
}

fn try_open_preview(
    workspace: &mut Workspace,
    action: &OpenPreview,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Result<()> {
    let backend = cx
        .try_global::<PreviewBackend>()
        .cloned()
        .context("previews are not available")?;
    let available = (backend.previews)(cx)
        .into_iter()
        .find(|available| {
            available.extension_id.as_ref() == action.extension_id
                && available.preview.id.as_ref() == action.preview
        })
        .with_context(|| {
            format!(
                "{}'s preview {:?} isn't available: the extension isn't loaded, or isn't \
                 allowed to show views",
                action.extension_id, action.preview
            )
        })?;
    let editor = workspace
        .active_item_as::<Editor>(cx)
        .context("open a file to preview first")?;
    let buffer = editor
        .read(cx)
        .buffer()
        .read(cx)
        .as_singleton()
        .context("previews work on a single file")?;
    let file_name = editor_file_name(&editor, cx).context("save the file before previewing it")?;
    if !available.preview.matches(&file_name) {
        anyhow::bail!(
            "{} opens files ending in {}",
            available.preview.title,
            available
                .preview
                .file_suffixes
                .iter()
                .map(|suffix| suffix.as_ref())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let existing = workspace.panes().iter().find_map(|pane| {
        pane.read(cx)
            .items_of_type::<ExtensionPreview>()
            .find(|preview| {
                let preview = preview.read(cx);
                preview.extension_id == available.extension_id
                    && preview.preview_id == available.preview.id
                    && preview.buffer == buffer
            })
            .map(|preview| (pane.clone(), preview))
    });
    if let Some((pane, preview)) = existing {
        pane.update(cx, |pane, cx| {
            if let Some(index) = pane.index_for_item(&preview) {
                pane.activate_item(index, true, false, window, cx);
            }
        });
        return Ok(());
    }

    let title = format!("{}: {file_name}", available.preview.title).into();
    let preview = cx.new(|cx| {
        ExtensionPreview::new(
            available.extension_id,
            available.preview.id,
            title,
            buffer,
            backend,
            window,
            cx,
        )
    });
    let origin = workspace.active_pane().clone();
    let target = workspace.adjacent_pane_of(&origin, window, cx);
    target.update(cx, |pane, cx| {
        pane.add_item(Box::new(preview), true, false, None, window, cx)
    });
    editor.focus_handle(cx).focus(window, cx);
    Ok(())
}

static NEXT_PREVIEW: AtomicUsize = AtomicUsize::new(1);

enum PreviewState {
    /// The extension hasn't loaded yet (or isn't allowed to show views), as
    /// happens when Lathe restores a preview at startup.
    Waiting,
    Opening,
    Ready(Entity<ExtensionViewHost>),
    Failed(SharedString),
}

/// A tab showing an extension's view of one file.
pub struct ExtensionPreview {
    extension_id: Arc<str>,
    preview_id: Arc<str>,
    view_id: Arc<str>,
    title: SharedString,
    /// Held strongly: after a restart, or once its editor is closed, the
    /// preview may be the only thing keeping the file open.
    buffer: Entity<Buffer>,
    backend: PreviewBackend,
    state: PreviewState,
    focus_handle: FocusHandle,
    pending_change: Option<Task<()>>,
    _open: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ExtensionPreview {
    fn new(
        extension_id: Arc<str>,
        preview_id: Arc<str>,
        title: SharedString,
        buffer: Entity<Buffer>,
        backend: PreviewBackend,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let view_id: Arc<str> =
            format!("preview-{}", NEXT_PREVIEW.fetch_add(1, Ordering::Relaxed)).into();
        let mut subscriptions = vec![cx.subscribe(&buffer, |this, buffer, event, cx| {
            if matches!(event, BufferEvent::Edited { .. } | BufferEvent::Reloaded) {
                this.schedule_change(buffer, cx);
            }
        })];
        let this = cx.weak_entity();
        let window_handle = window.window_handle();
        subscriptions.extend((backend.observe)(
            Box::new(move |cx| {
                window_handle
                    .update(cx, |_, window, cx| {
                        this.update(cx, |this, cx| this.open_when_available(window, cx))
                    })
                    .ok();
            }),
            cx,
        ));
        cx.on_release({
            let send = backend.send.clone();
            let extension_id = extension_id.clone();
            let view_id = view_id.clone();
            move |this, cx| {
                // Only an extension that heard `preview-opened` hears this.
                if matches!(this.state, PreviewState::Waiting) {
                    return;
                }
                if let Some(views) = cx
                    .try_global::<GlobalExtensionViews>()
                    .map(|views| views.0.clone())
                {
                    views.update(cx, |views, cx| {
                        if views.remove(extension_id.clone(), view_id.clone()) {
                            cx.notify();
                        }
                    });
                }
                let closed = send(
                    extension_id,
                    LatheSurfaceEvent::PreviewClosed(view_id),
                    None,
                    cx,
                );
                cx.background_spawn(async move { closed.await.log_err() })
                    .detach();
            }
        })
        .detach();
        let mut this = Self {
            extension_id,
            preview_id,
            view_id,
            title,
            buffer,
            backend,
            state: PreviewState::Waiting,
            focus_handle: cx.focus_handle(),
            pending_change: None,
            _open: None,
            _subscriptions: subscriptions,
        };
        this.open_when_available(window, cx);
        this
    }

    fn buffer_delegate(&self) -> Arc<dyn LatheEditorDelegate> {
        Arc::new(ExtensionBufferDelegate::new(
            self.buffer.downgrade(),
            self.backend.edited_by_extensions.clone(),
        ))
    }

    /// Sends `preview-opened` once the extension is loaded and allowed to
    /// show views, then shows its view.
    fn open_when_available(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.state, PreviewState::Waiting) {
            return;
        }
        let available = (self.backend.previews)(cx).into_iter().any(|available| {
            available.extension_id == self.extension_id && available.preview.id == self.preview_id
        });
        if !available {
            return;
        }
        let location = file_location(self.buffer.read(cx), cx);
        let opened = (self.backend.send)(
            self.extension_id.clone(),
            LatheSurfaceEvent::PreviewOpened {
                preview_id: self.preview_id.clone(),
                view_id: self.view_id.clone(),
                path: location.path,
                abs_path: location.abs_path,
            },
            Some(self.buffer_delegate()),
            cx,
        );
        self.state = PreviewState::Opening;
        cx.notify();
        // The view host is created only once the extension has heard about
        // the preview, so it never gets `view-shown` for a view id it hasn't
        // been told about.
        let extension_id = self.extension_id.clone();
        let view_id = self.view_id.clone();
        self._open = Some(cx.spawn_in(window, async move |this, cx| {
            let result = opened.await;
            this.update_in(cx, |this, window, cx| {
                let views = cx
                    .try_global::<GlobalExtensionViews>()
                    .map(|views| views.0.clone());
                this.state = match (result, views) {
                    (Ok(()), Some(views)) => PreviewState::Ready(cx.new(|cx| {
                        ExtensionViewHost::for_extension(extension_id, view_id, views, window, cx)
                    })),
                    (Ok(()), None) => {
                        PreviewState::Failed("extension views are not available".into())
                    }
                    (Err(error), _) => PreviewState::Failed(format!("{error:#}").into()),
                };
                cx.notify();
            })
            .log_err();
        }));
    }

    /// Tells the extension the file changed, once edits pause. Changes the
    /// extension made itself are skipped, so redrawing can't loop.
    fn schedule_change(&mut self, buffer: Entity<Buffer>, cx: &mut Context<Self>) {
        if !matches!(self.state, PreviewState::Ready(_)) {
            return;
        }
        self.pending_change = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CHANGE_DEBOUNCE).await;
            this.update(cx, |this, cx| {
                this.pending_change = None;
                if this
                    .backend
                    .edited_by_extensions
                    .is_unchanged_since_extension_edit(buffer.read(cx))
                {
                    return;
                }
                let changed = (this.backend.send)(
                    this.extension_id.clone(),
                    LatheSurfaceEvent::PreviewChanged(this.view_id.clone()),
                    Some(this.buffer_delegate()),
                    cx,
                );
                cx.background_spawn(async move { changed.await.log_err() })
                    .detach();
            })
            .log_err();
        }));
    }
}

impl EventEmitter<ItemEvent> for ExtensionPreview {}

impl Focusable for ExtensionPreview {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ExtensionPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.state {
            PreviewState::Waiting => Label::new(format!(
                "Waiting for the {} extension to load…",
                self.extension_id
            ))
            .color(Color::Muted)
            .into_any_element(),
            PreviewState::Opening => Label::new("Opening preview…")
                .color(Color::Muted)
                .into_any_element(),
            PreviewState::Ready(host) => host.clone().into_any_element(),
            PreviewState::Failed(error) => v_flex()
                .gap_1()
                .child(Label::new("The extension couldn't open this preview.").color(Color::Error))
                .child(Label::new(error.clone()).color(Color::Muted))
                .into_any_element(),
        };
        v_flex()
            .key_context("ExtensionPreview")
            .track_focus(&self.focus_handle)
            .size_full()
            .p_2()
            .bg(cx.theme().colors().editor_background)
            .child(content)
    }
}

impl Item for ExtensionPreview {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Eye))
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl SerializableItem for ExtensionPreview {
    fn serialized_item_kind() -> &'static str {
        "LatheExtensionPreview"
    }

    fn deserialize(
        project: Entity<Project>,
        _workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let db = persistence::LathePreviewDb::global(cx);
        let backend = cx.try_global::<PreviewBackend>().cloned();
        window.spawn(cx, async move |cx| {
            let backend = backend.context("previews are not available")?;
            let (abs_path, extension_id, preview_id, title) = db
                .get_preview(item_id, workspace_id)?
                .context("no saved extension preview")?;
            let (worktree, path) = project
                .update(cx, |project, cx| {
                    project.find_or_create_worktree(abs_path, false, cx)
                })
                .await?;
            let project_path = ProjectPath {
                worktree_id: worktree.read_with(cx, |worktree, _| worktree.id()),
                path,
            };
            let buffer = project
                .update(cx, |project, cx| project.open_buffer(project_path, cx))
                .await?;
            cx.update(|window, cx| {
                cx.new(|cx| {
                    ExtensionPreview::new(
                        extension_id.into(),
                        preview_id.into(),
                        title.into(),
                        buffer,
                        backend,
                        window,
                        cx,
                    )
                })
            })
        })
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let db = persistence::LathePreviewDb::global(cx);
        workspace::delete_unloaded_items(
            alive_items,
            workspace_id,
            "lathe_extension_previews",
            &db,
            cx,
        )
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let file = self.buffer.read(cx).file()?;
        let abs_path: PathBuf = workspace
            .project()
            .read(cx)
            .worktree_for_id(file.worktree_id(cx), cx)?
            .read(cx)
            .absolutize(file.path());
        let db = persistence::LathePreviewDb::global(cx);
        let extension_id = self.extension_id.to_string();
        let preview_id = self.preview_id.to_string();
        let title = self.title.to_string();
        Some(cx.background_spawn(async move {
            db.save_preview(
                item_id,
                workspace_id,
                abs_path,
                extension_id,
                preview_id,
                title,
            )
            .await
        }))
    }

    fn should_serialize(&self, _event: &Self::Event) -> bool {
        false
    }
}

mod persistence {
    use std::path::PathBuf;

    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use workspace::{ItemId, WorkspaceDb, WorkspaceId};

    pub struct LathePreviewDb(ThreadSafeConnection);

    impl Domain for LathePreviewDb {
        const NAME: &str = stringify!(LathePreviewDb);

        const MIGRATIONS: &[&str] = &[sql!(
            CREATE TABLE lathe_extension_previews (
                workspace_id INTEGER,
                item_id INTEGER,
                abs_path BLOB,
                extension_id TEXT NOT NULL,
                preview_id TEXT NOT NULL,
                title TEXT NOT NULL,

                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
            ) STRICT;
        )];
    }

    db::static_connection!(LathePreviewDb, [WorkspaceDb]);

    impl LathePreviewDb {
        query! {
            pub async fn save_preview(
                item_id: ItemId,
                workspace_id: WorkspaceId,
                abs_path: PathBuf,
                extension_id: String,
                preview_id: String,
                title: String
            ) -> Result<()> {
                INSERT OR REPLACE INTO lathe_extension_previews(
                    item_id, workspace_id, abs_path, extension_id, preview_id, title
                )
                VALUES (?, ?, ?, ?, ?, ?)
            }
        }

        query! {
            pub fn get_preview(
                item_id: ItemId,
                workspace_id: WorkspaceId
            ) -> Result<Option<(PathBuf, String, String, String)>> {
                SELECT abs_path, extension_id, preview_id, title
                FROM lathe_extension_previews
                WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lathe_ui::ExtensionViews;
    use gpui::{TestAppContext, VisualTestContext};
    use parking_lot::Mutex;
    use project::Project;
    use serde_json::json;
    use std::sync::atomic::AtomicBool;
    use util::path;
    use util::rel_path::rel_path;
    use workspace::{AppState, SaveIntent};

    /// Events the fake extension received, with the text `active_buffer()`
    /// would have returned.
    type Received = Arc<Mutex<Vec<(LatheSurfaceEvent, Option<String>)>>>;

    /// Whether the fake extension is loaded, and an entity to notify when
    /// that changes (the way the extension store notifies).
    #[derive(Clone)]
    struct Availability {
        loaded: Arc<AtomicBool>,
        notifier: Entity<()>,
    }

    impl Availability {
        fn set_loaded(&self, cx: &mut VisualTestContext) {
            self.loaded.store(true, Ordering::SeqCst);
            self.notifier.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
        }
    }

    fn init_test(
        fail_opens: Arc<AtomicBool>,
        cx: &mut TestAppContext,
    ) -> (Arc<AppState>, Received) {
        let (app_state, received, _) = init_test_with(fail_opens, true, cx);
        (app_state, received)
    }

    fn init_test_with(
        fail_opens: Arc<AtomicBool>,
        loaded: bool,
        cx: &mut TestAppContext,
    ) -> (Arc<AppState>, Received, Availability) {
        let received = Received::default();
        let availability = cx.update(|cx| Availability {
            loaded: Arc::new(AtomicBool::new(loaded)),
            notifier: cx.new(|_| ()),
        });
        let app_state = cx.update(|cx| {
            let app_state = AppState::test(cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
            let views = cx.new(|_| ExtensionViews::default());
            cx.set_global(GlobalExtensionViews(views));
            let loaded = availability.loaded.clone();
            let notifier = availability.notifier.clone();
            cx.set_global(PreviewBackend {
                previews: Arc::new(move |_| {
                    if !loaded.load(Ordering::SeqCst) {
                        return Vec::new();
                    }
                    vec![AvailablePreview {
                        extension_id: "tables".into(),
                        extension_name: "Tables".into(),
                        preview: LathePreview {
                            id: "csv".into(),
                            title: "CSV Preview".into(),
                            file_suffixes: Arc::from([Arc::<str>::from(".csv")]),
                        },
                    }]
                }),
                send: Arc::new({
                    let received = received.clone();
                    move |extension_id, event, buffer, cx| {
                        assert_eq!(&*extension_id, "tables");
                        let received = received.clone();
                        let fail = matches!(event, LatheSurfaceEvent::PreviewOpened { .. })
                            && fail_opens.load(Ordering::SeqCst);
                        cx.spawn(async move |cx| {
                            let text = match buffer {
                                Some(buffer) => buffer.snapshot(cx)?.map(|snapshot| snapshot.text),
                                None => None,
                            };
                            received.lock().push((event, text));
                            if fail {
                                anyhow::bail!("the extension trapped");
                            }
                            Ok(())
                        })
                    }
                }),
                observe: Arc::new(move |callback, cx| {
                    Some(cx.observe(&notifier, move |_, cx| callback(cx)))
                }),
                edited_by_extensions: ExtensionEditedBuffers::default(),
            });
            register(cx);
            app_state
        });
        (app_state, received, availability)
    }

    async fn open_workspace<'a>(
        app_state: &Arc<AppState>,
        cx: &'a mut TestAppContext,
    ) -> (Entity<Workspace>, &'a mut VisualTestContext) {
        app_state
            .fs
            .as_fake()
            .insert_tree(
                path!("/root"),
                json!({ "data.csv": "a,b\n1,2\n3,4\n", "notes.json": "{}" }),
            )
            .await;
        let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.add_window_view(|window, cx| {
            Workspace::new(None, project, app_state.clone(), window, cx)
        })
    }

    async fn open_file(workspace: &Entity<Workspace>, name: &str, cx: &mut VisualTestContext) {
        let worktree_id = workspace.read_with(cx, |workspace, cx| {
            workspace
                .project()
                .read(cx)
                .worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx).id())
                .expect("a worktree")
        });
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_path((worktree_id, rel_path(name)), None, true, window, cx)
            })
            .await
            .expect("opening the file");
        cx.run_until_parked();
    }

    fn previews(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Vec<Entity<ExtensionPreview>> {
        workspace.read_with(cx, |workspace, cx| {
            workspace
                .panes()
                .iter()
                .flat_map(|pane| {
                    pane.read(cx)
                        .items_of_type::<ExtensionPreview>()
                        .collect::<Vec<_>>()
                })
                .collect()
        })
    }

    /// Runs the `extension::OpenPreview` handler the way the workspace does.
    fn open(workspace: &Entity<Workspace>, action: OpenPreview, cx: &mut VisualTestContext) {
        workspace.update_in(cx, |workspace, window, cx| {
            open_preview(workspace, &action, window, cx)
        });
        cx.run_until_parked();
    }

    fn open_preview_action() -> OpenPreview {
        OpenPreview {
            extension_id: "tables".into(),
            preview: "csv".into(),
        }
    }

    #[gpui::test]
    async fn test_open_and_close_preview(cx: &mut TestAppContext) {
        let (app_state, received) = init_test(Arc::default(), cx);
        let (workspace, cx) = open_workspace(&app_state, cx).await;
        open_file(&workspace, "data.csv", cx).await;

        let editor = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<Editor>(cx)
                .expect("the csv editor")
        });
        open(&workspace, open_preview_action(), cx);

        let opened = previews(&workspace, cx);
        assert_eq!(opened.len(), 1);
        let (title, ready) = opened[0].read_with(cx, |preview, cx| {
            (
                preview.tab_content_text(0, cx),
                matches!(preview.state, PreviewState::Ready(_)),
            )
        });
        let view_id = match received.lock().first() {
            Some((LatheSurfaceEvent::PreviewOpened { view_id, .. }, _)) => view_id.clone(),
            other => panic!("expected preview-opened first, got {other:?}"),
        };
        assert_eq!(title.as_ref(), "CSV Preview: data.csv");
        assert!(
            ready,
            "the view host is created once the extension answered"
        );
        assert_eq!(
            std::mem::take(&mut *received.lock()),
            vec![(
                LatheSurfaceEvent::PreviewOpened {
                    preview_id: "csv".into(),
                    view_id: view_id.clone(),
                    path: Some("data.csv".into()),
                    abs_path: Some(path!("/root/data.csv").into()),
                },
                Some("a,b\n1,2\n3,4\n".to_string()),
            )],
            "the extension can read the previewed file"
        );
        assert_eq!(
            workspace.read_with(cx, |workspace, _| workspace.panes().len()),
            2,
            "the preview opens beside the editor"
        );
        let editor_focused =
            cx.update(|window, cx| editor.focus_handle(cx).contains_focused(window, cx));
        assert!(editor_focused, "focus stays in the editor");

        open(&workspace, open_preview_action(), cx);
        assert_eq!(
            previews(&workspace, cx).len(),
            1,
            "opening it again reuses the tab"
        );
        assert!(received.lock().is_empty());

        let preview_pane = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panes()
                .iter()
                .find(|pane| {
                    pane.read(cx)
                        .items_of_type::<ExtensionPreview>()
                        .next()
                        .is_some()
                })
                .cloned()
                .expect("the preview's pane")
        });
        let preview_id = opened[0].entity_id();
        drop(opened);
        preview_pane
            .update_in(cx, |pane, window, cx| {
                pane.close_item_by_id(preview_id, SaveIntent::Skip, window, cx)
            })
            .await
            .unwrap();
        // The workspace holds items it's about to serialize until its
        // throttle fires; the preview is released after that.
        cx.executor()
            .advance_clock(workspace::SERIALIZATION_THROTTLE_TIME * 2);
        cx.run_until_parked();
        assert!(previews(&workspace, cx).is_empty());
        assert_eq!(
            std::mem::take(&mut *received.lock()),
            vec![(LatheSurfaceEvent::PreviewClosed(view_id), None)]
        );
    }

    #[gpui::test]
    async fn test_preview_needs_a_matching_file(cx: &mut TestAppContext) {
        let (app_state, received) = init_test(Arc::default(), cx);
        let (workspace, cx) = open_workspace(&app_state, cx).await;
        open_file(&workspace, "notes.json", cx).await;

        open(&workspace, open_preview_action(), cx);
        assert!(previews(&workspace, cx).is_empty());
        assert!(received.lock().is_empty(), "the extension isn't asked");

        open(
            &workspace,
            OpenPreview {
                extension_id: "tables".into(),
                preview: "missing".into(),
            },
            cx,
        );
        assert!(previews(&workspace, cx).is_empty());
    }

    #[gpui::test]
    async fn test_failed_open_shows_the_error(cx: &mut TestAppContext) {
        let (app_state, _received) = init_test(Arc::new(AtomicBool::new(true)), cx);
        let (workspace, cx) = open_workspace(&app_state, cx).await;
        open_file(&workspace, "data.csv", cx).await;

        open(&workspace, open_preview_action(), cx);
        let opened = previews(&workspace, cx);
        assert_eq!(opened.len(), 1);
        let error = opened[0].read_with(cx, |preview, _| match &preview.state {
            PreviewState::Failed(error) => error.to_string(),
            _ => String::new(),
        });
        assert!(error.contains("the extension trapped"), "{error:?}");
    }

    fn buffer_of(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<Buffer> {
        workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<Editor>(cx)
                .and_then(|editor| editor.read(cx).buffer().read(cx).as_singleton())
                .expect("the file's buffer")
        })
    }

    #[gpui::test]
    async fn test_changes_reach_the_extension(cx: &mut TestAppContext) {
        let (app_state, received) = init_test(Arc::default(), cx);
        let (workspace, cx) = open_workspace(&app_state, cx).await;
        open_file(&workspace, "data.csv", cx).await;
        let buffer = buffer_of(&workspace, cx);
        open(&workspace, open_preview_action(), cx);
        let view_id = match received.lock().first() {
            Some((LatheSurfaceEvent::PreviewOpened { view_id, .. }, _)) => view_id.clone(),
            other => panic!("expected preview-opened, got {other:?}"),
        };
        received.lock().clear();

        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "x")], None, cx));
        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "y")], None, cx));
        cx.run_until_parked();
        assert!(received.lock().is_empty(), "nothing until typing pauses");
        cx.executor().advance_clock(CHANGE_DEBOUNCE);
        cx.run_until_parked();
        assert_eq!(
            std::mem::take(&mut *received.lock()),
            vec![(
                LatheSurfaceEvent::PreviewChanged(view_id),
                Some("yxa,b\n1,2\n3,4\n".to_string())
            )],
            "one event after the pause, with the new text readable"
        );

        // An edit the extension made itself doesn't come back as a change.
        let backend = cx.update(|_, cx| cx.global::<PreviewBackend>().clone());
        let delegate =
            ExtensionBufferDelegate::new(buffer.downgrade(), backend.edited_by_extensions);
        let mut async_cx = cx.to_async();
        let snapshot = delegate.snapshot(&mut async_cx).unwrap().unwrap();
        delegate
            .edit(snapshot.version, vec![(0..0, "!".into())], &mut async_cx)
            .unwrap();
        cx.executor().advance_clock(CHANGE_DEBOUNCE);
        cx.run_until_parked();
        assert!(received.lock().is_empty(), "{:?}", received.lock());
    }

    #[gpui::test]
    async fn test_waits_for_the_extension(cx: &mut TestAppContext) {
        let (app_state, received, availability) = init_test_with(Arc::default(), false, cx);
        let (workspace, cx) = open_workspace(&app_state, cx).await;
        open_file(&workspace, "data.csv", cx).await;
        let buffer = buffer_of(&workspace, cx);
        let backend = cx.update(|_, cx| cx.global::<PreviewBackend>().clone());

        // What restoring at startup does: create the tab before the
        // extension has loaded.
        let preview = cx.update(|window, cx| {
            cx.new(|cx| {
                ExtensionPreview::new(
                    "tables".into(),
                    "csv".into(),
                    "CSV Preview: data.csv".into(),
                    buffer.clone(),
                    backend,
                    window,
                    cx,
                )
            })
        });
        cx.run_until_parked();
        assert!(preview.read_with(cx, |preview, _| matches!(
            preview.state,
            PreviewState::Waiting
        )));
        assert!(received.lock().is_empty());

        availability.set_loaded(cx);
        assert!(
            preview.read_with(cx, |preview, _| matches!(
                preview.state,
                PreviewState::Ready(_)
            )),
            "it opens once the extension loads"
        );
        assert!(matches!(
            received.lock().first(),
            Some((LatheSurfaceEvent::PreviewOpened { .. }, Some(_)))
        ));
    }

    #[gpui::test]
    async fn test_save_and_restore(cx: &mut TestAppContext) {
        let (app_state, _received) = init_test(Arc::default(), cx);
        let (workspace, cx) = open_workspace(&app_state, cx).await;
        let db = cx.update(|_, cx| workspace::WorkspaceDb::global(cx));
        let workspace_id = db.next_id().await.unwrap();
        let preview_db = cx.update(|_, cx| persistence::LathePreviewDb::global(cx));
        let item_id: ItemId = 42;
        preview_db
            .save_preview(
                item_id,
                workspace_id,
                PathBuf::from(path!("/root/data.csv")),
                "tables".into(),
                "csv".into(),
                "CSV Preview: data.csv".into(),
            )
            .await
            .unwrap();

        let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
        let restored = cx
            .update(|window, cx| {
                ExtensionPreview::deserialize(
                    project.clone(),
                    workspace.downgrade(),
                    workspace_id,
                    item_id,
                    window,
                    cx,
                )
            })
            .await
            .expect("restoring the preview");
        cx.run_until_parked();
        restored.read_with(cx, |preview, cx| {
            assert_eq!(
                preview.tab_content_text(0, cx).as_ref(),
                "CSV Preview: data.csv"
            );
            assert_eq!(preview.buffer.read(cx).text(), "a,b\n1,2\n3,4\n");
            assert!(matches!(preview.state, PreviewState::Ready(_)));
        });

        let missing = cx
            .update(|window, cx| {
                ExtensionPreview::deserialize(
                    project,
                    workspace.downgrade(),
                    workspace_id,
                    7,
                    window,
                    cx,
                )
            })
            .await;
        assert!(missing.is_err(), "nothing saved under that item id");
    }

    #[gpui::test]
    async fn test_toolbar_matches_by_file_name(cx: &mut TestAppContext) {
        let (app_state, _received) = init_test(Arc::default(), cx);
        let (workspace, cx) = open_workspace(&app_state, cx).await;
        let matching = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                let editor = workspace.active_item_as::<Editor>(cx).expect("an editor");
                matching_previews(&editor, cx)
                    .into_iter()
                    .map(|available| available.label().to_string())
                    .collect::<Vec<_>>()
            })
        };
        open_file(&workspace, "data.csv", cx).await;
        assert_eq!(
            matching(&workspace, cx),
            vec!["Tables: CSV Preview".to_string()]
        );
        open_file(&workspace, "notes.json", cx).await;
        assert!(matching(&workspace, cx).is_empty());
    }
}
