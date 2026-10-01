
use std::future::Future;
use std::ops::Range;
use std::{path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result};
use collections::HashMap;
use buffer_diff::BufferDiff;
use editor::SelectionEffects;
use editor::scroll::Autoscroll;
use editor::{Editor, EditorMode, MultiBuffer};
use feature_flags::{FeatureFlagAppExt as _, NotebookFeatureFlag};
use futures::FutureExt;
use futures::future::Shared;
use gpui::{
    AnyElement, App, Entity, EventEmitter, FocusHandle, Focusable, Global, KeyContext, ListState, Point, Task, list, prelude::*,
};
use jupyter_protocol::JupyterKernelspec;
use gpui::{Font, Subscription};
use language::{HighlightedText, Language, LanguageRegistry};
use log;
use project::{Project, ProjectEntryId, ProjectPath};
use settings::{Settings as _, SettingsStore};
use ui::{KeyBinding, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::ToolbarItemLocation;
use workspace::item::{SaveOptions, TabContentParams};
use workspace::searchable::SearchableItemHandle;
use workspace::{Item, Pane, ProjectItem};

use super::{
    Cell, CellEvent, CellPosition, CellReconciliation, MarkdownCellEvent, NotebookOutline,
    OutlineEntry, RenderableCell, ShadowBuffer, projection_of_notebook, reconcile_cells,
};

use nbformat::v4::{CellId, CellType};
use serde_json;
use uuid::Uuid;

use crate::JupyterSettings;
use crate::components::{KernelPicker, KernelSelector};
use crate::kernels::{
    Kernel, KernelSession, KernelSpecification, KernelStatus, LocalKernelSpecification,
    NativeRunningKernel, RemoteRunningKernel, SshRunningKernel, WslRunningKernel,
};
use crate::notebook::MovementDirection;
use crate::repl_store::ReplStore;

use runtimelib::{ExecuteRequest, JupyterMessage, JupyterMessageContent};
use ui::PopoverMenuHandle;
use zed_actions::editor::{MoveDown, MoveUp};
use zed_actions::notebook::{
    AddCodeBlock, AddMarkdownBlock, ClearOutputs, DeleteCell, EnterCommandMode, EnterEditMode,
    InterruptKernel, MoveCellDown, MoveCellUp, NotebookMoveDown, NotebookMoveUp, OpenNotebook,
    RestartKernel, Run, RunAll, RunAndAdvance, ToggleSourceView,
};

/// Which view of the notebook is on screen.
///
/// Both views edit the same document: the cell view through per-cell excerpts, the
/// source view through one editor over the whole shared buffer. That is why switching
/// needs no synchronization in the common case, and why there is only ever one copy of
/// the text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NotebookViewMode {
    Cells,
    Source,
}

/// Whether the notebook is in command mode (navigating cells) or edit mode (editing a cell).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NotebookMode {
    Command,
    Edit,
}

#[derive(PartialEq, Eq)]
enum SelectionMode {
    SelectOnly,
    SelectAndMove,
}

pub(crate) const MEDIUM_SPACING_SIZE: f32 = 12.0;
pub(crate) const GUTTER_WIDTH: f32 = 19.0;
pub(crate) const CODE_BLOCK_INSET: f32 = MEDIUM_SPACING_SIZE;
pub(crate) const CONTROL_SIZE: f32 = 20.0;

const NOTEBOOK_EXTENSION: &str = "ipynb";

struct NotebookEditorRegistered;

impl Global for NotebookEditorRegistered {}

fn register_notebook_editor(cx: &mut App) {
    if cx.has_global::<NotebookEditorRegistered>() {
        return;
    }

    workspace::register_project_item::<NotebookEditor>(cx);
    cx.set_global(NotebookEditorRegistered);
}

fn notebook_editor_enabled(cx: &App) -> bool {
    JupyterSettings::notebook_enabled(cx)
        || cx.has_flag::<NotebookFeatureFlag>()
        || std::env::var("LOCAL_NOTEBOOK_DEV").is_ok()
}

pub fn init(cx: &mut App) {
    if notebook_editor_enabled(cx) {
        register_notebook_editor(cx);
    }

    cx.observe_global::<SettingsStore>(|cx| {
        if notebook_editor_enabled(cx) {
            register_notebook_editor(cx);
        }
    })
    .detach();

    cx.observe_flag::<NotebookFeatureFlag, _>({
        move |flag, cx| {
            if *flag {
                register_notebook_editor(cx);
            } else {
                // todo: there is no way to unregister a project item, so if the feature flag
                // gets turned off they need to restart Zed.
            }
        }
    })
    .detach();
}

pub struct NotebookEditor {
    languages: Arc<LanguageRegistry>,
    project: Entity<Project>,
    worktree_id: project::WorktreeId,
    focus_handle: FocusHandle,
    notebook_item: Entity<NotebookItem>,
    notebook_language: Shared<Task<Option<Arc<Language>>>>,
    cell_list: ListState,
    notebook_mode: NotebookMode,
    selected_cell_index: usize,
    cell_order: Vec<CellId>,
    original_cell_order: Vec<CellId>,
    cell_map: HashMap<CellId, Cell>,
    /// Every code cell's source as one project-registered buffer. Cell editors are
    /// excerpts over it, which is what gives them a project and puts all code cells in
    /// one scope for a language server.
    shadow: ShadowBuffer,
    notebook_diff: Option<Entity<BufferDiff>>,
    /// The committed notebook, projected. Held so edits can be re-diffed against it
    /// without going back to git each time.
    notebook_diff_base: Option<String>,
    _diff_task: Task<()>,
    _diff_subscription: Option<Subscription>,
    view_mode: NotebookViewMode,
    /// Built on first use, then kept so the source view holds its cursor across toggles.
    source_editor: Option<Entity<Editor>>,
    _kernel_launch_task: Task<()>,
    kernel_discovery: Shared<Task<()>>,
    /// Incremented per launch so a superseded attempt cannot clobber a live kernel.
    kernel_launch_generation: usize,
    nav_history: Option<workspace::ItemNavHistory>,
    /// Set once the sidecar file exists, so it can be cleaned up on close.
    sidecar_path: Option<ProjectPath>,
    _shared_language_task: Task<()>,
    kernel: Kernel,
    kernel_specification: Option<KernelSpecification>,
    execution_requests: HashMap<String, CellId>,
    /// Cells asked to run before the kernel was ready. Opening a notebook and pressing
    /// run immediately is the common first action, and the kernel takes seconds to
    /// start, so refusing would fail almost every first attempt.
    pending_executions: Vec<CellId>,
    kernel_picker_handle: PopoverMenuHandle<KernelPicker>,
}

enum SaveDestination {
    CurrentPath,
    NewPath(ProjectPath),
}

/// Reads the committed contents of a notebook, via the diff Zed already maintains for
/// the file itself.
async fn load_committed_notebook(
    project: &Entity<Project>,
    notebook_path: ProjectPath,
    cx: &mut gpui::AsyncApp,
) -> Option<String> {
    let buffer = project
        .update(cx, |project, cx| project.open_buffer(notebook_path, cx))
        .await
        .ok()?;

    let file_diff = project
        .update(cx, |project, cx| project.open_uncommitted_diff(buffer, cx))
        .await
        .ok()?;

    // The diff resolves before its base text is necessarily populated, so a single read
    // often comes back empty and the notebook silently ends up with no diff at all.
    for attempt in 0..20 {
        if let Some(base) = file_diff.read_with(cx, |diff, cx| {
            let snapshot = diff.snapshot(cx);
            snapshot.base_text_exists().then(|| snapshot.base_text_string())?
        }) {
            log::debug!("notebook: committed text available after {attempt} attempts");
            return Some(base);
        }
        cx.background_executor()
            .timer(std::time::Duration::from_millis(100))
            .await;
    }

    log::warn!("notebook: gave up waiting for the committed notebook; no diff will show");
    None
}

/// Replaces a cell's source, leaving its id, metadata and outputs alone.
fn set_cell_source(cell: &mut nbformat::v4::Cell, source: &str) {
    let lines = source
        .split_inclusive('\n')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    match cell {
        nbformat::v4::Cell::Code { source, .. }
        | nbformat::v4::Cell::Markdown { source, .. }
        | nbformat::v4::Cell::Raw { source, .. } => *source = lines,
    }
}

fn new_nbformat_cell(cell_type: CellType, source: &str) -> nbformat::v4::Cell {
    let id: CellId = Uuid::new_v4().into();
    let metadata: nbformat::v4::CellMetadata =
        serde_json::from_str("{}").expect("empty object should parse");
    let source = source
        .split_inclusive('\n')
        .map(str::to_owned)
        .collect::<Vec<_>>();

    match cell_type {
        CellType::Code => nbformat::v4::Cell::Code {
            id,
            metadata,
            execution_count: None,
            source,
            outputs: Vec::new(),
        },
        CellType::Markdown => nbformat::v4::Cell::Markdown {
            id,
            metadata,
            source,
            attachments: None,
        },
        CellType::Raw => nbformat::v4::Cell::Raw {
            id,
            metadata,
            source,
        },
    }
}

/// Summarizes a cell in one line for the breadcrumb and the outline.
///
/// Markdown headings win, because that is how a notebook is actually organized;
/// otherwise the first definition, otherwise the first non-empty line.
fn cell_label(source: &str) -> Option<String> {
    const MAX: usize = 60;

    let truncate = |text: &str| {
        let text = text.trim();
        if text.chars().count() > MAX {
            let cut: String = text.chars().take(MAX - 1).collect();
            format!("{}\u{2026}", cut.trim_end())
        } else {
            text.to_string()
        }
    };

    // The first heading that actually carries text. A bare `#` is an empty heading and
    // must not win over the cell's real content.
    if let Some(heading) = source.lines().find_map(|line| {
        let rest = line.trim().strip_prefix('#')?.trim_start_matches('#').trim();
        (!rest.is_empty()).then(|| rest.to_string())
    }) {
        return Some(truncate(&heading));
    }

    if let Some(definition) = source.lines().map(str::trim).find(|line| {
        line.starts_with("def ")
            || line.starts_with("class ")
            || line.starts_with("async def ")
    }) {
        return Some(truncate(definition.trim_end_matches(':')));
    }

    source
        .lines()
        .map(str::trim)
        .find(|line| !line.trim_matches('#').trim().is_empty())
        .map(truncate)
}

/// A position in a notebook for the workspace's go-back and go-forward.
///
/// Stores the cell id rather than its index, so an entry still points at the right cell
/// after cells are added, removed or reordered.
struct NotebookNavigationData {
    cell_id: CellId,
}

/// Serializes a notebook in the same shape as the file it came from.
///
/// Two things force this. `jupyter-protocol` and `nbformat` route several structures
/// through `HashMap`, and Rust randomizes `HashMap` iteration per process, so the key
/// order of MIME bundles and metadata changes on every run. And `to_string_pretty`
/// writes two-space indent with no trailing newline, while Jupyter writes one space with
/// one.
///
/// Left alone, opening a notebook and saving it rewrites the entire file. `template`,
/// the JSON as it was read from disk, restores the original key order; the writer
/// restores the original layout. Saving an unchanged notebook then produces no diff.
pub fn serialize_notebook(
    notebook: &nbformat::v4::Notebook,
    template: Option<&serde_json::Value>,
) -> Result<String> {
    let mut value = serde_json::to_value(notebook).context("Failed to serialize notebook")?;

    match template {
        Some(template) => apply_key_order(&mut value, template),
        // A notebook with no file behind it still needs a stable order, or its own
        // saves churn against each other.
        None => sort_json_keys(&mut value),
    }

    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
    serde::Serialize::serialize(&value, &mut serializer)
        .context("Failed to write notebook JSON")?;
    let mut json = String::from_utf8(out).context("notebook JSON should be valid UTF-8")?;
    json.push('\n');
    Ok(json)
}

/// Puts every object key in a fixed order, for a notebook with no file to match.
fn sort_json_keys(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(String, serde_json::Value)> =
                std::mem::take(map).into_iter().collect();
            entries.sort_by(|(a, _), (b, _)| a.cmp(b));
            for (_, entry) in entries.iter_mut() {
                sort_json_keys(entry);
            }
            *map = entries.into_iter().collect();
        }
        serde_json::Value::Array(items) => {
            for item in items {
                sort_json_keys(item);
            }
        }
        _ => {}
    }
}

/// Reorders `value`'s object keys to match `template`, recursively.
///
/// Keys the template does not have keep their existing relative order and follow the
/// ones it does, so a newly added field lands at the end rather than reshuffling the
/// file. Arrays line up by index, which is what makes an edited cell diff on its own
/// rather than dragging every later cell with it.
fn apply_key_order(value: &mut serde_json::Value, template: &serde_json::Value) {
    match (value, template) {
        (serde_json::Value::Object(map), serde_json::Value::Object(template_map)) => {
            let mut taken = std::mem::take(map);
            let mut ordered = serde_json::Map::new();

            for key in template_map.keys() {
                if let Some(mut entry) = taken.shift_remove(key) {
                    if let Some(template_entry) = template_map.get(key) {
                        apply_key_order(&mut entry, template_entry);
                    }
                    ordered.insert(key.clone(), entry);
                }
            }
            for (key, entry) in taken {
                // Parsing a partial structure and re-serializing it fills the absent
                // optional fields with nulls, which would add lines the original never
                // had. A null the file did not carry says nothing, so it is dropped.
                if entry.is_null() {
                    continue;
                }
                ordered.insert(key, entry);
            }
            *map = ordered;
        }
        (serde_json::Value::Array(items), serde_json::Value::Array(template_items)) => {
            for (item, template_item) in items.iter_mut().zip(template_items) {
                apply_key_order(item, template_item);
            }
        }
        _ => {}
    }
}

/// Drops a cell's outputs and execution count.
///
/// `.ipynb` stores outputs inline, so a committed notebook carries image data and
/// execution results that make diffs unreadable. Stripping on save is what keeps a
/// notebook reviewable in a pull request.
fn strip_outputs(cell: nbformat::v4::Cell) -> nbformat::v4::Cell {
    match cell {
        nbformat::v4::Cell::Code {
            id,
            metadata,
            source,
            ..
        } => nbformat::v4::Cell::Code {
            id,
            metadata,
            execution_count: None,
            source,
            outputs: Vec::new(),
        },
        other => other,
    }
}

impl NotebookEditor {
    pub fn new(
        project: Entity<Project>,
        notebook_item: Entity<NotebookItem>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();

        let languages = project.read(cx).languages().clone();
        let _language_name = notebook_item.read(cx).language_name();
        let worktree_id = notebook_item.read(cx).project_path.worktree_id;

        let notebook_language = notebook_item.read(cx).notebook_language();
        let notebook_language = cx
            .spawn_in(window, async move |_, _| notebook_language.await)
            .shared();

        let shared_language_task = cx.spawn_in(window, {
            let notebook_language = notebook_language.clone();
            async move |this, cx| {
                let language = notebook_language.await;
                this.update(cx, |this, cx| this.shadow.set_language(language, cx))
                    .ok();
            }
        });

        let mut cell_order = vec![]; // Vec<CellId>
        let mut cell_map = HashMap::default(); // HashMap<CellId, Cell>

        // Built before the cells, because each code cell's editor is an excerpt over
        // it. The language arrives asynchronously and is applied below.
        let notebook_cells = notebook_item.read(cx).notebook.cells.clone();
        let shadow = ShadowBuffer::new(&project, &notebook_cells, None, "#", cx);

        let cell_count = notebook_cells.len();
        for index in 0..cell_count {
            let cell = notebook_cells[index].clone();
            let cell_id = cell.id();
            cell_order.push(cell_id.clone());
            let backing = shadow.backing_for(cell_id, &project, cx);
            let cell_entity =
                Cell::load(&cell, &languages, backing, notebook_language.clone(), window, cx);

            match &cell_entity {
                Cell::Code(code_cell) => {
                    let cell_id_for_focus = cell_id.clone();
                    cx.subscribe_in(code_cell, window, move |this, _cell, event, window, cx| {
                        match event {
                            CellEvent::Run(cell_id) => {
                                this.execute_cell(cell_id.clone(), window, cx)
                            }
                            CellEvent::FocusedIn(_) => {
                                this.select_cell_by_id(&cell_id_for_focus, cx)
                            }
                        }
                    })
                    .detach();

                    let cell_id_for_editor = cell_id.clone();
                    let editor = code_cell.read(cx).editor().clone();
                    cx.subscribe(&editor, move |this, _editor, event, cx| {
                        if let editor::EditorEvent::Focused = event {
                            this.select_cell_by_id(&cell_id_for_editor, cx);
                        }
                    })
                    .detach();
                }
                Cell::Markdown(markdown_cell) => {
                    cx.subscribe(
                        markdown_cell,
                        move |_this, cell, event: &MarkdownCellEvent, cx| {
                            match event {
                                MarkdownCellEvent::FinishedEditing => {
                                    cell.update(cx, |cell, cx| {
                                        cell.reparse_markdown(cx);
                                    });
                                }
                                MarkdownCellEvent::Run(_cell_id) => {
                                    // run is handled separately by move_to_next_cell
                                    // Just reparse here
                                    cell.update(cx, |cell, cx| {
                                        cell.reparse_markdown(cx);
                                    });
                                }
                            }
                        },
                    )
                    .detach();

                    let cell_id_for_editor = cell_id.clone();
                    let editor = markdown_cell.read(cx).editor().clone();
                    cx.subscribe(&editor, move |this, _editor, event, cx| {
                        if let editor::EditorEvent::Focused = event {
                            this.select_cell_by_id(&cell_id_for_editor, cx);
                        }
                    })
                    .detach();
                }
                Cell::Raw(_) => {}
            }

            cell_map.insert(cell_id.clone(), cell_entity);
        }

        let _notebook_handle = cx.entity().downgrade();
        let cell_count = cell_order.len();

        let _this = cx.entity();
        let cell_list = ListState::new(cell_count, gpui::ListAlignment::Top, px(1000.));

        let mut editor = Self {
            project,
            languages: languages.clone(),
            worktree_id,
            focus_handle,
            notebook_item: notebook_item.clone(),
            notebook_language,
            cell_list,
            notebook_mode: NotebookMode::Command,
            selected_cell_index: 0,
            cell_order: cell_order.clone(),
            original_cell_order: cell_order.clone(),
            cell_map: cell_map.clone(),
            kernel: Kernel::Shutdown,
            kernel_specification: None,
            execution_requests: HashMap::default(),
            pending_executions: Vec::new(),
            kernel_picker_handle: PopoverMenuHandle::default(),
            shadow,
            notebook_diff: None,
            notebook_diff_base: None,
            _diff_task: Task::ready(()),
            _diff_subscription: None,
            view_mode: NotebookViewMode::Cells,
            source_editor: None,
            _kernel_launch_task: Task::ready(()),
            kernel_discovery: Task::ready(()).shared(),
            kernel_launch_generation: 0,
            nav_history: None,
            sidecar_path: None,
            _shared_language_task: shared_language_task,
        };
        // Registered here rather than in `Drop`, which has no `App` to delete through.
        cx.on_release(|this, cx| {
            let Some(sidecar_path) = this.sidecar_path.take() else {
                return;
            };
            this.project.update(cx, |project, cx| {
                if let Some(task) = project.delete_file(sidecar_path, cx) {
                    task.detach();
                }
            });
        })
        .detach();

        editor.refresh_notebook_diff(cx);
        editor.start_language_server_sidecar(window, cx);
        editor.refresh_kernelspecs(cx);
        editor.refresh_language(cx);
        editor.launch_kernel(window, cx);

        cx.subscribe(&notebook_item, |this, _item, _event, cx| {
            this.refresh_language(cx);
        })
        .detach();

        // The file can change underneath us: discarding in the git panel, a rebase, or
        // another tool writing the notebook. `Item::reload` already rebuilds every cell,
        // it just had nothing telling it when to run.
        let notebook_buffer = notebook_item.read(cx).buffer.clone();
        cx.subscribe_in(
            &notebook_buffer,
            window,
            |this, _buffer, event, window, cx| {
                if matches!(event, language::BufferEvent::Reloaded) {
                    let project = this.project.clone();
                    this.reload(project, window, cx).detach_and_log_err(cx);
                }
            },
        )
        .detach();

        editor
    }

    /// Requests kernel discovery for this notebook.
    ///
    /// Called when the editor is created rather than while rendering the status bar,
    /// which is where it used to happen.
    fn refresh_kernelspecs(&mut self, cx: &mut Context<Self>) {
        ReplStore::global(cx).update(cx, |store, cx| store.ensure_kernelspecs(cx));

        let store = ReplStore::global(cx);
        let project = self.project.clone();
        let worktree_id = self.worktree_id;

        let refresh_task = store.update(cx, |store, cx| {
            store.refresh_python_kernelspecs(worktree_id, &project, cx)
        });

        self.kernel_discovery = cx
            .background_spawn(async move {
                refresh_task.await.log_err();
            })
            .shared();
    }

    fn refresh_language(&mut self, cx: &mut Context<Self>) {
        let notebook_language = self.notebook_item.read(cx).notebook_language();
        let task = cx.spawn(async move |this, cx| {
            let language = notebook_language.await;
            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| {
                    for cell in this.cell_map.values() {
                        if let Cell::Code(code_cell) = cell {
                            code_cell.update(cx, |cell, cx| {
                                cell.set_language(language.clone(), cx);
                            });
                        }
                    }
                });
            }
            language
        });
        self.notebook_language = task.shared();
    }

    fn has_structural_changes(&self) -> bool {
        self.cell_order != self.original_cell_order
    }

    fn has_content_changes(&self, cx: &App) -> bool {
        self.cell_map.values().any(|cell| cell.is_dirty(cx))
    }

    pub fn to_notebook(&self, cx: &App) -> nbformat::v4::Notebook {
        let clear_outputs = JupyterSettings::get_global(cx).clear_outputs_on_save;

        let cells: Vec<nbformat::v4::Cell> = self
            .cell_order
            .iter()
            .filter_map(|cell_id| {
                self.cell_map
                    .get(cell_id)
                    .map(|cell| cell.to_nbformat_cell(cx))
            })
            .map(|cell| {
                if clear_outputs {
                    strip_outputs(cell)
                } else {
                    cell
                }
            })
            .collect();

        let metadata = self.notebook_item.read(cx).notebook.metadata.clone();

        nbformat::v4::Notebook {
            metadata,
            nbformat: 4,
            nbformat_minor: 5,
            cells,
        }
    }

    pub fn mark_as_saved(&mut self, cx: &mut Context<Self>) {
        self.original_cell_order = self.cell_order.clone();

        // Code cells are excerpts over the shared buffer, so their dirty state lives
        // on that one buffer rather than on a buffer per cell. Without this they stay
        // dirty forever, because the per-cell path below only reaches singletons.
        self.shadow.mark_saved(cx);

        for cell in self.cell_map.values() {
            match cell {
                Cell::Code(code_cell) => {
                    code_cell.update(cx, |code_cell, cx| {
                        let editor = code_cell.editor();
                        editor.update(cx, |editor, cx| {
                            editor.buffer().update(cx, |buffer, cx| {
                                if let Some(buf) = buffer.as_singleton() {
                                    buf.update(cx, |b, cx| {
                                        let version = b.version();
                                        b.did_save(version, None, cx);
                                    });
                                }
                            });
                        });
                    });
                }
                Cell::Markdown(markdown_cell) => {
                    markdown_cell.update(cx, |markdown_cell, cx| {
                        let editor = markdown_cell.editor();
                        editor.update(cx, |editor, cx| {
                            editor.buffer().update(cx, |buffer, cx| {
                                if let Some(buf) = buffer.as_singleton() {
                                    buf.update(cx, |b, cx| {
                                        let version = b.version();
                                        b.did_save(version, None, cx);
                                    });
                                }
                            });
                        });
                    });
                }
                Cell::Raw(_) => {}
            }
        }
        cx.notify();
    }

    fn save_impl(
        &mut self,
        destination: SaveDestination,
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let notebook = self.to_notebook(cx);
        let template = self.notebook_item.read(cx).original_json.clone();
        let project_path = self.notebook_item.read(cx).project_path.clone();

        self.mark_as_saved(cx);

        cx.spawn(async move |this, cx| {
            let json = serialize_notebook(&notebook, template.as_ref())?;
            let buffer = project
                .update(cx, |project, cx| project.open_buffer(project_path, cx))
                .await?;
            buffer.update(cx, |buffer, cx| buffer.set_text(json, cx));

            match destination {
                SaveDestination::CurrentPath => {
                    project
                        .update(cx, |project, cx| project.save_buffer(buffer, cx))
                        .await
                }
                SaveDestination::NewPath(new_path) => {
                    project
                        .update(cx, |project, cx| {
                            project.save_buffer_as(buffer, new_path.clone(), cx)
                        })
                        .await?;

                    // The buffer now lives at the new path, so the notebook has
                    // to follow it or the next save writes to the old file.
                    let entry_id = project.read_with(cx, |project, cx| {
                        project.entry_for_path(&new_path, cx).map(|entry| entry.id)
                    });
                    this.update(cx, |this, cx| {
                        this.notebook_item.update(cx, |notebook_item, _| {
                            notebook_item.project_path = new_path;
                            if let Some(entry_id) = entry_id {
                                notebook_item.id = entry_id;
                            }
                        })
                    })
                }
            }
        })
    }

    /// Waits for the notebook's language before choosing a kernel.
    ///
    /// The language resolves asynchronously, so reading it during construction always
    /// yields `None`, the store's lookup then matches nothing, and the hardcoded
    /// fallback runs bare `python3`, which on most machines is not the interpreter the
    /// notebook needs and may not have ipykernel at all.
    fn launch_kernel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let notebook_language = self.notebook_language.clone();
        let kernel_discovery = self.kernel_discovery.clone();
        self._kernel_launch_task = cx.spawn_in(window, async move |this, cx| {
            let language = notebook_language.await;

            // Discovery shells out to probe interpreters. Choosing before it finishes
            // only sees the global kernels, so a project venv loses to a bare `python3`.
            futures::select_biased! {
                _ = kernel_discovery.fuse() => {}
                _ = cx
                    .background_executor()
                    .timer(std::time::Duration::from_secs(10))
                    .fuse() => {}
            }

            this.update_in(cx, |this, window, cx| {
                this.launch_kernel_for_language(language, window, cx);
            })
            .log_err();
        });
    }

    fn launch_kernel_for_language(
        &mut self,
        language: Option<Arc<Language>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {

        let spec = self.kernel_specification.clone().or_else(|| {
            ReplStore::global(cx)
                .read(cx)
                .active_kernelspec(self.worktree_id, language, cx)
        });

        let spec = spec.unwrap_or_else(|| {
            KernelSpecification::Jupyter(LocalKernelSpecification {
                name: "python3".to_string(),
                path: PathBuf::from("python3"),
                kernelspec: JupyterKernelspec {
                    argv: vec![
                        "python3".to_string(),
                        "-m".to_string(),
                        "ipykernel_launcher".to_string(),
                        "-f".to_string(),
                        "{connection_file}".to_string(),
                    ],
                    display_name: "Python 3".to_string(),
                    language: "python".to_string(),
                    interrupt_mode: None,
                    metadata: None,
                    env: None,
                },
            })
        });

        // A Python environment without ipykernel cannot host a kernel. Launching anyway
        // spawns a process that dies during the handshake, which the notebook can only
        // report as a timeout, so say what is actually wrong and how to fix it.
        if !spec.has_ipykernel() {
            self.kernel = Kernel::ErroredLaunch(format!(
                "this environment has no ipykernel, so no kernel can start.\n\n\
                 install it with:\n  {} -m pip install ipykernel\n\n\
                 then pick the environment again from the kernel selector.",
                spec.path()
            ));
            cx.notify();
            return;
        }
        self.launch_kernel_with_spec(spec, window, cx);
    }

    fn launch_kernel_with_spec(
        &mut self,
        spec: KernelSpecification,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entity_id = cx.entity_id();
        let working_directory = self
            .project
            .read(cx)
            .worktree_for_id(self.worktree_id, cx)
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            .unwrap_or_else(std::env::temp_dir);
        let fs = self.project.read(cx).fs().clone();
        let view = cx.entity();

        self.kernel_specification = Some(spec.clone());

        let kernel_task = match spec {
            KernelSpecification::Jupyter(local_spec) => NativeRunningKernel::new(
                local_spec,
                entity_id,
                working_directory,
                fs,
                view,
                window,
                cx,
            ),
            KernelSpecification::PythonEnv(env_spec) => NativeRunningKernel::new(
                env_spec.as_local_spec(),
                entity_id,
                working_directory,
                fs,
                view,
                window,
                cx,
            ),
            KernelSpecification::JupyterServer(remote_spec) => {
                RemoteRunningKernel::new(remote_spec, working_directory, view, window, cx)
            }

            KernelSpecification::SshRemote(spec) => {
                let project = self.project.clone();
                SshRunningKernel::new(spec, working_directory, project, view, window, cx)
            }
            KernelSpecification::WslRemote(spec) => {
                WslRunningKernel::new(spec, entity_id, working_directory, fs, view, window, cx)
            }
        };

        self.kernel_launch_generation += 1;
        let launch_generation = self.kernel_launch_generation;

        let pending_kernel = cx
            .spawn(async move |this, cx| {
                // A kernel whose process dies on startup, or whose interpreter lacks
                // ipykernel, never completes the connection handshake. Without a bound
                // the notebook sits on a spinner forever with no way to find out why.
                // A slow kernel is not a failed one. Returning here used to drop
                // `kernel_task`, but the process it spawned was already live and talking
                // to this editor, so the kernel came up and ran cells while the editor
                // had declared the launch dead and stamped errors on those same cells.
                // The short timer only warns; only the long one gives up.
                let mut kernel_task = kernel_task.fuse();
                let mut slow_warning = cx
                    .background_executor()
                    .timer(std::time::Duration::from_secs(45))
                    .fuse();
                let mut give_up = cx
                    .background_executor()
                    .timer(std::time::Duration::from_secs(300))
                    .fuse();

                let kernel = loop {
                    futures::select_biased! {
                        kernel = kernel_task => break kernel,
                        _ = slow_warning => {
                            log::warn!(
                                "kernel: still starting after 45 seconds; \
                                 check that the interpreter has ipykernel installed"
                            );
                        }
                        _ = give_up => {
                            // Kept to short explicit lines: this is rendered through a
                            // fixed-width terminal emulator, which hard-wraps mid-word.
                            break Err(anyhow::anyhow!(
                                "the kernel did not start within 5 minutes.\n\
                                 check that the interpreter has ipykernel installed.\n\
                                 the kernel log button in the status bar has details."
                            ));
                        }
                    }
                };

                match kernel {
                    Ok(kernel) => {
                        this.update_in(cx, |editor, window, cx| {
                            // A launch that has been superseded must not take over, or it
                            // replaces the kernel the user is actually talking to.
                            if editor.kernel_launch_generation != launch_generation {
                                return;
                            }
                            editor.kernel = Kernel::RunningKernel(kernel);
                            editor.flush_pending_executions(window, cx);
                            cx.notify();
                        })
                        .ok();
                    }
                    Err(err) => {
                        log::error!("Kernel failed to start: {:?}", err);
                        this.update_in(cx, |editor, window, cx| {
                            // Same guard, and it matters more here: a stale attempt timing
                            // out used to mark a running kernel as failed and stamp
                            // "could not be executed" onto cells that had already run.
                            if editor.kernel_launch_generation != launch_generation {
                                return;
                            }
                            editor.kernel = Kernel::ErroredLaunch(err.to_string());
                            // Anything queued while the kernel was starting has to be
                            // told, or it spins on "Running..." forever. Queueing a run
                            // is only an improvement over refusing it if the failure
                            // still reaches the cell.
                            editor.fail_pending_executions(&err.to_string(), window, cx);
                            cx.notify();
                        })
                        .ok();
                    }
                }
            })
            .shared();

        self.kernel = Kernel::StartingKernel(pending_kernel);
        cx.notify();
    }

    // Note: Python environments are only detected as kernels if ipykernel is installed.
    // Users need to run `pip install ipykernel` (or `uv pip install ipykernel`) in their
    // virtual environment for it to appear in the kernel selector.
    // This happens because we have an ipykernel check inside the function python_env_kernel_specification in mod.rs L:121

    fn change_kernel(
        &mut self,
        spec: KernelSpecification,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Kernel::RunningKernel(kernel) = &mut self.kernel {
            kernel.force_shutdown(window, cx).detach();
        }

        self.execution_requests.clear();

        // A Python environment without ipykernel cannot host a kernel. Launching anyway
        // spawns a process that dies during the handshake, which the notebook can only
        // report as a timeout, so say what is actually wrong and how to fix it.
        if !spec.has_ipykernel() {
            self.kernel = Kernel::ErroredLaunch(format!(
                "this environment has no ipykernel, so no kernel can start.\n\n\
                 install it with:\n  {} -m pip install ipykernel\n\n\
                 then pick the environment again from the kernel selector.",
                spec.path()
            ));
            cx.notify();
            return;
        }
        self.record_kernel_in_metadata(&spec, cx);
        self.launch_kernel_with_spec(spec, window, cx);
    }

    /// Writes the chosen kernel into the notebook's metadata.
    ///
    /// Only called for an explicit choice. Doing it on every launch meant the automatic
    /// selection at open rewrote `kernelspec`, so merely opening a notebook dirtied it
    /// and autosave turned that into a diff against a file the user never edited.
    fn record_kernel_in_metadata(&mut self, spec: &KernelSpecification, cx: &mut Context<Self>) {
        let kernel_name = spec.name().to_string();
        let language = spec.language().to_string();
        let display_name = match spec {
            KernelSpecification::Jupyter(s) => s.kernelspec.display_name.clone(),
            KernelSpecification::PythonEnv(s) => s.kernelspec.display_name.clone(),
            KernelSpecification::JupyterServer(s) => s.kernelspec.display_name.clone(),
            KernelSpecification::SshRemote(s) => s.kernelspec.display_name.clone(),
            KernelSpecification::WslRemote(s) => s.kernelspec.display_name.clone(),
        };

        let kernelspec_json = serde_json::json!({
            "display_name": display_name,
            "name": kernel_name,
            "language": language
        });

        self.notebook_item.update(cx, |item, cx| {
            if let Ok(kernelspec) = serde_json::from_value(kernelspec_json) {
                item.notebook.metadata.kernelspec = Some(kernelspec);
                cx.emit(());
            }
        });
    }

    fn restart_kernel(&mut self, _: &RestartKernel, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(spec) = self.kernel_specification.clone() {
            if let Kernel::RunningKernel(kernel) = &mut self.kernel {
                kernel.force_shutdown(window, cx).detach();
            }

            self.kernel = Kernel::Restarting;
            cx.notify();

            self.launch_kernel_with_spec(spec, window, cx);
        }
    }

    fn interrupt_kernel(
        &mut self,
        _: &InterruptKernel,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A queued run is not on the kernel yet, so interrupting the kernel cannot stop
        // it. Without this the stop control does nothing at all for a cell that is
        // waiting for the kernel to come up.
        self.cancel_pending_executions(cx);

        if let Kernel::RunningKernel(kernel) = &self.kernel {
            let interrupt_request = runtimelib::InterruptRequest {};
            let message: JupyterMessage = interrupt_request.into();
            kernel.request_tx().try_send(message).ok();
            cx.notify();
        }
    }

    /// Drops anything waiting for the kernel and takes those cells out of the running
    /// state.
    fn cancel_pending_executions(&mut self, cx: &mut Context<Self>) {
        for cell_id in std::mem::take(&mut self.pending_executions) {
            if let Some(Cell::Code(cell)) = self.cell_map.get(&cell_id) {
                cell.update(cx, |cell, cx| {
                    cell.finish_execution();
                    cx.notify();
                });
            }
        }
    }

    /// Reports a launch failure on every cell that was waiting for the kernel.
    fn fail_pending_executions(
        &mut self,
        error: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for cell_id in std::mem::take(&mut self.pending_executions) {
            if let Some(Cell::Code(cell)) = self.cell_map.get(&cell_id) {
                cell.update(cx, |cell, cx| {
                    cell.show_kernel_error(error, window, cx);
                    cx.notify();
                });
            }
        }
    }

    /// Runs anything that was asked for while the kernel was starting.
    fn flush_pending_executions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for cell_id in std::mem::take(&mut self.pending_executions) {
            self.execute_cell(cell_id, window, cx);
        }
    }

    fn execute_cell(&mut self, cell_id: CellId, window: &mut Window, cx: &mut Context<Self>) {
        let code = if let Some(Cell::Code(cell)) = self.cell_map.get(&cell_id) {
            cell.read(cx).current_source(cx)
        } else {
            return;
        };

        let request = ExecuteRequest {
            code,
            ..Default::default()
        };
        let message: JupyterMessage = request.into();
        let msg_id = message.header.msg_id.clone();

        let send_result = match &mut self.kernel {
            Kernel::RunningKernel(kernel) => kernel
                .request_tx()
                .try_send(message)
                .map_err(|err| format!("failed to send execute request to kernel (the kernel process may have died): {err}")),
            // Queued rather than refused: the kernel is on its way, so the run is
            // deferred instead of failing. `flush_pending_executions` replays it.
            Kernel::StartingKernel(_) | Kernel::Restarting => {
                if !self.pending_executions.contains(&cell_id) {
                    self.pending_executions.push(cell_id.clone());
                }
                if let Some(Cell::Code(cell)) = self.cell_map.get(&cell_id) {
                    cell.update(cx, |cell, cx| {
                        cell.start_execution();
                        cx.notify();
                    });
                }
                return;
            }
            Kernel::ErroredLaunch(error) => Err(format!("the kernel failed to launch: {error}")),
            Kernel::ShuttingDown | Kernel::Shutdown => Err("the kernel is shut down".to_string()),
        };

        if let Some(Cell::Code(cell)) = self.cell_map.get(&cell_id) {
            cell.update(cx, |cell, cx| {
                if cell.has_outputs() {
                    cell.clear_outputs();
                }
                if let Err(error) = &send_result {
                    cell.show_kernel_error(error, window, cx);
                } else {
                    cell.start_execution();
                }
                cx.notify();
            });
        }

        if let Err(error) = send_result {
            log::error!("notebook: cannot execute cell: {error}");
        } else {
            self.execution_requests.insert(msg_id, cell_id.clone());
        }
    }

    fn get_selected_cell(&self) -> Option<&Cell> {
        self.cell_order
            .get(self.selected_cell_index)
            .and_then(|cell_id| self.cell_map.get(cell_id))
    }

    fn has_outputs(&self, _window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.cell_map.values().any(|cell| {
            if let Cell::Code(code_cell) = cell {
                code_cell.read(cx).has_outputs()
            } else {
                false
            }
        })
    }

    fn clear_outputs(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        for cell in self.cell_map.values() {
            if let Cell::Code(code_cell) = cell {
                code_cell.update(cx, |cell, cx| {
                    cell.clear_outputs();
                    cx.notify();
                });
            }
        }
        cx.notify();
    }

    fn run_cells(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for cell_id in self.cell_order.clone() {
            self.execute_cell(cell_id, window, cx);
        }
    }

    fn run_current_cell(&mut self, _: &Run, window: &mut Window, cx: &mut Context<Self>) {
        // In the source view runs the selected cell, which is unrelated to where the cursor is.
        if self.view_mode != NotebookViewMode::Cells {
            return;
        }
        let Some(cell_id) = self.cell_order.get(self.selected_cell_index).cloned() else {
            return;
        };
        let Some(cell) = self.cell_map.get(&cell_id) else {
            return;
        };
        match cell {
            Cell::Code(_) => {
                self.execute_cell(cell_id, window, cx);
            }
            Cell::Markdown(markdown_cell) => {
                // for markdown, finish editing and move to next cell
                let is_editing = markdown_cell.read(cx).is_editing();
                if is_editing {
                    markdown_cell.update(cx, |cell, cx| {
                        cell.run(cx);
                    });
                    self.enter_command_mode(window, cx);
                }
            }
            Cell::Raw(_) => {}
        }
    }

    fn run_and_advance(&mut self, _: &RunAndAdvance, window: &mut Window, cx: &mut Context<Self>) {
        // In the source view advancing moves the selection and focus, which fights the source editor.
        if self.view_mode != NotebookViewMode::Cells {
            return;
        }
        if let Some(cell_id) = self.cell_order.get(self.selected_cell_index).cloned() {
            if let Some(cell) = self.cell_map.get(&cell_id) {
                match cell {
                    Cell::Code(_) => {
                        self.execute_cell(cell_id, window, cx);
                    }
                    Cell::Markdown(markdown_cell) => {
                        if markdown_cell.read(cx).is_editing() {
                            markdown_cell.update(cx, |cell, cx| {
                                cell.run(cx);
                            });
                        }
                    }
                    Cell::Raw(_) => {}
                }
            }
        }

        let is_last_cell = self.selected_cell_index == self.cell_count().saturating_sub(1);
        if is_last_cell {
            self.add_code_block(window, cx);
            self.enter_command_mode(window, cx);
        } else {
            self.advance_in_command_mode(window, cx);
        }
    }

    fn enter_edit_mode(&mut self, _: &EnterEditMode, window: &mut Window, cx: &mut Context<Self>) {
        self.notebook_mode = NotebookMode::Edit;
        if let Some(cell_id) = self.cell_order.get(self.selected_cell_index) {
            if let Some(cell) = self.cell_map.get(cell_id) {
                match cell {
                    Cell::Code(code_cell) => {
                        let editor = code_cell.read(cx).editor().clone();
                        window.focus(&editor.focus_handle(cx), cx);
                    }
                    Cell::Markdown(markdown_cell) => {
                        markdown_cell.update(cx, |cell, cx| {
                            cell.set_editing(true);
                            cx.notify();
                        });
                        let editor = markdown_cell.read(cx).editor().clone();
                        window.focus(&editor.focus_handle(cx), cx);
                    }
                    Cell::Raw(_) => {}
                }
            }
        }
        cx.notify();
    }

    fn enter_command_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.notebook_mode = NotebookMode::Command;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn handle_enter_command_mode(
        &mut self,
        _: &EnterCommandMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.enter_command_mode(window, cx);
    }

    /// Advances to the next cell while staying in command mode (used by RunAndAdvance and shift-enter).
    fn advance_in_command_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.cell_count();
        if count == 0 {
            return;
        }
        if self.selected_cell_index < count - 1 {
            self.selected_cell_index += 1;
            self.cell_list
                .scroll_to_reveal_item(self.selected_cell_index);
        }
        self.notebook_mode = NotebookMode::Command;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn open_notebook(&mut self, _: &OpenNotebook, _window: &mut Window, _cx: &mut Context<Self>) {}

    fn move_cell_up(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.view_mode != NotebookViewMode::Cells {
            return;
        }
        if self.selected_cell_index > 0 {
            let to = self.selected_cell_index - 1;
            self.move_selected_cell_to(to, cx);
        }
    }

    fn move_cell_down(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.view_mode != NotebookViewMode::Cells {
            return;
        }
        if !self.cell_order.is_empty() && self.selected_cell_index + 1 < self.cell_order.len() {
            let to = self.selected_cell_index + 1;
            self.move_selected_cell_to(to, cx);
        }
    }

    /// Moves the selected cell to `to`, keeping the cell list, the shared buffer and
    /// the moved cell's excerpt in step.
    ///
    /// Reordering used to swap `cell_order` alone, which left the rendered list and the
    /// shared buffer showing the old order.
    fn move_selected_cell_to(&mut self, to: usize, cx: &mut Context<Self>) {
        let from = self.selected_cell_index;
        if from == to || to >= self.cell_order.len() {
            return;
        }

        self.cell_order.swap(from, to);
        self.selected_cell_index = to;

        let cell_id = self.cell_order[to].clone();
        // The cell now sits after whatever precedes it in the new order.
        let after = to.checked_sub(1).map(|index| self.cell_order[index].clone());

        let code_cell = match self.cell_map.get(&cell_id) {
            Some(Cell::Code(cell)) => Some(cell.clone()),
            _ => None,
        };
        if let Some(range) = self.shadow.move_cell(&cell_id, after.as_ref(), cx) {
            if let Some(cell) = code_cell {
                cell.update(cx, |cell, cx| cell.set_excerpt_range(range, cx));
            }
        }

        // Tell the list both positions changed, or it keeps rendering the old order.
        let (lower, upper) = (from.min(to), from.max(to));
        self.cell_list.splice(lower..upper + 1, upper - lower + 1);
        self.cell_list.scroll_to_reveal_item(self.selected_cell_index);
        cx.notify();
    }

    fn delete_cell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Acts on the selected cell, which means nothing in the source view.
        if self.view_mode != NotebookViewMode::Cells {
            return;
        }
        if self.cell_order.is_empty() {
            return;
        }
        let index = self.selected_cell_index.min(self.cell_order.len() - 1);
        let cell_id = self.cell_order.remove(index);
        self.cell_map.remove(&cell_id);
        self.shadow.remove_cell(&cell_id, cx);
        self.cell_list.splice(index..index + 1, 0);

        if self.cell_order.is_empty() {
            self.selected_cell_index = 0;
        } else {
            self.selected_cell_index = index.min(self.cell_order.len() - 1);
            self.cell_list
                .scroll_to_reveal_item(self.selected_cell_index);
        }
        self.notebook_mode = NotebookMode::Command;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn insert_cell_at_current_position(&mut self, cell_id: CellId, cell: Cell) {
        let insert_index = if self.cell_order.is_empty() {
            0
        } else {
            self.selected_cell_index + 1
        };
        self.cell_order.insert(insert_index, cell_id.clone());
        self.cell_map.insert(cell_id, cell);
        self.selected_cell_index = insert_index;
        self.cell_list.splice(insert_index..insert_index, 1);
        self.cell_list.scroll_to_reveal_item(insert_index);
    }

    fn add_markdown_block(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Acts on the selected cell, which means nothing in the source view.
        if self.view_mode != NotebookViewMode::Cells {
            return;
        }
        let new_cell_id: CellId = Uuid::new_v4().into();
        let languages = self.languages.clone();
        let metadata: nbformat::v4::CellMetadata =
            serde_json::from_str("{}").expect("empty object should parse");

        let markdown_cell = cx.new(|cx| {
            super::MarkdownCell::new(
                new_cell_id.clone(),
                metadata,
                None,
                String::new(),
                languages,
                window,
                cx,
            )
        });

        cx.subscribe(
            &markdown_cell,
            move |_this, cell, event: &MarkdownCellEvent, cx| match event {
                MarkdownCellEvent::FinishedEditing | MarkdownCellEvent::Run(_) => {
                    cell.update(cx, |cell, cx| {
                        cell.reparse_markdown(cx);
                    });
                }
            },
        )
        .detach();

        let cell_id_for_editor = new_cell_id.clone();
        let editor = markdown_cell.read(cx).editor().clone();
        cx.subscribe(&editor, move |this, _editor, event, cx| {
            if let editor::EditorEvent::Focused = event {
                this.select_cell_by_id(&cell_id_for_editor, cx);
            }
        })
        .detach();

        self.insert_cell_at_current_position(new_cell_id, Cell::Markdown(markdown_cell.clone()));
        markdown_cell.update(cx, |cell, cx| {
            cell.set_editing(true);
            cx.notify();
        });
        let editor = markdown_cell.read(cx).editor().clone();
        window.focus(&editor.focus_handle(cx), cx);
        self.notebook_mode = NotebookMode::Edit;
        cx.notify();
    }

    fn add_code_block(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Acts on the selected cell, which means nothing in the source view.
        if self.view_mode != NotebookViewMode::Cells {
            return;
        }
        let new_cell_id: CellId = Uuid::new_v4().into();
        let notebook_language = self.notebook_language.clone();

        // The cell needs a range in the shared buffer before its editor can excerpt
        // one. It is inserted after the selected cell, matching where
        // `insert_cell_at_current_position` puts it in the cell list.
        let after = self.cell_order.get(self.selected_cell_index).cloned();
        self.shadow
            .insert_code_cell(new_cell_id.clone(), after.as_ref(), cx);
        let backing = self.shadow.backing_for(&new_cell_id, &self.project, cx);
        let metadata: nbformat::v4::CellMetadata =
            serde_json::from_str("{}").expect("empty object should parse");

        let code_cell = cx.new(|cx| {
            super::CodeCell::new(
                super::CellSource::None,
                new_cell_id.clone(),
                metadata,
                String::new(),
                backing,
                notebook_language,
                window,
                cx,
            )
        });

        let cell_id_for_run = new_cell_id.clone();
        cx.subscribe_in(
            &code_cell,
            window,
            move |this, _cell, event, window, cx| match event {
                CellEvent::Run(cell_id) => this.execute_cell(cell_id.clone(), window, cx),
                CellEvent::FocusedIn(_) => this.select_cell_by_id(&cell_id_for_run, cx),
            },
        )
        .detach();

        let cell_id_for_editor = new_cell_id.clone();
        let editor = code_cell.read(cx).editor().clone();
        cx.subscribe(&editor, move |this, _editor, event, cx| {
            if let editor::EditorEvent::Focused = event {
                this.select_cell_by_id(&cell_id_for_editor, cx);
            }
        })
        .detach();

        self.insert_cell_at_current_position(new_cell_id, Cell::Code(code_cell.clone()));
        let editor = code_cell.read(cx).editor().clone();
        window.focus(&editor.focus_handle(cx), cx);
        self.notebook_mode = NotebookMode::Edit;
        cx.notify();
    }

    fn cell_count(&self) -> usize {
        self.cell_map.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_cell_index
    }

    fn select_cell_by_id(&mut self, cell_id: &CellId, cx: &mut Context<Self>) {
        if let Some(index) = self.cell_order.iter().position(|id| id == cell_id) {
            self.selected_cell_index = index;
            self.notebook_mode = NotebookMode::Edit;
            cx.notify();
        }
    }

    pub fn set_selected_index(
        &mut self,
        index: usize,
        jump_to_index: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if index != self.selected_cell_index {
            self.push_nav_history(cx);
        }
        self.selected_cell_index = index;
        let current_index = self.selected_cell_index;

        // in the future we may have some `on_cell_change` event that we want to fire here

        if jump_to_index {
            self.jump_to_cell(current_index, window, cx);
        }
    }

    fn select_next(
        &mut self,
        _: &menu::SelectNext,
        selection_mode: SelectionMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.cell_count();
        if count > 0 {
            let index = self.selected_index();
            let ix = if index == count - 1 {
                count - 1
            } else {
                index + 1
            };
            self.set_selected_index(ix, true, window, cx);

            if selection_mode == SelectionMode::SelectAndMove
                && let Some(cell) = self.get_selected_cell()
            {
                cell.move_to(MovementDirection::Start, window, cx);
            }

            cx.notify();
        }
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        selection_mode: SelectionMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.cell_count();
        if count > 0 {
            let index = self.selected_index();
            let ix = if index == 0 { 0 } else { index - 1 };
            self.set_selected_index(ix, true, window, cx);

            if selection_mode == SelectionMode::SelectAndMove
                && let Some(cell) = self.get_selected_cell()
            {
                cell.move_to(MovementDirection::End, window, cx);
            }

            cx.notify();
        }
    }

    pub fn select_first(
        &mut self,
        _: &menu::SelectFirst,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.cell_count();
        if count > 0 {
            self.set_selected_index(0, true, window, cx);
            cx.notify();
        }
    }

    pub fn select_last(
        &mut self,
        _: &menu::SelectLast,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.cell_count();
        if count > 0 {
            self.set_selected_index(count - 1, true, window, cx);
            cx.notify();
        }
    }

    /// Moves the shared buffer onto a real file so language servers can attach.
    ///
    /// Zed only registers language servers for file-backed buffers, so without this the
    /// notebook's cells have one shared scope but no completions or diagnostics. Opt-in,
    /// because it puts a file next to the user's notebook.
    ///
    /// Failure is not fatal: the notebook keeps the in-memory buffer it already has, so
    /// a read-only directory or a denied write costs language intelligence, not the
    /// notebook.
    fn start_language_server_sidecar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !JupyterSettings::get_global(cx).language_server_sidecar {
            return;
        }
        // Already upgraded; creating a second sidecar would orphan the first.
        if self.shadow.is_file_backed(cx) {
            return;
        }

        let notebook_path = self.notebook_item.read(cx).project_path.clone();
        let project = self.project.clone();
        let notebook_language = self.notebook_language.clone();

        cx.spawn_in(window, async move |this, cx| {
            let language = notebook_language.await;
            let extension = language
                .as_ref()
                .and_then(|language| language.path_suffixes().first().cloned())
                .unwrap_or_else(|| "txt".to_string());

            let Some(sidecar) = ShadowBuffer::sidecar_path(&notebook_path, &extension) else {
                return;
            };

            let created = project
                .update(cx, |project, cx| {
                    project.create_entry(sidecar.project_path.clone(), false, cx)
                })
                .await;
            if let Err(error) = created {
                log::warn!("notebook: could not create language server sidecar: {error}");
                return;
            }

            let buffer = project
                .update(cx, |project, cx| {
                    project.open_buffer(sidecar.project_path.clone(), cx)
                })
                .await;
            let buffer = match buffer {
                Ok(buffer) => buffer,
                Err(error) => {
                    log::warn!("notebook: could not open language server sidecar: {error}");
                    return;
                }
            };

            this.update_in(cx, |this, _window, cx| {
                let cells = this.to_notebook(cx).cells;
                this.shadow.adopt_buffer(buffer, &cells, cx);
                this.shadow.set_language(language, cx);
                this.sidecar_path = Some(sidecar.project_path);
                this.reexcerpt_all_cells(cx);
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    /// Re-points every code cell's editor at its range in the shared buffer.
    ///
    /// Needed after the buffer is swapped, because anchors belong to the buffer they
    /// were taken from.
    fn reexcerpt_all_cells(&mut self, cx: &mut Context<Self>) {
        let cells: Vec<_> = self
            .cell_order
            .iter()
            .filter_map(|cell_id| match self.cell_map.get(cell_id) {
                Some(Cell::Code(cell)) => Some((cell_id.clone(), cell.clone())),
                _ => None,
            })
            .collect();

        for (cell_id, cell) in cells {
            let Some(range) = self.shadow.source_range(&cell_id, cx) else {
                continue;
            };
            cell.update(cx, |cell, cx| cell.set_excerpt_range(range, cx));
        }
    }

    /// A short label for the selected cell: a markdown cell's first heading, or a code
    /// cell's first `def`/`class`, falling back to its first non-empty line.
    fn selected_cell_label(&self, cx: &App) -> Option<String> {
        let cell_id = self.cell_order.get(self.selected_cell_index)?;
        let cell = self.cell_map.get(cell_id)?;
        let source = match cell {
            Cell::Code(cell) => cell.read(cx).current_source(cx),
            Cell::Markdown(cell) => cell.read(cx).current_source(cx),
            Cell::Raw(_) => return None,
        };
        cell_label(&source)
    }

    /// Diffs the notebook against its committed version, cell by cell.
    ///
    /// `git diff` on an `.ipynb` is unreadable: the file is JSON, so a one-line change
    /// hides among escaped strings, and re-running a cell rewrites kilobytes of base64.
    /// Both sides are projected to source first, so outputs never reach the comparison.
    ///
    /// The result is an ordinary `BufferDiff` over the shared buffer. Cell editors are
    /// excerpts over that buffer, so each one renders the hunks falling inside it in its
    /// own gutter, with no notebook-specific diff UI.
    fn refresh_notebook_diff(&mut self, cx: &mut Context<Self>) {
        let project = self.project.clone();
        let notebook_path = self.notebook_item.read(cx).project_path.clone();

        self._diff_task = cx.spawn(async move |this, cx| {
            let Some(committed) = load_committed_notebook(&project, notebook_path, cx).await
            else {
                return;
            };

            let Some(base) = cx
                .background_spawn(async move { projection_of_notebook(&committed, "#") })
                .await
                .log_err()
            else {
                return;
            };

            let Ok((diff, buffer)) = this.update(cx, |this, cx| {
                let buffer = this.shadow.buffer().clone();
                let snapshot = buffer.read(cx).text_snapshot();
                let diff = cx.new(|cx| BufferDiff::new(&snapshot, None, None, cx));
                this.notebook_diff = Some(diff.clone());
                this.attach_diff_to_cells(&diff, cx);
                (diff, buffer)
            }) else {
                return;
            };

            Self::recompute_diff(&diff, buffer, base.clone(), cx).await;

            this.update(cx, |this, cx| {
                this.notebook_diff_base = Some(base);
                this.refresh_cell_change_markers(cx);
                this.watch_buffer_for_diff(cx);
            })
            .log_err();
        });
    }

    /// Recomputes hunks against the current state of the shared buffer.
    async fn recompute_diff(
        diff: &Entity<BufferDiff>,
        buffer: Entity<language::Buffer>,
        base: String,
        cx: &mut gpui::AsyncApp,
    ) {
        let base: std::sync::Arc<str> = base.into();
        let snapshot = buffer.read_with(cx, |buffer, _| buffer.text_snapshot());
        diff.update(cx, |diff, cx| diff.set_base_text(Some(base), snapshot, cx))
            .await;
    }

    /// Recomputes the diff whenever the shared buffer changes.
    ///
    /// Zed's git store keeps a `BufferDiff` current for a real file, but this one is
    /// built by hand over a buffer that has none. Without this the diff is computed once
    /// when the notebook opens, when nothing has changed yet, and then never again, so
    /// no marker ever appears no matter what is edited.
    fn watch_buffer_for_diff(&mut self, cx: &mut Context<Self>) {
        let buffer = self.shadow.buffer().clone();
        self._diff_subscription = Some(cx.subscribe(&buffer, |this, _buffer, event, cx| {
            if matches!(event, language::BufferEvent::Edited { .. }) {
                this.schedule_diff_recompute(cx);
            }
        }));
    }

    /// Debounced so a burst of keystrokes recomputes once.
    fn schedule_diff_recompute(&mut self, cx: &mut Context<Self>) {
        let (Some(diff), Some(base)) = (self.notebook_diff.clone(), self.notebook_diff_base.clone())
        else {
            return;
        };
        let buffer = self.shadow.buffer().clone();

        self._diff_task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(150))
                .await;

            Self::recompute_diff(&diff, buffer, base, cx).await;
            this.update(cx, |this, cx| this.refresh_cell_change_markers(cx))
                .log_err();
        });
    }

    /// Marks each code cell according to whether the diff has a hunk inside it.
    ///
    /// Cell editors hide their gutter, so `BufferDiff` hunks have nowhere to paint. The
    /// hunks are still the source of truth; this just moves the indication onto the
    /// cell's own margin line.
    fn refresh_cell_change_markers(&mut self, cx: &mut Context<Self>) {
        let Some(diff) = self.notebook_diff.clone() else {
            log::warn!("notebook: no diff, so no cell can be marked as changed");
            return;
        };

        let buffer = self.shadow.buffer().clone();
        let snapshot = buffer.read(cx).snapshot();
        let diff_snapshot = diff.read(cx).snapshot(cx);

        let changed: Vec<(CellId, bool)> = self
            .cell_order
            .iter()
            .filter_map(|cell_id| {
                let range = self.shadow.source_range(cell_id, cx)?;
                let has_hunk = diff_snapshot
                    .hunks_intersecting_range(
                        snapshot.anchor_before(range.start)..snapshot.anchor_after(range.end),
                        &snapshot,
                    )
                    .next()
                    .is_some();
                Some((cell_id.clone(), has_hunk))
            })
            .collect();

        let total_hunks = diff_snapshot
            .hunks_intersecting_range(
                snapshot.anchor_before(0)..snapshot.anchor_after(snapshot.len()),
                &snapshot,
            )
            .count();
        log::info!(
            "notebook: diff has {total_hunks} hunk(s); marking {} of {} cells",
            changed.iter().filter(|(_, changed)| *changed).count(),
            changed.len()
        );

        for (cell_id, has_changes) in changed {
            if let Some(Cell::Code(cell)) = self.cell_map.get(&cell_id) {
                cell.update(cx, |cell, cx| cell.set_has_changes(has_changes, cx));
            }
        }
    }

    /// Gives the notebook's diff to every editor over the shared buffer.
    ///
    /// The source view is a full editor with a gutter, so it renders hunks directly.
    /// Cell editors hide their gutter, so for them this only makes the hunks available;
    /// `refresh_cell_change_markers` is what puts the indication on screen.
    fn attach_diff_to_cells(&self, diff: &Entity<BufferDiff>, cx: &mut Context<Self>) {
        let mut editors: Vec<Entity<Editor>> = self
            .cell_order
            .iter()
            .filter_map(|cell_id| match self.cell_map.get(cell_id) {
                Some(Cell::Code(cell)) => Some(cell.read(cx).editor().clone()),
                _ => None,
            })
            .collect();
        editors.extend(self.source_editor.clone());

        for editor in editors {
            let multi_buffer = editor.read(cx).buffer().clone();
            multi_buffer.update(cx, |multi_buffer, cx| {
                multi_buffer.add_diff(diff.clone(), cx);
            });
        }
    }

    /// Rebuilds the cell list from the source document if that view is showing.
    ///
    /// Saving reads the cells, and a cell is an excerpt over the shared buffer, so text
    /// typed in the source view between two markers belongs to no cell until the list is
    /// rebuilt. Without this, saving from the source view silently drops it.
    fn sync_from_source_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.view_mode == NotebookViewMode::Source {
            self.reconcile_cells_from_source(window, cx);
        }
    }

    /// Switches between the cell view and the notebook as one script.
    ///
    /// The two views are over the same buffer, so no text is copied. Leaving the source
    /// view re-derives the cell list, because editing the text can add, remove, retype
    /// or reorder cells.
    fn toggle_source_view(
        &mut self,
        _: &ToggleSourceView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.view_mode {
            NotebookViewMode::Cells => {
                self.view_mode = NotebookViewMode::Source;
                // The source view is a text buffer. Command mode's bindings act on the
                // selected cell, and `backspace` there deletes one, so leaving the mode
                // behind would delete a cell mid-keystroke.
                self.notebook_mode = NotebookMode::Edit;
                let editor = self.source_editor_or_create(window, cx);
                window.focus(&editor.focus_handle(cx), cx);
            }
            NotebookViewMode::Source => {
                self.reconcile_cells_from_source(window, cx);
                self.view_mode = NotebookViewMode::Cells;
                window.focus(&self.focus_handle, cx);
            }
        }
        cx.notify();
    }

    fn source_editor_or_create(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Editor> {
        if let Some(editor) = self.source_editor.clone() {
            return editor;
        }

        let buffer = self.shadow.buffer().clone();
        let project = self.project.clone();
        let editor = cx.new(|cx| {
            let multi_buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
            Editor::new(
                EditorMode::full(),
                multi_buffer,
                Some(project),
                window,
                cx,
            )
        });

        self.source_editor = Some(editor.clone());

        // The diff usually resolves before anyone opens this view, so hand it over now
        // rather than waiting for the next refresh.
        if let Some(diff) = self.notebook_diff.clone() {
            let multi_buffer = editor.read(cx).buffer().clone();
            multi_buffer.update(cx, |multi_buffer, cx| {
                multi_buffer.add_diff(diff, cx);
            });
        }

        editor
    }

    /// Rebuilds the cell list from the source document.
    ///
    /// Cells are matched by position, since the document carries no ids. A cell that
    /// still lines up keeps its id, metadata and outputs; anything else becomes a new
    /// cell. Type changes count as new, because a code cell's outputs mean nothing on
    /// prose.
    fn reconcile_cells_from_source(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let parsed = self.shadow.parse_cells(cx);
        let existing_types: Vec<CellType> = self
            .cell_order
            .iter()
            .filter_map(|cell_id| match self.cell_map.get(cell_id)? {
                Cell::Code(_) => Some(CellType::Code),
                Cell::Markdown(_) => Some(CellType::Markdown),
                Cell::Raw(_) => Some(CellType::Raw),
            })
            .collect();

        let plan = reconcile_cells(&existing_types, &parsed);

        // Nothing structural changed, so the existing cells already show the new text
        // through their excerpts and there is nothing to rebuild.
        if plan.len() == existing_types.len()
            && plan
                .iter()
                .enumerate()
                .all(|(index, entry)| matches!(entry, CellReconciliation::Kept { index: kept, .. } if *kept == index))
        {
            return;
        }

        let mut cells = Vec::with_capacity(plan.len());
        for entry in &plan {
            match entry {
                CellReconciliation::Kept { index, source } => {
                    let Some(cell_id) = self.cell_order.get(*index) else {
                        continue;
                    };
                    let Some(cell) = self.cell_map.get(cell_id) else {
                        continue;
                    };
                    let mut nbformat_cell = cell.to_nbformat_cell(cx);
                    set_cell_source(&mut nbformat_cell, source);
                    cells.push(nbformat_cell);
                }
                CellReconciliation::Added { cell_type, source } => {
                    cells.push(new_nbformat_cell(cell_type.clone(), source));
                }
            }
        }

        self.rebuild_cells_from(cells, window, cx);
    }

    /// Replaces the cell list wholesale, reprojecting the shared buffer and rebuilding
    /// every cell editor against it.
    fn rebuild_cells_from(
        &mut self,
        cells: Vec<nbformat::v4::Cell>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.shadow.rebuild(&cells, cx);

        let languages = self.languages.clone();
        let notebook_language = self.notebook_language.clone();
        let project = self.project.clone();

        let mut cell_order = Vec::with_capacity(cells.len());
        let mut cell_map = HashMap::default();
        for cell in &cells {
            let cell_id = cell.id();
            cell_order.push(cell_id.clone());
            let backing = self.shadow.backing_for(cell_id, &project, cx);
            let entity = Cell::load(
                cell,
                &languages,
                backing,
                notebook_language.clone(),
                window,
                cx,
            );
            cell_map.insert(cell_id.clone(), entity);
        }

        self.cell_order = cell_order;
        self.cell_map = cell_map;
        self.selected_cell_index = self.selected_cell_index.min(self.cell_order.len().saturating_sub(1));
        self.cell_list = ListState::new(self.cell_order.len(), gpui::ListAlignment::Top, px(1000.));
        cx.notify();
    }

    /// Selects `cell_id` and brings it on screen. Used by the outline.
    ///
    /// In the source view there is no cell list to scroll, so the cursor moves to the
    /// cell's text instead. Scrolling a list nobody can see is why picking an outline
    /// entry from the source view appeared to do nothing.
    pub fn select_cell_by_id_and_reveal(
        &mut self,
        cell_id: &CellId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self
            .cell_order
            .iter()
            .position(|existing| existing == cell_id)
        else {
            return;
        };

        match self.view_mode {
            NotebookViewMode::Cells => {
                self.set_selected_index(index, true, window, cx);
            }
            NotebookViewMode::Source => {
                self.set_selected_index(index, false, window, cx);
                self.reveal_cell_in_source(cell_id, window, cx);
            }
        }
        cx.notify();
    }

    /// Moves the source view's cursor to the start of `cell_id` and scrolls it into
    /// view.
    ///
    /// Markdown and raw cells have no range in the shared buffer, so this falls back to
    /// the nearest preceding code cell rather than doing nothing.
    fn reveal_cell_in_source(
        &mut self,
        cell_id: &CellId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.source_editor.clone() else {
            return;
        };

        let range = self.shadow.source_range(cell_id, cx).or_else(|| {
            let index = self
                .cell_order
                .iter()
                .position(|existing| existing == cell_id)?;
            self.cell_order[..index]
                .iter()
                .rev()
                .find_map(|earlier| self.shadow.source_range(earlier, cx))
        });
        let Some(range) = range else {
            return;
        };

        editor.update(cx, |editor, cx| {
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::center()),
                window,
                cx,
                |selections| selections.select_ranges([range.start..range.start]),
            );
        });
        window.focus(&editor.focus_handle(cx), cx);
    }

    /// The notebook's cells as outline entries, in order.
    pub fn outline_entries(&self, cx: &App) -> Vec<OutlineEntry> {
        self.cell_order
            .iter()
            .enumerate()
            .filter_map(|(index, cell_id)| {
                let cell = self.cell_map.get(cell_id)?;
                let (cell_type, source) = match cell {
                    Cell::Code(cell) => (CellType::Code, cell.read(cx).current_source(cx)),
                    Cell::Markdown(cell) => (CellType::Markdown, cell.read(cx).current_source(cx)),
                    Cell::Raw(_) => (CellType::Raw, String::new()),
                };
                Some(OutlineEntry {
                    cell_id: cell_id.clone(),
                    cell_type,
                    index,
                    // An unlabelable cell still needs a row, or the outline would skip
                    // cells and its numbering would not match the notebook.
                    label: cell_label(&source).unwrap_or_else(|| format!("Cell {}", index + 1)),
                })
            })
            .collect()
    }

    fn toggle_outline(
        &mut self,
        _: &zed_actions::outline::ToggleOutline,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = window.root::<workspace::MultiWorkspace>().flatten() else {
            return;
        };
        let workspace = workspace.read(cx).workspace().clone();

        let entries = self.outline_entries(cx);
        if entries.is_empty() {
            return;
        }
        let selected_index = self.selected_cell_index.min(entries.len() - 1);
        let notebook = cx.entity().downgrade();

        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                NotebookOutline::new(notebook, entries, selected_index, window, cx)
            });
        });
    }

    /// Records the cell currently selected, so go-back returns to it rather than to
    /// whichever cell happens to be selected later.
    fn push_nav_history(&mut self, cx: &mut Context<Self>) {
        let Some(cell_id) = self.cell_order.get(self.selected_cell_index).cloned() else {
            return;
        };
        if let Some(nav_history) = self.nav_history.as_mut() {
            nav_history.push(Some(NotebookNavigationData { cell_id }), None, cx);
        }
    }

    fn jump_to_cell(&mut self, index: usize, _window: &mut Window, _cx: &mut Context<Self>) {
        self.cell_list.scroll_to_reveal_item(index);
    }

    fn button_group(_window: &mut Window, cx: &mut Context<Self>) -> Div {
        v_flex()
            .gap(DynamicSpacing::Base04.rems(cx))
            .items_center()
            .w(px(CONTROL_SIZE + 4.0))
            .overflow_hidden()
            .rounded(px(5.))
            .bg(cx.theme().colors().title_bar_background)
            .p_px()
            .border_1()
            .border_color(cx.theme().colors().border)
    }

    fn render_notebook_control(
        id: impl Into<SharedString>,
        icon: IconName,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> IconButton {
        let id: ElementId = ElementId::Name(id.into());
        IconButton::new(id, icon).width(px(CONTROL_SIZE))
    }

    fn render_notebook_controls(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let has_outputs = self.has_outputs(window, cx);

        v_flex()
            .max_w(px(CONTROL_SIZE + 4.0))
            .items_center()
            .gap(DynamicSpacing::Base16.rems(cx))
            .justify_between()
            .flex_none()
            .h_full()
            .py(DynamicSpacing::Base12.px(cx))
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base08.rems(cx))
                    .child(
                        Self::button_group(window, cx)
                            .child(
                                Self::render_notebook_control(
                                    "run-all-cells",
                                    IconName::PlayFilled,
                                    window,
                                    cx,
                                )
                                .tooltip(move |_window, cx| {
                                    Tooltip::for_action("Execute all cells", &RunAll, cx)
                                })
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(RunAll), cx);
                                }),
                            )
                            .child(
                                Self::render_notebook_control(
                                    "clear-all-outputs",
                                    IconName::ListX,
                                    window,
                                    cx,
                                )
                                .disabled(!has_outputs)
                                .tooltip(move |_window, cx| {
                                    Tooltip::for_action("Clear all outputs", &ClearOutputs, cx)
                                })
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(ClearOutputs), cx);
                                }),
                            ),
                    )
                    .child(
                        Self::button_group(window, cx)
                            .child(
                                Self::render_notebook_control(
                                    "move-cell-up",
                                    IconName::ArrowUp,
                                    window,
                                    cx,
                                )
                                .tooltip(move |_window, cx| {
                                    Tooltip::for_action("Move cell up", &MoveCellUp, cx)
                                })
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(MoveCellUp), cx);
                                }),
                            )
                            .child(
                                Self::render_notebook_control(
                                    "move-cell-down",
                                    IconName::ArrowDown,
                                    window,
                                    cx,
                                )
                                .tooltip(move |_window, cx| {
                                    Tooltip::for_action("Move cell down", &MoveCellDown, cx)
                                })
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(MoveCellDown), cx);
                                }),
                            ),
                    )
                    .child(
                        Self::button_group(window, cx)
                            .child(
                                Self::render_notebook_control(
                                    "new-markdown-cell",
                                    IconName::Plus,
                                    window,
                                    cx,
                                )
                                .tooltip(move |_window, cx| {
                                    Tooltip::for_action("Add markdown block", &AddMarkdownBlock, cx)
                                })
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(AddMarkdownBlock), cx);
                                }),
                            )
                            .child(
                                Self::render_notebook_control(
                                    "new-code-cell",
                                    IconName::Code,
                                    window,
                                    cx,
                                )
                                .tooltip(move |_window, cx| {
                                    Tooltip::for_action("Add code block", &AddCodeBlock, cx)
                                })
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(AddCodeBlock), cx);
                                }),
                            ),
                    )
                    .child(
                        Self::button_group(window, cx).child(
                            Self::render_notebook_control(
                                "delete-cell",
                                IconName::Trash,
                                window,
                                cx,
                            )
                            .disabled(self.cell_order.is_empty())
                            .tooltip(move |_window, cx| {
                                Tooltip::for_action("Delete cell", &DeleteCell, cx)
                            })
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(DeleteCell), cx);
                            }),
                        ),
                    ),
            )
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base08.rems(cx))
                    .items_center()
                    .child(
                        Self::render_notebook_control("more-menu", IconName::Ellipsis, window, cx)
                            .tooltip(move |window, cx| (Tooltip::text("More options"))(window, cx)),
                    )
                    .child(Self::button_group(window, cx).child({
                        let kernel_status = self.kernel.status();
                        let (icon, icon_color) = match &kernel_status {
                            KernelStatus::Idle => (IconName::ReplNeutral, Color::Success),
                            KernelStatus::Busy => (IconName::ReplNeutral, Color::Warning),
                            KernelStatus::Starting => (IconName::ReplNeutral, Color::Muted),
                            KernelStatus::Error => (IconName::ReplNeutral, Color::Error),
                            KernelStatus::ShuttingDown => (IconName::ReplNeutral, Color::Muted),
                            KernelStatus::Shutdown => (IconName::ReplNeutral, Color::Disabled),
                            KernelStatus::Restarting => (IconName::ReplNeutral, Color::Warning),
                        };
                        let kernel_name = self
                            .kernel_specification
                            .as_ref()
                            .map(|spec| spec.name().to_string())
                            .unwrap_or_else(|| "Select Kernel".to_string());
                        IconButton::new("repl", icon)
                            .icon_color(icon_color)
                            .tooltip(move |window, cx| {
                                Tooltip::text(format!(
                                    "{} ({}). Click to change kernel.",
                                    kernel_name,
                                    kernel_status.to_string()
                                ))(window, cx)
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.kernel_picker_handle.toggle(window, cx);
                            }))
                    })),
            )
    }

    fn render_kernel_status_bar(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let kernel_status = self.kernel.status();
        let kernel_name = self
            .kernel_specification
            .as_ref()
            .map(|spec| spec.name().to_string())
            .unwrap_or_else(|| "Select Kernel".to_string());

        let (status_icon, status_color) = match &kernel_status {
            KernelStatus::Idle => (IconName::Circle, Color::Success),
            KernelStatus::Busy => (IconName::ArrowCircle, Color::Warning),
            KernelStatus::Starting => (IconName::ArrowCircle, Color::Muted),
            KernelStatus::Error => (IconName::XCircle, Color::Error),
            KernelStatus::ShuttingDown => (IconName::ArrowCircle, Color::Muted),
            KernelStatus::Shutdown => (IconName::Circle, Color::Muted),
            KernelStatus::Restarting => (IconName::ArrowCircle, Color::Warning),
        };

        // States where the kernel is doing something, which the selector renders as a
        // spinner rather than a static icon.
        let is_working = matches!(
            kernel_status,
            KernelStatus::Busy
                | KernelStatus::Starting
                | KernelStatus::ShuttingDown
                | KernelStatus::Restarting
        );

        let worktree_id = self.worktree_id;
        let kernel_picker_handle = self.kernel_picker_handle.clone();
        let view = cx.entity().downgrade();

        h_flex()
            .w_full()
            .px_3()
            .py_1()
            .gap_2()
            .items_center()
            .justify_between()
            .bg(cx.theme().colors().status_bar_background)
            .child(
                KernelSelector::new(
                    std::rc::Rc::new(move |spec: KernelSpecification, window, cx| {
                        let Some(view) = view.upgrade() else {
                            return;
                        };
                        if spec.has_ipykernel() {
                            view.update(cx, |this, cx| {
                                this.change_kernel(spec, window, cx);
                            });
                            return;
                        }
                        // Picking an environment without ipykernel used to select a
                        // kernel that could never start. Offer to install it instead.
                        crate::install_ipykernel(spec, window, cx, move |spec, window, cx| {
                            view.update(cx, |this, cx| {
                                this.change_kernel(spec, window, cx);
                            });
                            Ok(())
                        })
                        .log_err();
                    }),
                    worktree_id,
                    Button::new("kernel-selector", kernel_name.clone())
                        .label_size(LabelSize::Small)
                        // `loading` swaps the start icon for a rotating spinner, which
                        // is the only way to animate it here: `start_icon` takes an
                        // `Icon`, not an element.
                        .loading(is_working)
                        .start_icon(
                            Icon::new(status_icon)
                                .size(IconSize::Small)
                                .color(status_color),
                        ),
                    Tooltip::text(format!(
                        "Kernel: {} ({}). Click to change.",
                        kernel_name,
                        kernel_status.to_string()
                    )),
                )
                .with_handle(kernel_picker_handle),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        IconButton::new("restart-kernel", IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .tooltip(|_window, cx| {
                                Tooltip::for_action("Restart Kernel", &RestartKernel, cx)
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.restart_kernel(&RestartKernel, window, cx);
                            })),
                    )
                    .child(
                        // Kernel stdout and stderr are logged with a `kernel:` prefix,
                        // so the application log is where a kernel that will not start
                        // explains itself.
                        IconButton::new("open-kernel-log", IconName::FileDoc)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Open Kernel Log"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(workspace::OpenLog), cx);
                            }),
                    )
                    .child(
                        IconButton::new("interrupt-kernel", IconName::Stop)
                            .icon_size(IconSize::Small)
                            .disabled(!matches!(kernel_status, KernelStatus::Busy))
                            .tooltip(|_window, cx| {
                                Tooltip::for_action("Interrupt Kernel", &InterruptKernel, cx)
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.interrupt_kernel(&InterruptKernel, window, cx);
                            })),
                    ),
            )
    }


    fn cell_list(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        list(self.cell_list.clone(), move |index, window, cx| {
            view.update(cx, |this, cx| {
                // Panicking here would take the editor down mid-frame, and cell order
                // and the map can disagree transiently while cells are added or removed.
                let Some(cell) = this
                    .cell_order
                    .get(index)
                    .and_then(|cell_id| this.cell_map.get(cell_id))
                    .cloned()
                else {
                    return div().into_any_element();
                };
                this.render_cell(index, &cell, window, cx).into_any_element()
            })
        })
        .size_full()
    }

    fn render_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .child(Label::new("This notebook is empty.").color(Color::Muted))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("empty-state-add-code", "Add code cell")
                            .start_icon(Icon::new(IconName::Code))
                            .key_binding(KeyBinding::for_action_in(
                                &AddCodeBlock,
                                &self.focus_handle,
                                cx,
                            ))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.add_code_block(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("empty-state-add-markdown", "Add markdown cell")
                            .style(ButtonStyle::Subtle)
                            .start_icon(Icon::new(IconName::FileMarkdown))
                            .key_binding(KeyBinding::for_action_in(
                                &AddMarkdownBlock,
                                &self.focus_handle,
                                cx,
                            ))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_markdown_block(window, cx)
                            })),
                    ),
            )
    }

    fn cell_position(&self, index: usize) -> CellPosition {
        match index {
            0 => CellPosition::First,
            index if index == self.cell_count() - 1 => CellPosition::Last,
            _ => CellPosition::Middle,
        }
    }

    fn render_cell(
        &self,
        index: usize,
        cell: &Cell,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let cell_position = self.cell_position(index);

        let is_selected = index == self.selected_cell_index;

        match cell {
            Cell::Code(cell) => {
                cell.update(cx, |cell, _cx| {
                    cell.set_selected(is_selected)
                        .set_cell_position(cell_position);
                });
                cell.clone().into_any_element()
            }
            Cell::Markdown(cell) => {
                cell.update(cx, |cell, _cx| {
                    cell.set_selected(is_selected)
                        .set_cell_position(cell_position);
                });
                cell.clone().into_any_element()
            }
            Cell::Raw(cell) => {
                cell.update(cx, |cell, _cx| {
                    cell.set_selected(is_selected)
                        .set_cell_position(cell_position);
                });
                cell.clone().into_any_element()
            }
        }
    }
}

impl Render for NotebookEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut key_context = KeyContext::new_with_defaults();
        key_context.add("NotebookEditor");
        key_context.set(
            "notebook_mode",
            match self.notebook_mode {
                NotebookMode::Command => "command",
                NotebookMode::Edit => "edit",
            },
        );
        // Cell-level bindings are scoped to the cell view. In the source view the
        // notebook is one text buffer, so moving a cell would shuffle text under the
        // cursor, and the editor's own line bindings are what a person expects.
        key_context.set(
            "notebook_view",
            match self.view_mode {
                NotebookViewMode::Cells => "cells",
                NotebookViewMode::Source => "source",
            },
        );

        v_flex()
            .size_full()
            .key_context(key_context)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &OpenNotebook, window, cx| {
                this.open_notebook(&OpenNotebook, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &ClearOutputs, window, cx| this.clear_outputs(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &Run, window, cx| this.run_current_cell(&Run, window, cx)),
            )
            .on_action(
                cx.listener(|this, action, window, cx| this.run_and_advance(action, window, cx)),
            )
            .on_action(cx.listener(|this, _: &RunAll, window, cx| this.run_cells(window, cx)))
            // Captured rather than bubbled: in the source view the inner editor has
            // focus and registers its own `ToggleOutline`, which bails silently because
            // a nested editor has no workspace. Capturing means the notebook's outline
            // answers wherever focus happens to be.
            .capture_action(cx.listener(Self::toggle_outline))
            .capture_action(cx.listener(Self::toggle_source_view))
            .on_action(
                cx.listener(|this, _: &MoveCellUp, window, cx| this.move_cell_up(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &MoveCellDown, window, cx| this.move_cell_down(window, cx)),
            )
            .on_action(cx.listener(|this, _: &AddMarkdownBlock, window, cx| {
                this.add_markdown_block(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &AddCodeBlock, window, cx| this.add_code_block(window, cx)),
            )
            .on_action(cx.listener(|this, _: &DeleteCell, window, cx| this.delete_cell(window, cx)))
            .on_action(
                cx.listener(|this, action, window, cx| this.enter_edit_mode(action, window, cx)),
            )
            .on_action(cx.listener(|this, action, window, cx| {
                this.handle_enter_command_mode(action, window, cx)
            }))
            .on_action(cx.listener(|this, action, window, cx| {
                this.select_next(action, SelectionMode::SelectOnly, window, cx)
            }))
            .on_action(cx.listener(|this, action, window, cx| {
                this.select_previous(action, SelectionMode::SelectOnly, window, cx)
            }))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(|this, _: &MoveDown, window, cx| {
                this.select_next(
                    &Default::default(),
                    SelectionMode::SelectAndMove,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &MoveUp, window, cx| {
                this.select_previous(
                    &Default::default(),
                    SelectionMode::SelectAndMove,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &NotebookMoveDown, window, cx| {
                let Some(cell) = this.get_selected_cell() else {
                    return;
                };

                let Some(editor) = cell.editor(cx).cloned() else {
                    return;
                };

                let is_at_last_line = editor.update(cx, |editor, cx| {
                    let display_snapshot = editor.display_snapshot(cx);
                    let selections = editor.selections.all_display(&display_snapshot);
                    if let Some(selection) = selections.last() {
                        let head = selection.head();
                        let cursor_row = head.row();
                        let max_row = display_snapshot.max_point().row();

                        cursor_row >= max_row
                    } else {
                        false
                    }
                });

                if is_at_last_line {
                    this.select_next(
                        &Default::default(),
                        SelectionMode::SelectAndMove,
                        window,
                        cx,
                    );
                } else {
                    editor.update(cx, |editor, cx| {
                        editor.move_down(&Default::default(), window, cx);
                    });
                }
            }))
            .on_action(cx.listener(|this, _: &NotebookMoveUp, window, cx| {
                let Some(cell) = this.get_selected_cell() else {
                    return;
                };

                let Some(editor) = cell.editor(cx).cloned() else {
                    return;
                };

                let is_at_first_line = editor.update(cx, |editor, cx| {
                    let display_snapshot = editor.display_snapshot(cx);
                    let selections = editor.selections.all_display(&display_snapshot);
                    if let Some(selection) = selections.first() {
                        let head = selection.head();
                        let cursor_row = head.row();

                        cursor_row.0 == 0
                    } else {
                        false
                    }
                });

                if is_at_first_line {
                    this.select_previous(
                        &Default::default(),
                        SelectionMode::SelectAndMove,
                        window,
                        cx,
                    );
                } else {
                    editor.update(cx, |editor, cx| {
                        editor.move_up(&Default::default(), window, cx);
                    });
                }
            }))
            .on_action(
                cx.listener(|this, action, window, cx| this.restart_kernel(action, window, cx)),
            )
            .on_action(
                cx.listener(|this, action, window, cx| this.interrupt_kernel(action, window, cx)),
            )
            .child(
                h_flex()
                    .flex_1()
                    .w_full()
                    .h_full()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .child(match self.view_mode {
                                NotebookViewMode::Source => self
                                    .source_editor_or_create(window, cx)
                                    .into_any_element(),
                                NotebookViewMode::Cells if self.cell_order.is_empty() => {
                                    self.render_empty_state(cx).into_any_element()
                                }
                                NotebookViewMode::Cells => {
                                    self.cell_list(window, cx).into_any_element()
                                }
                            }),
                    )
                    .child(self.render_notebook_controls(window, cx)),
            )
            .child(self.render_kernel_status_bar(window, cx))
    }
}

impl Focusable for NotebookEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

// Intended to be a NotebookBuffer
pub struct NotebookItem {
    project_path: ProjectPath,
    languages: Arc<LanguageRegistry>,
    // Raw notebook data
    notebook: nbformat::v4::Notebook,
    /// The file exactly as it was read, used to write it back in the same shape:
    /// same key order, same indent, same trailing newline. Without it, opening and
    /// saving a notebook reformats the whole file.
    original_json: Option<serde_json::Value>,
    /// Held so the editor can watch for writes it did not make. Discarding a hunk in the
    /// git panel rewrites the file on disk; without this the open notebook kept showing
    /// the old cells.
    buffer: Entity<language::Buffer>,
    // Store our version of the notebook in memory (cell_order, cell_map)
    id: ProjectEntryId,
}

impl project::ProjectItem for NotebookItem {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<anyhow::Result<Entity<Self>>>> {
        let path = path.clone();
        let project = project.clone();
        let languages = project.read(cx).languages().clone();

        // For single-file worktrees the relative path is empty, so fall back
        // to the absolute path to detect notebooks opened directly.
        let abs_path = project.read(cx).absolute_path(&path, cx);
        let is_notebook = path.path.extension().unwrap_or_default() == NOTEBOOK_EXTENSION
            || abs_path
                .as_ref()
                .and_then(|abs_path| abs_path.extension())
                .is_some_and(|extension| extension == NOTEBOOK_EXTENSION);

        if is_notebook {
            Some(cx.spawn(async move |cx| {
                let buffer = project
                    .update(cx, |project, cx| project.open_buffer(path.clone(), cx))
                    .await?;
                let file_content = buffer.read_with(cx, |buffer, _| buffer.text());

                let notebook = if file_content.trim().is_empty() {
                    nbformat::v4::Notebook {
                        nbformat: 4,
                        nbformat_minor: 5,
                        cells: vec![],
                        metadata: serde_json::from_str("{}")
                            .context("empty notebook metadata should parse")?,
                    }
                } else {
                    let notebook = match nbformat::parse_notebook(&file_content) {
                        Ok(nb) => nb,
                        Err(_) => {
                            // Pre-process to ensure IDs exist
                            let mut json: serde_json::Value = serde_json::from_str(&file_content)?;
                            if let Some(cells) =
                                json.get_mut("cells").and_then(|c| c.as_array_mut())
                            {
                                for cell in cells {
                                    if cell.get("id").is_none() {
                                        cell["id"] =
                                            serde_json::Value::String(Uuid::new_v4().to_string());
                                    }
                                }
                            }
                            let file_content = serde_json::to_string(&json)?;
                            nbformat::parse_notebook(&file_content)?
                        }
                    };

                    match notebook {
                        nbformat::Notebook::V4(notebook) => notebook,
                        // 4.1 - 4.4 are converted to 4.5
                        nbformat::Notebook::Legacy(legacy_notebook) => {
                            // TODO: Decide if we want to mutate the notebook by including Cell IDs
                            // and any other conversions

                            nbformat::upgrade_legacy_notebook(legacy_notebook)?
                        }
                        nbformat::Notebook::V3(v3_notebook) => {
                            nbformat::upgrade_v3_notebook(v3_notebook)?
                        }
                    }
                };

                let id = project
                    .update(cx, |project, cx| {
                        project.entry_for_path(&path, cx).map(|entry| entry.id)
                    })
                    .context("Entry not found")?;

                let original_json = serde_json::from_str(&file_content).ok();

                Ok(cx.new(|_| NotebookItem {
                    project_path: path,
                    languages,
                    notebook,
                    original_json,
                    buffer,
                    id,
                }))
            }))
        } else {
            None
        }
    }

    fn entry_id(&self, _: &App) -> Option<ProjectEntryId> {
        Some(self.id)
    }

    fn project_path(&self, _: &App) -> Option<ProjectPath> {
        Some(self.project_path.clone())
    }

    fn is_dirty(&self) -> bool {
        // TODO: Track if notebook metadata or structure has changed
        false
    }
}

impl NotebookItem {
    pub fn language_name(&self) -> Option<String> {
        self.notebook
            .metadata
            .language_info
            .as_ref()
            .map(|l| l.name.clone())
            .or(self
                .notebook
                .metadata
                .kernelspec
                .as_ref()
                .and_then(|spec| spec.language.clone()))
    }

    pub fn notebook_language(&self) -> impl Future<Output = Option<Arc<Language>>> + use<> {
        let language_name = self.language_name();
        let languages = self.languages.clone();

        async move {
            if let Some(language_name) = language_name {
                languages.language_for_name(&language_name).await.ok()
            } else {
                None
            }
        }
    }
}

impl EventEmitter<()> for NotebookItem {}

impl EventEmitter<()> for NotebookEditor {}

// pub struct NotebookControls {
//     pane_focused: bool,
//     active_item: Option<Box<dyn ItemHandle>>,
//     // subscription: Option<Subscription>,
// }

// impl NotebookControls {
//     pub fn new() -> Self {
//         Self {
//             pane_focused: false,
//             active_item: Default::default(),
//             // subscription: Default::default(),
//         }
//     }
// }

// impl EventEmitter<ToolbarItemEvent> for NotebookControls {}

// impl Render for NotebookControls {
//     fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
//         div().child("notebook controls")
//     }
// }

// impl ToolbarItemView for NotebookControls {
//     fn set_active_pane_item(
//         &mut self,
//         active_pane_item: Option<&dyn workspace::ItemHandle>,
//         window: &mut Window, cx: &mut Context<Self>,
//     ) -> workspace::ToolbarItemLocation {
//         cx.notify();
//         self.active_item = None;

//         let Some(item) = active_pane_item else {
//             return ToolbarItemLocation::Hidden;
//         };

//         ToolbarItemLocation::PrimaryLeft
//     }

//     fn pane_focus_update(&mut self, pane_focused: bool, _window: &mut Window, _cx: &mut Context<Self>) {
//         self.pane_focused = pane_focused;
//     }
// }

impl Item for NotebookEditor {
    type Event = ();

    /// Splitting is disabled until the notebook can be shown in two places at once.
    ///
    /// `clone_on_split` builds a second `NotebookEditor` from the same notebook item,
    /// and each one creates its own shared buffer and its own cell entities from the
    /// notebook as it was parsed from disk. The panes then diverge: edits in one are
    /// invisible in the other, and whichever saves last silently discards the other's
    /// work. Making this work means sharing the shadow buffer and the cell entities
    /// across views, which is a larger change than disabling a feature nobody has been
    /// able to use correctly.
    fn can_split(&self) -> bool {
        false
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        // See `can_split`: a second view would diverge from this one.
        Task::ready(None)
    }

    fn buffer_kind(&self, _: &App) -> workspace::item::ItemBufferKind {
        workspace::item::ItemBufferKind::Singleton
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        f(self.notebook_item.entity_id(), self.notebook_item.read(cx))
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.notebook_item
            .read(cx)
            .project_path
            .path
            .file_name()
            .map(|s| s.to_string())
            .unwrap_or_default()
            .into()
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(params.detail.unwrap_or(0), cx))
            .single_line()
            .color(params.text_color())
            .when(params.preview, |this| this.italic())
            .into_any_element()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(IconName::Book.into())
    }

    fn show_toolbar(&self) -> bool {
        true
    }

    fn breadcrumb_location(&self, _: &App) -> ToolbarItemLocation {
        ToolbarItemLocation::PrimaryLeft
    }

    /// Notebook path, then which cell you are on, then that cell's heading or first
    /// definition. The middle segment is what a plain editor cannot give you: in a
    /// notebook "cell 7 of 40" is most of the orientation.
    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<Font>)> {
        let font = theme_settings::ThemeSettings::get_global(cx)
            .buffer_font
            .clone();

        let path = self
            .notebook_item
            .read(cx)
            .project_path
            .path
            .file_name()
            .unwrap_or("notebook")
            .to_string();

        let mut breadcrumbs = vec![HighlightedText {
            text: path.into(),
            highlights: Vec::new(),
        }];

        if !self.cell_order.is_empty() {
            breadcrumbs.push(HighlightedText {
                text: format!(
                    "Cell {} of {}",
                    self.selected_cell_index + 1,
                    self.cell_order.len()
                )
                .into(),
                highlights: Vec::new(),
            });
        }

        if let Some(label) = self.selected_cell_label(cx) {
            breadcrumbs.push(HighlightedText {
                text: label.into(),
                highlights: Vec::new(),
            });
        }

        Some((breadcrumbs, Some(font)))
    }

    /// The cursor of whichever cell is selected, so popovers and the assistant anchor
    /// to the right place instead of the top left of the window.
    fn pixel_position_of_cursor(&self, cx: &App) -> Option<Point<Pixels>> {
        let cell_id = self.cell_order.get(self.selected_cell_index)?;
        let editor = self.cell_map.get(cell_id)?.editor(cx)?;
        editor.read(cx).pixel_position_of_cursor(cx)
    }

    fn as_searchable(
        &self,
        handle: &Entity<Self>,
        _: &App,
    ) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(handle.clone()))
    }

    fn set_nav_history(
        &mut self,
        nav_history: workspace::ItemNavHistory,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) {
        self.nav_history = Some(nav_history);
    }

    fn navigate(
        &mut self,
        data: Arc<dyn std::any::Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(data) = data.downcast_ref::<NotebookNavigationData>() else {
            return false;
        };
        let Some(index) = self
            .cell_order
            .iter()
            .position(|cell_id| cell_id == &data.cell_id)
        else {
            // The cell was deleted since the entry was recorded.
            return false;
        };
        if index == self.selected_cell_index {
            return false;
        }

        self.selected_cell_index = index;
        self.jump_to_cell(index, window, cx);
        cx.notify();
        true
    }

    fn can_save(&self, _cx: &App) -> bool {
        true
    }

    /// `save_as` is implemented below, but the trait default is `false`, so without this
    /// the Save As keybinding silently did nothing.
    fn can_save_as(&self, _cx: &App) -> bool {
        true
    }

    fn save(
        &mut self,
        _options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.sync_from_source_view(window, cx);
        self.save_impl(SaveDestination::CurrentPath, project, cx)
    }

    fn save_as(
        &mut self,
        project: Entity<Project>,
        path: ProjectPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.sync_from_source_view(window, cx);
        self.save_impl(SaveDestination::NewPath(path), project, cx)
    }

    fn reload(
        &mut self,
        _project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let project_path = self.notebook_item.read(cx).project_path.clone();
        let languages = self.languages.clone();
        let notebook_language = self.notebook_language.clone();

        cx.spawn_in(window, async move |this, cx| {
            let buffer = this
                .update(cx, |this, cx| {
                    this.project
                        .update(cx, |project, cx| project.open_buffer(project_path, cx))
                })?
                .await?;

            let file_content = buffer.read_with(cx, |buffer, _| buffer.text());

            let mut json: serde_json::Value = serde_json::from_str(&file_content)?;
            if let Some(cells) = json.get_mut("cells").and_then(|c| c.as_array_mut()) {
                for cell in cells {
                    if cell.get("id").is_none() {
                        cell["id"] = serde_json::Value::String(Uuid::new_v4().to_string());
                    }
                }
            }
            let file_content = serde_json::to_string(&json)?;

            let notebook = nbformat::parse_notebook(&file_content);
            let notebook = match notebook {
                Ok(nbformat::Notebook::V4(notebook)) => notebook,
                Ok(nbformat::Notebook::Legacy(legacy_notebook)) => {
                    nbformat::upgrade_legacy_notebook(legacy_notebook)?
                }
                Ok(nbformat::Notebook::V3(v3_notebook)) => {
                    nbformat::upgrade_v3_notebook(v3_notebook)?
                }
                Err(e) => {
                    anyhow::bail!("Failed to parse notebook: {:?}", e);
                }
            };

            this.update_in(cx, |this, window, cx| {
                let mut cell_order = vec![];
                let mut cell_map = HashMap::default();

                // Reload used to rebuild only the cells. The notebook's own metadata and
                // the key-order template kept their pre-reload values, so the next save
                // wrote the stale kernelspec and the stale ordering straight back over
                // whatever had just been restored on disk.
                this.notebook_item.update(cx, |item, _| {
                    item.notebook = notebook.clone();
                    item.original_json = serde_json::from_str(&file_content).ok();
                });

                this.shadow.rebuild(&notebook.cells, cx);

                for cell in notebook.cells.iter() {
                    let cell_id = cell.id();
                    cell_order.push(cell_id.clone());
                    let backing = this.shadow.backing_for(cell_id, &this.project, cx);
                    let cell_entity = Cell::load(
                        cell,
                        &languages,
                        backing,
                        notebook_language.clone(),
                        window,
                        cx,
                    );
                    cell_map.insert(cell_id.clone(), cell_entity);
                }

                this.cell_order = cell_order.clone();
                this.original_cell_order = cell_order;
                this.cell_map = cell_map;
                this.cell_list =
                    ListState::new(this.cell_order.len(), gpui::ListAlignment::Top, px(1000.));
                cx.notify();
            })?;

            Ok(())
        })
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.has_structural_changes() || self.has_content_changes(cx)
    }
}

/// A search hit inside one cell's editor.
///
/// Cells own independent buffers, so a notebook-wide match has to carry the cell it
/// belongs to alongside the range the cell's own editor produced.
#[derive(Clone)]
pub struct NotebookSearchMatch {
    cell_index: usize,
    range: Range<editor::Anchor>,
}

impl EventEmitter<workspace::searchable::SearchEvent> for NotebookEditor {}

impl NotebookEditor {
    /// The editors that take part in search, in cell order. Raw cells have no
    /// editor and are skipped, so indices here are cell indices, not positions in
    /// the returned list.
    fn searchable_cell_editors(&self, cx: &App) -> Vec<(usize, Entity<Editor>)> {
        self.cell_order
            .iter()
            .enumerate()
            .filter_map(|(cell_index, cell_id)| {
                let cell = self.cell_map.get(cell_id)?;
                Some((cell_index, cell.editor(cx)?.clone()))
            })
            .collect()
    }

    fn cell_editor_at(&self, cell_index: usize, cx: &App) -> Option<Entity<Editor>> {
        let cell_id = self.cell_order.get(cell_index)?;
        Some(self.cell_map.get(cell_id)?.editor(cx)?.clone())
    }

    /// Regroups a flat match list back into the per-cell lists each cell editor
    /// needs in order to highlight its own buffer.
    fn matches_by_cell(
        matches: &[NotebookSearchMatch],
    ) -> HashMap<usize, Vec<Range<editor::Anchor>>> {
        let mut grouped: HashMap<usize, Vec<Range<editor::Anchor>>> = HashMap::default();
        for search_match in matches {
            grouped
                .entry(search_match.cell_index)
                .or_default()
                .push(search_match.range.clone());
        }
        grouped
    }
}

impl workspace::searchable::SearchableItem for NotebookEditor {
    type Match = NotebookSearchMatch;

    fn supported_options(&self) -> workspace::searchable::SearchOptions {
        workspace::searchable::SearchOptions {
            case: true,
            word: true,
            regex: true,
            replacement: true,
            // Cells are separate buffers, so there is no single selection or range
            // to restrict a search to.
            selection: false,
            select_all: false,
            find_in_results: false,
        }
    }

    fn clear_matches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for (_, editor) in self.searchable_cell_editors(cx) {
            editor.update(cx, |editor, cx| {
                workspace::searchable::SearchableItem::clear_matches(editor, window, cx)
            });
        }
    }

    fn update_matches(
        &mut self,
        matches: &[Self::Match],
        active_match_index: Option<usize>,
        token: workspace::searchable::SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let grouped = Self::matches_by_cell(matches);
        let active = active_match_index.and_then(|index| matches.get(index));

        for (cell_index, editor) in self.searchable_cell_editors(cx) {
            let cell_matches = grouped.get(&cell_index).cloned().unwrap_or_default();

            // Only the cell holding the active match gets an active index, so a
            // single hit is highlighted across the whole notebook.
            let active_in_cell = active.and_then(|active| {
                (active.cell_index == cell_index).then(|| {
                    cell_matches
                        .iter()
                        .position(|range| *range == active.range)
                        .unwrap_or(0)
                })
            });

            editor.update(cx, |editor, cx| {
                workspace::searchable::SearchableItem::update_matches(
                    editor,
                    &cell_matches,
                    active_in_cell,
                    token,
                    window,
                    cx,
                )
            });
        }
    }

    fn query_suggestion(
        &mut self,
        seed_query_override: Option<settings::SeedQuerySetting>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> String {
        let Some(editor) = self.cell_editor_at(self.selected_cell_index, cx) else {
            return String::new();
        };
        editor.update(cx, |editor, cx| {
            workspace::searchable::SearchableItem::query_suggestion(
                editor,
                seed_query_override,
                window,
                cx,
            )
        })
    }

    fn activate_match(
        &mut self,
        index: usize,
        matches: &[Self::Match],
        token: workspace::searchable::SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(search_match) = matches.get(index) else {
            return;
        };
        let cell_index = search_match.cell_index;
        let Some(editor) = self.cell_editor_at(cell_index, cx) else {
            return;
        };

        // Bring the cell on screen before activating within it, otherwise the
        // editor scrolls a cell the user cannot see.
        self.set_selected_index(cell_index, true, window, cx);

        let cell_matches = Self::matches_by_cell(matches)
            .remove(&cell_index)
            .unwrap_or_default();
        let index_in_cell = cell_matches
            .iter()
            .position(|range| *range == search_match.range)
            .unwrap_or(0);

        editor.update(cx, |editor, cx| {
            workspace::searchable::SearchableItem::activate_match(
                editor,
                index_in_cell,
                &cell_matches,
                token,
                window,
                cx,
            )
        });
        cx.notify();
    }

    fn select_matches(
        &mut self,
        matches: &[Self::Match],
        token: workspace::searchable::SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let grouped = Self::matches_by_cell(matches);
        for (cell_index, editor) in self.searchable_cell_editors(cx) {
            let Some(cell_matches) = grouped.get(&cell_index) else {
                continue;
            };
            editor.update(cx, |editor, cx| {
                workspace::searchable::SearchableItem::select_matches(
                    editor,
                    cell_matches,
                    token,
                    window,
                    cx,
                )
            });
        }
    }

    fn replace(
        &mut self,
        search_match: &Self::Match,
        query: &project::search::SearchQuery,
        token: workspace::searchable::SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.cell_editor_at(search_match.cell_index, cx) else {
            return;
        };
        editor.update(cx, |editor, cx| {
            workspace::searchable::SearchableItem::replace(
                editor,
                &search_match.range,
                query,
                token,
                window,
                cx,
            )
        });
    }

    fn find_matches(
        &mut self,
        query: Arc<project::search::SearchQuery>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Vec<Self::Match>> {
        let searches = self
            .searchable_cell_editors(cx)
            .into_iter()
            .map(|(cell_index, editor)| {
                let task = editor.update(cx, |editor, cx| {
                    workspace::searchable::SearchableItem::find_matches(
                        editor,
                        query.clone(),
                        window,
                        cx,
                    )
                });
                (cell_index, task)
            })
            .collect::<Vec<_>>();

        cx.background_spawn(async move {
            let mut matches = Vec::new();
            // Awaited in cell order so the flat match list reads top to bottom,
            // which is what next/previous navigation relies on.
            for (cell_index, task) in searches {
                matches.extend(task.await.into_iter().map(|range| NotebookSearchMatch {
                    cell_index,
                    range,
                }));
            }
            matches
        })
    }

    fn active_match_index(
        &mut self,
        direction: workspace::searchable::Direction,
        matches: &[Self::Match],
        token: workspace::searchable::SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let selected_cell_index = self.selected_cell_index;
        let editor = self.cell_editor_at(selected_cell_index, cx)?;

        let cell_matches = Self::matches_by_cell(matches).remove(&selected_cell_index);

        // With no hit in the selected cell, fall back to the nearest cell that has
        // one in the direction of travel.
        let Some(cell_matches) = cell_matches else {
            return match direction {
                workspace::searchable::Direction::Next => matches
                    .iter()
                    .position(|search_match| search_match.cell_index > selected_cell_index)
                    .or(if matches.is_empty() { None } else { Some(0) }),
                workspace::searchable::Direction::Prev => matches
                    .iter()
                    .rposition(|search_match| search_match.cell_index < selected_cell_index)
                    .or_else(|| matches.len().checked_sub(1)),
            };
        };

        let index_in_cell = editor.update(cx, |editor, cx| {
            workspace::searchable::SearchableItem::active_match_index(
                editor,
                direction,
                &cell_matches,
                token,
                window,
                cx,
            )
        })?;

        let range = cell_matches.get(index_in_cell)?;
        matches.iter().position(|search_match| {
            search_match.cell_index == selected_cell_index && search_match.range == *range
        })
    }
}

impl ProjectItem for NotebookEditor {
    type Item = NotebookItem;

    fn for_project_item(
        project: Entity<Project>,
        _pane: Option<&Pane>,
        item: Entity<Self::Item>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(project, item, window, cx)
    }
}

impl KernelSession for NotebookEditor {
    fn route(&mut self, message: &JupyterMessage, window: &mut Window, cx: &mut Context<Self>) {
        // Handle kernel status updates (these are broadcast to all)
        if let JupyterMessageContent::Status(status) = &message.content {
            self.kernel.set_execution_state(&status.execution_state);
            cx.notify();
        }

        if let JupyterMessageContent::KernelInfoReply(reply) = &message.content {
            self.kernel.set_kernel_info(reply);

            // A kernel that answers `kernel_info` is up and idle. Its first idle status
            // goes out on iopub, which we can miss if the kernel publishes it before our
            // subscription is live, leaving the indicator stuck on "Starting" until some
            // later execution produced fresh status messages.
            if matches!(KernelStatus::from(&self.kernel), KernelStatus::Starting) {
                self.kernel
                    .set_execution_state(&runtimelib::ExecutionState::Idle);
            }

            if let Ok(language_info) = serde_json::from_value::<nbformat::v4::LanguageInfo>(
                serde_json::to_value(&reply.language_info).unwrap(),
            ) {
                self.notebook_item.update(cx, |item, cx| {
                    item.notebook.metadata.language_info = Some(language_info);
                    cx.emit(());
                });
            }
            cx.notify();
        }

        // Handle cell-specific messages
        if let Some(parent_header) = &message.parent_header {
            if let Some(cell_id) = self.execution_requests.get(&parent_header.msg_id) {
                if let Some(Cell::Code(cell)) = self.cell_map.get(cell_id) {
                    cell.update(cx, |cell, cx| {
                        cell.handle_message(message, window, cx);
                    });
                }
            }
        }
    }

    fn kernel_errored(&mut self, error_message: String, cx: &mut Context<Self>) {
        self.kernel = Kernel::ErroredLaunch(error_message);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, Project, ProjectItem as _};
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;
    use util::rel_path::rel_path;

    const NOTEBOOK_WITH_ONE_CODE_CELL: &str = r#"{
        "metadata": {
            "kernelspec": {
                "display_name": "Python 3",
                "language": "python",
                "name": "python3"
            },
            "language_info": {
                "name": "python"
            }
        },
        "nbformat": 4,
        "nbformat_minor": 5,
        "cells": [
            {
                "cell_type": "code",
                "id": "cell-one",
                "metadata": {},
                "execution_count": null,
                "outputs": [],
                "source": ["print('hello')"]
            }
        ]
    }"#;


    /// Opens `contents` as a notebook in an empty window and returns the editor.
    ///
    /// No kernelspec is selected, so the editor's launch attempt is a no-op; these
    /// tests exercise parsing and serialization, not execution.
    async fn open_notebook<'a>(
        contents: &str,
        cx: &'a mut TestAppContext,
    ) -> (Entity<NotebookEditor>, &'a mut gpui::VisualTestContext) {
        open_notebook_with(contents, |_| {}, cx).await
    }

    async fn open_notebook_with<'a>(
        contents: &str,
        configure: impl FnOnce(&mut settings::SettingsContent),
        cx: &'a mut TestAppContext,
    ) -> (Entity<NotebookEditor>, &'a mut gpui::VisualTestContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            cx.update_global::<SettingsStore, _>(|settings_store, cx| {
                settings_store.update_user_settings(cx, |settings| {
                    // Cell editors blink their cursors on a repeating timer, which the
                    // deterministic test executor never finishes advancing, so a
                    // rendered notebook would never park.
                    settings.editor.cursor_blink = Some(false);
                    configure(settings);
                });
            });
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/notebooks"), json!({ "test.ipynb": contents }))
            .await;

        let project = Project::test(fs.clone(), [path!("/notebooks").as_ref()], cx).await;
        cx.update(|cx| {
            ReplStore::init(fs.clone(), cx);
            // Discovery probes real interpreters, which makes painting the kernel
            // selector non-deterministic. These tests are about the notebook, not about
            // which kernels this machine happens to have.
            ReplStore::global(cx).update(cx, |store, _| store.disable_discovery());
        });

        let worktree_id = project.read_with(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        });

        let notebook_item = cx
            .update(|cx| {
                NotebookItem::try_open(
                    &project,
                    &ProjectPath {
                        worktree_id,
                        path: rel_path("test.ipynb").into(),
                    },
                    cx,
                )
                .expect("ipynb files should be openable as notebooks")
            })
            .await
            .expect("notebook should parse");

        // Rendering the notebook animates the kernel status icon, which schedules a
        // frame on every render and makes `run_until_parked` spin forever.
        let cx = cx.add_empty_window();
        let editor = cx.update(|window, cx| {
            cx.new(|cx| NotebookEditor::new(project.clone(), notebook_item, window, cx))
        });

        (editor, cx)
    }


    #[gpui::test]
    async fn test_reloads_when_the_file_changes_on_disk(cx: &mut TestAppContext) {
        let (notebook, cx) = open_notebook(NOTEBOOK_WITH_ONE_CODE_CELL, cx).await;
        notebook.read_with(cx, |notebook, _| {
            assert_eq!(notebook.cell_order.len(), 1);
        });

        // Stands in for discarding the file in the git panel, which rewrites it on disk
        // without going through the editor.
        let fs = notebook.read_with(cx, |notebook, cx| notebook.project.read(cx).fs().clone());
        fs.save(
            path!("/notebooks/test.ipynb").as_ref(),
            &NOTEBOOK_WITH_TWO_CODE_CELLS.into(),
            Default::default(),
        )
        .await
        .unwrap();
        cx.run_until_parked();

        notebook.read_with(cx, |notebook, _| {
            assert_eq!(
                notebook.cell_order.len(),
                2,
                "the open notebook should follow the file on disk"
            );
        });
    }




    #[gpui::test]
    async fn test_saving_after_an_external_change_does_not_restore_the_old_file(
        cx: &mut TestAppContext,
    ) {
        let (notebook, cx) = open_notebook(NOTEBOOK_WITH_ONE_CODE_CELL, cx).await;

        // Stands in for discarding in the git panel: the file on disk gains a cell and
        // names a different kernel than the one the editor already parsed.
        let mut external: serde_json::Value =
            serde_json::from_str(NOTEBOOK_WITH_TWO_CODE_CELLS).unwrap();
        external["metadata"]["kernelspec"]["name"] = "restored-kernel".into();
        external["metadata"]["kernelspec"]["display_name"] = "Restored Kernel".into();
        let external = serde_json::to_string_pretty(&external).unwrap();

        let fs = notebook.read_with(cx, |notebook, cx| notebook.project.read(cx).fs().clone());
        fs.save(
            path!("/notebooks/test.ipynb").as_ref(),
            &external.as_str().into(),
            Default::default(),
        )
        .await
        .unwrap();
        cx.run_until_parked();

        let project = notebook.read_with(cx, |notebook, _| notebook.project.clone());
        notebook
            .update_in(cx, |notebook, _window, cx| {
                notebook.save_impl(SaveDestination::CurrentPath, project, cx)
            })
            .await
            .unwrap();

        let written = fs
            .load(path!("/notebooks/test.ipynb").as_ref())
            .await
            .unwrap();
        let written: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(
            written["metadata"]["kernelspec"]["name"], "restored-kernel",
            "saving after an external change must not write back the pre-reload metadata"
        );
        assert_eq!(
            written["cells"].as_array().map(|cells| cells.len()),
            Some(2),
            "saving must not write back the pre-reload cells"
        );
    }


    #[gpui::test]
    async fn test_clear_outputs_on_save_strips_error_outputs(cx: &mut TestAppContext) {
        let mut source: serde_json::Value =
            serde_json::from_str(NOTEBOOK_WITH_ONE_CODE_CELL).unwrap();
        source["cells"][0]["outputs"] = serde_json::json!([{
            "output_type": "error",
            "ename": "NameError",
            "evalue": "name 'message' is not defined",
            "traceback": ["NameError: name 'message' is not defined"],
        }]);
        let source = serde_json::to_string(&source).unwrap();

        let (editor, cx) = open_notebook_with(
            &source,
            |settings| {
                settings
                    .editor
                    .jupyter
                    .get_or_insert_default()
                    .clear_outputs_on_save = Some(true);
            },
            cx,
        )
        .await;

        let stripped = cx.update(|_window, cx| editor.read(cx).to_notebook(cx));
        match stripped.cells.first() {
            Some(nbformat::v4::Cell::Code { outputs, .. }) => {
                assert!(
                    outputs.is_empty(),
                    "an error is still an output and must be stripped: {outputs:?}"
                );
            }
            other => panic!("expected a code cell, got {other:?}"),
        }
    }

    const NOTEBOOK_WITH_TWO_CODE_CELLS: &str = r#"{
        "metadata": {
            "kernelspec": {
                "display_name": "Python 3",
                "language": "python",
                "name": "python3"
            },
            "language_info": {
                "name": "python"
            }
        },
        "nbformat": 4,
        "nbformat_minor": 5,
        "cells": [
            {
                "cell_type": "code",
                "id": "cell-one",
                "metadata": {},
                "execution_count": null,
                "outputs": [],
                "source": ["import pandas as pd"]
            },
            {
                "cell_type": "code",
                "id": "cell-two",
                "metadata": {},
                "execution_count": null,
                "outputs": [],
                "source": ["pd.DataFrame()"]
            }
        ]
    }"#;

    const NOTEBOOK_WITH_HEADINGS: &str = r###"{
        "metadata": {"kernelspec": {"display_name": "Python 3", "language": "python", "name": "python3"},
                     "language_info": {"name": "python"}},
        "nbformat": 4,
        "nbformat_minor": 5,
        "cells": [
            {"cell_type": "markdown", "id": "md-intro", "metadata": {},
             "source": ["# Loading the data\n", "\n", "Some prose"]},
            {"cell_type": "code", "id": "code-load", "metadata": {}, "execution_count": null,
             "outputs": [], "source": ["def load(path):\n", "    return path"]},
            {"cell_type": "code", "id": "code-blank", "metadata": {}, "execution_count": null,
             "outputs": [], "source": [""]},
            {"cell_type": "markdown", "id": "md-results", "metadata": {},
             "source": ["## Results"]}
        ]
    }"###;

    /// Exercises the shapes that used to be dropped on save: a PNG output (the
    /// editor renders it but used to serialize nothing), a `text/plain` sibling in
    /// the same bundle, a media type the editor cannot render at all, and markdown
    /// attachments. Sources deliberately cover a trailing newline, no trailing
    /// newline, and an embedded blank line.
    const NOTEBOOK_WITH_RICH_OUTPUTS: &str = r##"{
 "metadata": {
  "kernelspec": {
   "display_name": "Python 3",
   "language": "python",
   "name": "python3"
  },
  "language_info": {
   "name": "python"
  }
 },
 "nbformat": 4,
 "nbformat_minor": 5,
 "cells": [
  {
   "cell_type": "code",
   "id": "cell-plot",
   "metadata": {},
   "execution_count": 3,
   "outputs": [
    {
     "output_type": "execute_result",
     "execution_count": 3,
     "data": {
      "image/png": "iVBORw0KGgo=",
      "text/plain": [
       "<Figure size 640x480>"
      ]
     },
     "metadata": {}
    },
    {
     "output_type": "display_data",
     "data": {
      "application/vnd.jupyter.widget-view+json": {
       "model_id": "abc123",
       "version_major": 2,
       "version_minor": 0
      }
     },
     "metadata": {}
    },
    {
     "output_type": "stream",
     "name": "stderr",
     "text": [
      "a warning\n"
     ]
    }
   ],
   "source": [
    "import x\n",
    "\n",
    "x.plot()"
   ]
  },
  {
   "cell_type": "markdown",
   "id": "cell-notes",
   "metadata": {},
   "attachments": {
    "diagram.png": {
     "image/png": "iVBORw0KGgo="
    }
   },
   "source": [
    "# Notes\n",
    "\n",
    "See ![](attachment:diagram.png)\n"
   ]
  },
  {
   "cell_type": "raw",
   "id": "cell-raw",
   "metadata": {},
   "source": [
    "raw line one\n",
    "raw line two"
   ]
  }
 ]
}
"##;

    /// When the configured interpreter doesn't exist (e.g. Python isn't installed),
    /// running a cell must not leave it stuck in the executing state. It should
    /// instead surface the kernel launch error as an error output on the cell.
    #[gpui::test]
    async fn test_run_cell_with_missing_interpreter_shows_error(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/notebooks"),
            json!({ "test.ipynb": NOTEBOOK_WITH_ONE_CODE_CELL }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/notebooks").as_ref()], cx).await;
        cx.update(|cx| {
            ReplStore::init(fs.clone(), cx);
            // Kernel discovery probes real interpreters. The launch now waits for the
            // notebook's language before choosing a kernel, so it reaches discovery in
            // these setups too and trips the scheduler's non-determinism check.
            ReplStore::global(cx).update(cx, |store, _| store.disable_discovery());
        });

        let worktree_id = project.read_with(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        });

        // Select a kernel whose interpreter doesn't exist, simulating a machine
        // where Python isn't installed properly. This is the same path the
        // kernel picker uses.
        let missing_interpreter = path!("/nonexistent/python3");
        let broken_spec = KernelSpecification::Jupyter(LocalKernelSpecification {
            name: "python3".to_string(),
            path: PathBuf::from(missing_interpreter),
            kernelspec: JupyterKernelspec {
                argv: vec![
                    missing_interpreter.to_string(),
                    "-m".to_string(),
                    "ipykernel_launcher".to_string(),
                    "-f".to_string(),
                    "{connection_file}".to_string(),
                ],
                display_name: "Python 3".to_string(),
                language: "python".to_string(),
                interrupt_mode: None,
                metadata: None,
                env: None,
            },
        });
        cx.update(|cx| {
            ReplStore::global(cx).update(cx, |store, cx| {
                store.set_active_kernelspec(worktree_id, broken_spec, cx);
            })
        });

        let notebook_item = cx
            .update(|cx| {
                NotebookItem::try_open(
                    &project,
                    &ProjectPath {
                        worktree_id,
                        path: rel_path("test.ipynb").into(),
                    },
                    cx,
                )
                .expect("ipynb files should be openable as notebooks")
            })
            .await
            .expect("notebook should parse");

        // Don't render the notebook UI itself: its animated kernel status icon
        // schedules a new frame on every render, which makes `run_until_parked`
        // spin forever in tests. The editor entity is created inside an empty
        // window instead; we are testing execution behavior, not rendering.
        let cx = cx.add_empty_window();

        // Launching a kernel probes real TCP ports on localhost, which the
        // deterministic test scheduler cannot drive.
        cx.executor().allow_parking();

        let editor = cx.update(|window, cx| {
            cx.new(|cx| NotebookEditor::new(project.clone(), notebook_item, window, cx))
        });

        // The launch waits for the notebook's language to resolve before choosing a
        // kernel, so driving to quiescence runs the whole attempt: pick a spec, spawn
        // the interpreter, and fail because it does not exist.
        cx.run_until_parked();

        editor.read_with(cx, |editor, _| {
            assert!(
                matches!(editor.kernel, Kernel::ErroredLaunch(_)),
                "kernel launch should fail, instead status is: {}",
                editor.kernel.status().to_string()
            );
        });

        // Run the (only) cell via the production action handler.
        editor.update_in(cx, |editor, window, cx| {
            editor.run_current_cell(&Run, window, cx);
        });

        editor.read_with(cx, |editor, cx| {
            let cell_id = editor.cell_order.first().expect("notebook has one cell");
            let Some(Cell::Code(cell)) = editor.cell_map.get(cell_id) else {
                panic!("expected a code cell");
            };
            let cell = cell.read(cx);

            assert!(
                !cell.is_executing(),
                "cell must not be stuck in the executing state when the kernel is not running"
            );

            let nbformat::v4::Cell::Code { outputs, .. } = cell.to_nbformat_cell(cx) else {
                panic!("expected a code cell");
            };
            match outputs.as_slice() {
                [nbformat::v4::Output::Error(error)] => {
                    assert_eq!(error.ename, "Kernel Error");
                    let traceback = error.traceback.join("\n");
                    assert!(
                        traceback.contains("the kernel failed to launch"),
                        "error output should explain why the cell could not run, got: {traceback}"
                    );
                }
                other => panic!("expected a single error output, got: {other:?}"),
            }
        });
    }

    /// Opening a notebook as a single file (its own worktree) leaves the
    /// worktree-relative path empty, so only the absolute path carries the
    /// `.ipynb` extension. `try_open` must still recognize it as a notebook.
    #[gpui::test]
    async fn test_open_single_file_notebook(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/notebooks"),
            json!({ "single.ipynb": NOTEBOOK_WITH_ONE_CODE_CELL }),
        )
        .await;

        let project =
            Project::test(fs.clone(), [path!("/notebooks/single.ipynb").as_ref()], cx).await;
        cx.update(|cx| {
            ReplStore::init(fs.clone(), cx);
            // Kernel discovery probes real interpreters. The launch now waits for the
            // notebook's language before choosing a kernel, so it reaches discovery in
            // these setups too and trips the scheduler's non-determinism check.
            ReplStore::global(cx).update(cx, |store, _| store.disable_discovery());
        });

        let project_path = project.read_with(cx, |project, cx| {
            let worktree = project.worktrees(cx).next().unwrap();
            let worktree = worktree.read(cx);
            assert!(
                worktree.is_single_file(),
                "opening a bare .ipynb should create a single-file worktree"
            );
            ProjectPath {
                worktree_id: worktree.id(),
                path: worktree.root_entry().unwrap().path.clone(),
            }
        });

        assert!(
            project_path.path.extension().is_none(),
            "single-file worktree relative path should have no extension"
        );

        let notebook_item = cx
            .update(|cx| {
                NotebookItem::try_open(&project, &project_path, cx)
                    .expect("single-file .ipynb should open as a notebook")
            })
            .await
            .expect("notebook should parse");

        notebook_item.read_with(cx, |item, _| {
            assert_eq!(item.notebook.cells.len(), 1);
        });
    }

    /// Notebooks must be saved through the project rather than through the
    /// client's own filesystem, otherwise a remote notebook's path is resolved
    /// against the local machine and the save fails.
    #[gpui::test]
    async fn test_save_goes_through_the_project(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/notebooks"),
            json!({ "test.ipynb": NOTEBOOK_WITH_ONE_CODE_CELL }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/notebooks").as_ref()], cx).await;
        cx.update(|cx| {
            ReplStore::init(fs.clone(), cx);
            // Kernel discovery probes real interpreters. The launch now waits for the
            // notebook's language before choosing a kernel, so it reaches discovery in
            // these setups too and trips the scheduler's non-determinism check.
            ReplStore::global(cx).update(cx, |store, _| store.disable_discovery());
        });

        let project_path = project.read_with(cx, |project, cx| ProjectPath {
            worktree_id: project.worktrees(cx).next().unwrap().read(cx).id(),
            path: rel_path("test.ipynb").into(),
        });

        let notebook_item = cx
            .update(|cx| {
                NotebookItem::try_open(&project, &project_path, cx)
                    .expect("ipynb files should be openable as notebooks")
            })
            .await
            .expect("notebook should parse");

        // Held across the save: a save that bypasses the project writes the file
        // behind this buffer's back, leaving it stale.
        let buffer = project
            .update(cx, |project, cx| {
                project.open_buffer(project_path.clone(), cx)
            })
            .await
            .expect("notebook buffer should open");

        // Rendering the notebook animates the kernel status icon, which makes
        // `run_until_parked` spin forever; only the editor entity is needed here.
        let cx = cx.add_empty_window();
        let notebook_editor = cx.update(|window, cx| {
            cx.new(|cx| NotebookEditor::new(project.clone(), notebook_item, window, cx))
        });

        let cell_editor = notebook_editor.read_with(cx, |notebook_editor, cx| {
            let cell_id = notebook_editor
                .cell_order
                .first()
                .expect("notebook has one cell");
            let Some(Cell::Code(cell)) = notebook_editor.cell_map.get(cell_id) else {
                panic!("expected a code cell");
            };
            cell.read(cx).editor().clone()
        });
        // Cell editors are excerpts over the notebook's shared buffer, so they are not
        // singletons and `set_text` would assert. This is the path typing takes.
        cell_editor.update_in(cx, |cell_editor, _window, cx| {
            let end = cell_editor.buffer().read(cx).read(cx).max_point();
            cell_editor.buffer().update(cx, |buffer, cx| {
                buffer.edit(
                    [(
                        multi_buffer::MultiBufferPoint::zero()..end,
                        "print('goodbye')",
                    )],
                    None,
                    cx,
                );
            });
        });

        notebook_editor
            .update_in(cx, |notebook_editor, window, cx| {
                notebook_editor.save(SaveOptions::default(), project.clone(), window, cx)
            })
            .await
            .expect("saving the notebook should succeed");

        let saved = String::from_utf8(
            fs.read_file_sync(path!("/notebooks/test.ipynb"))
                .expect("notebook should still exist"),
        )
        .expect("notebook should be valid UTF-8");
        assert!(
            saved.contains("print('goodbye')"),
            "the edited cell should be written to the notebook, got: {saved}"
        );

        buffer.read_with(cx, |buffer, _| {
            assert_eq!(
                buffer.text(),
                saved,
                "the project's buffer should hold the saved notebook"
            );
            assert!(!buffer.is_dirty(), "saving should leave the buffer clean");
        });
    }

    /// Opening a notebook and saving it without edits must reproduce the file.
    /// `str::lines` used to drop the distinction between a source line that ends in
    /// a newline and one that doesn't, so every cell was rewritten on the first
    /// save and the whole file showed up in `git diff`.
    #[gpui::test]
    async fn test_round_trip_preserves_sources_and_outputs(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_RICH_OUTPUTS, cx).await;

        let original: nbformat::v4::Notebook =
            match nbformat::parse_notebook(NOTEBOOK_WITH_RICH_OUTPUTS) {
                Ok(nbformat::Notebook::V4(notebook)) => notebook,
                other => panic!("fixture should parse as nbformat v4, got {other:?}"),
            };
        let saved = cx.update(|_window, cx| editor.read(cx).to_notebook(cx));

        assert_eq!(
            saved.cells.len(),
            original.cells.len(),
            "cell count should be unchanged"
        );

        for (saved_cell, original_cell) in saved.cells.iter().zip(original.cells.iter()) {
            assert_eq!(
                serde_json::to_value(saved_cell).expect("saved cell should serialize"),
                serde_json::to_value(original_cell).expect("original cell should serialize"),
                "cell {:?} changed on save",
                original_cell.id()
            );
        }
    }

    /// The editor displays one representation out of a MIME bundle. Saving used to
    /// rebuild the bundle from the rendered view, which discarded every other
    /// representation and dropped image, table, markdown and JSON outputs entirely.
    #[gpui::test]
    async fn test_round_trip_preserves_unrendered_media_types(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_RICH_OUTPUTS, cx).await;

        let saved = cx.update(|_window, cx| editor.read(cx).to_notebook(cx));
        let outputs = match saved.cells.first() {
            Some(nbformat::v4::Cell::Code { outputs, .. }) => outputs,
            other => panic!("first cell should be a code cell, got {other:?}"),
        };

        assert_eq!(outputs.len(), 3, "no output should be dropped");

        let execute_result = match &outputs[0] {
            nbformat::v4::Output::ExecuteResult(result) => result,
            other => panic!("expected an execute_result, got {other:?}"),
        };
        let data = serde_json::to_value(&execute_result.data).expect("media should serialize");
        assert!(
            data.get("image/png").is_some(),
            "the rendered representation should survive: {data}"
        );
        assert!(
            data.get("text/plain").is_some(),
            "the unrendered sibling representation should survive too: {data}"
        );

        let widget = match &outputs[1] {
            nbformat::v4::Output::DisplayData(display_data) => {
                serde_json::to_value(&display_data.data).expect("media should serialize")
            }
            other => panic!("expected display_data, got {other:?}"),
        };
        assert!(
            widget
                .get("application/vnd.jupyter.widget-view+json")
                .is_some(),
            "a media type the editor cannot render must still round-trip: {widget}"
        );

        match &outputs[2] {
            nbformat::v4::Output::Stream { name, text } => {
                assert_eq!(name, "stderr", "stream name should not be forced to stdout");
                assert_eq!(text.0, "a warning\n");
            }
            other => panic!("expected a stream output, got {other:?}"),
        }
    }

    /// Attachments hold the image data for `attachment:` URLs in a markdown cell.
    /// They were parsed and then written back as `None`, so saving broke every
    /// embedded image.
    #[gpui::test]
    async fn test_round_trip_preserves_markdown_attachments(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_RICH_OUTPUTS, cx).await;

        let saved = cx.update(|_window, cx| editor.read(cx).to_notebook(cx));
        let attachments = match saved.cells.get(1) {
            Some(nbformat::v4::Cell::Markdown { attachments, .. }) => attachments,
            other => panic!("second cell should be a markdown cell, got {other:?}"),
        };

        let attachments = attachments
            .as_ref()
            .expect("markdown attachments should survive a save");
        assert!(
            attachments.get("diagram.png").is_some(),
            "expected the attachment to be carried through: {attachments}"
        );
    }


    /// The reason the shared buffer exists. Before this, every cell built a detached
    /// `Buffer::local` with no project, so nothing in a cell could reach a language
    /// server: no completions, no diagnostics, no go-to-definition.
    #[gpui::test]
    async fn test_code_cell_editors_are_attached_to_the_project(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;

        cx.update(|_window, cx| {
            let notebook = editor.read(cx);
            for cell_id in &notebook.cell_order {
                let Some(Cell::Code(cell)) = notebook.cell_map.get(cell_id) else {
                    continue;
                };
                assert!(
                    cell.read(cx).editor().read(cx).project().is_some(),
                    "code cell {cell_id:?} should have a project attached"
                );
            }
        });
    }

    /// All code cells must excerpt the same buffer, otherwise a language server still
    /// sees each cell in isolation and a name defined in cell one is unresolved in
    /// cell two.
    #[gpui::test]
    async fn test_code_cells_share_one_buffer(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;

        let buffer_ids = cx.update(|_window, cx| {
            let notebook = editor.read(cx);
            notebook
                .cell_order
                .iter()
                .filter_map(|cell_id| match notebook.cell_map.get(cell_id) {
                    Some(Cell::Code(cell)) => {
                        let editor = cell.read(cx).editor().read(cx);
                        let buffers = editor.buffer().read(cx).all_buffers();
                        Some(
                            buffers
                                .into_iter()
                                .map(|buffer| buffer.entity_id())
                                .collect::<Vec<_>>(),
                        )
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        });

        assert_eq!(buffer_ids.len(), 2, "the fixture has two code cells");
        assert_eq!(
            buffer_ids[0], buffer_ids[1],
            "both cells should excerpt the same shared buffer"
        );

        let shared = cx.update(|_window, cx| editor.read(cx).shadow.buffer().read(cx).text());
        assert!(
            shared.contains("import pandas as pd") && shared.contains("pd.DataFrame()"),
            "the shared buffer should hold both cells in one scope:\n{shared}"
        );
    }

    /// Each cell editor must still show only its own cell.
    #[gpui::test]
    async fn test_each_cell_editor_shows_only_its_own_cell(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;

        let texts = cx.update(|_window, cx| {
            let notebook = editor.read(cx);
            notebook
                .cell_order
                .iter()
                .filter_map(|cell_id| match notebook.cell_map.get(cell_id) {
                    Some(Cell::Code(cell)) => Some(cell.read(cx).current_source(cx)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        });

        assert_eq!(
            texts,
            vec!["import pandas as pd".to_string(), "pd.DataFrame()".to_string()]
        );
    }

    /// Outputs survive a save by default. Teams that keep notebooks in git want the
    /// opposite, because inline image data makes a diff unreadable.
    #[gpui::test]
    async fn test_clear_outputs_on_save_strips_outputs(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_RICH_OUTPUTS, cx).await;

        let kept = cx.update(|_window, cx| editor.read(cx).to_notebook(cx));
        match kept.cells.first() {
            Some(nbformat::v4::Cell::Code {
                outputs,
                execution_count,
                ..
            }) => {
                assert_eq!(outputs.len(), 3, "outputs are kept by default");
                assert_eq!(*execution_count, Some(3));
            }
            other => panic!("expected a code cell, got {other:?}"),
        }

        cx.update(|_window, cx| {
            cx.update_global::<SettingsStore, _>(|settings_store, cx| {
                settings_store.update_user_settings(cx, |settings| {
                    settings
                        .editor
                        .jupyter
                        .get_or_insert_default()
                        .clear_outputs_on_save = Some(true);
                });
            });
        });

        let stripped = cx.update(|_window, cx| editor.read(cx).to_notebook(cx));
        match stripped.cells.first() {
            Some(nbformat::v4::Cell::Code {
                outputs,
                execution_count,
                source,
                ..
            }) => {
                assert!(outputs.is_empty(), "outputs should be stripped: {outputs:?}");
                assert_eq!(*execution_count, None, "execution count should be cleared");
                assert_eq!(
                    source.concat(),
                    "import x\n\nx.plot()",
                    "the source must not be touched"
                );
            }
            other => panic!("expected a code cell, got {other:?}"),
        }

        match stripped.cells.get(1) {
            Some(nbformat::v4::Cell::Markdown { attachments, .. }) => {
                assert!(
                    attachments.is_some(),
                    "markdown attachments are not outputs and should survive"
                );
            }
            other => panic!("expected a markdown cell, got {other:?}"),
        }
    }

    /// Reordering used to swap `cell_order` only, leaving the shared buffer and the
    /// rendered list showing the old order.
    #[gpui::test]
    async fn test_moving_a_cell_reorders_everything(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.set_selected_index(1, false, window, cx);
                editor.move_cell_up(window, cx);
            })
        });

        cx.update(|_window, cx| {
            let notebook = editor.read(cx);

            let order: Vec<String> = notebook
                .cell_order
                .iter()
                .map(|cell_id| cell_id.to_string())
                .collect();
            assert_eq!(
                order,
                vec!["cell-two".to_string(), "cell-one".to_string()],
                "the cell list should be reordered"
            );
            assert_eq!(notebook.selected_cell_index, 0, "selection follows the cell");

            let shared = notebook.shadow.buffer().read(cx).text();
            let second = shared.find("pd.DataFrame()").expect("cell two in buffer");
            let first = shared.find("import pandas as pd").expect("cell one in buffer");
            assert!(
                second < first,
                "the shared buffer should reflect the new order:\n{shared}"
            );
        });

        // Each editor must still show its own cell after the excerpt was re-pointed.
        let texts = cx.update(|_window, cx| {
            let notebook = editor.read(cx);
            notebook
                .cell_order
                .iter()
                .filter_map(|cell_id| match notebook.cell_map.get(cell_id) {
                    Some(Cell::Code(cell)) => Some(cell.read(cx).current_source(cx)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(
            texts,
            vec!["pd.DataFrame()".to_string(), "import pandas as pd".to_string()]
        );

        // And the notebook saves in the new order.
        let saved = cx.update(|_window, cx| editor.read(cx).to_notebook(cx));
        let sources: Vec<String> = saved
            .cells
            .iter()
            .map(|cell| match cell {
                nbformat::v4::Cell::Code { source, .. } => source.concat(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(
            sources,
            vec!["pd.DataFrame()".to_string(), "import pandas as pd".to_string()]
        );
    }

    /// Go-back has to return to the cell you were on. Entries store the cell id, not an
    /// index, so they survive the list changing underneath them.
    #[gpui::test]
    async fn test_navigating_back_restores_the_cell(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;

        let entry = cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                // Start on the first cell, then move to the second.
                editor.set_selected_index(1, false, window, cx);
                Arc::new(NotebookNavigationData {
                    cell_id: editor.cell_order[0].clone(),
                }) as Arc<dyn std::any::Any + Send>
            })
        });

        let navigated = cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                assert_eq!(editor.selected_cell_index, 1);
                Item::navigate(editor, entry.clone(), window, cx)
            })
        });

        assert!(navigated, "navigating to a known cell should be handled");
        cx.update(|_window, cx| {
            assert_eq!(editor.read(cx).selected_cell_index, 0);
        });

        // Re-navigating to the cell already selected is a no-op, so the workspace does
        // not record a redundant entry.
        let again = cx.update(|window, cx| {
            editor.update(cx, |editor, cx| Item::navigate(editor, entry, window, cx))
        });
        assert!(!again, "navigating to the current cell should not be handled");
    }

    /// An entry pointing at a deleted cell must be declined rather than jumping
    /// somewhere arbitrary.
    #[gpui::test]
    async fn test_navigating_to_a_deleted_cell_is_declined(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;

        let stale = Arc::new(NotebookNavigationData {
            cell_id: CellId::new("cell-gone").expect("valid id"),
        }) as Arc<dyn std::any::Any + Send>;

        let navigated = cx.update(|window, cx| {
            editor.update(cx, |editor, cx| Item::navigate(editor, stale, window, cx))
        });

        assert!(!navigated);
        cx.update(|_window, cx| {
            assert_eq!(
                editor.read(cx).selected_cell_index,
                0,
                "selection should not move"
            );
        });
    }

    /// With the sidecar off, the shared buffer has no file, so Zed will not register a
    /// language server for it. This is the state that gives cells one shared scope but
    /// no completions.
    #[gpui::test]
    async fn test_without_the_sidecar_the_shared_buffer_has_no_file(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;

        cx.update(|_window, cx| {
            assert!(
                !editor.read(cx).shadow.is_file_backed(cx),
                "an untitled buffer is never registered with a language server"
            );
        });
    }

    /// With the sidecar on, the projection moves onto a real file beside the notebook,
    /// which is the only way Zed's LSP layer will see the notebook's code. Every cell
    /// must still show its own text after the buffer is swapped underneath it.
    #[gpui::test]
    async fn test_sidecar_moves_the_projection_onto_a_file(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook_with(
            NOTEBOOK_WITH_TWO_CODE_CELLS,
            |settings| {
                settings
                    .editor
                    .jupyter
                    .get_or_insert_default()
                    .language_server_sidecar = Some(true);
            },
            cx,
        )
        .await;

        cx.run_until_parked();

        cx.update(|_window, cx| {
            let notebook = editor.read(cx);
            assert!(
                notebook.shadow.is_file_backed(cx),
                "the projection should have moved onto a file"
            );
            assert!(
                notebook.sidecar_path.is_some(),
                "the path is recorded so the file can be cleaned up on close"
            );

            let shared = notebook.shadow.buffer().read(cx).text();
            assert!(
                shared.contains("import pandas as pd") && shared.contains("pd.DataFrame()"),
                "the file-backed buffer should still hold both cells:\n{shared}"
            );
        });

        let texts = cx.update(|_window, cx| {
            let notebook = editor.read(cx);
            notebook
                .cell_order
                .iter()
                .filter_map(|cell_id| match notebook.cell_map.get(cell_id) {
                    Some(Cell::Code(cell)) => Some(cell.read(cx).current_source(cx)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(
            texts,
            vec!["import pandas as pd".to_string(), "pd.DataFrame()".to_string()],
            "cells must be re-excerpted against the new buffer"
        );
    }

    #[test]
    fn test_cell_label_prefers_headings_then_definitions() {
        assert_eq!(
            cell_label("## Loading the data\n\nSome prose"),
            Some("Loading the data".to_string()),
            "a markdown heading is how a notebook is organized"
        );
        assert_eq!(
            cell_label("import pandas\n\ndef load(path):\n    pass"),
            Some("def load(path)".to_string()),
            "a definition beats the first line"
        );
        assert_eq!(
            cell_label("frame.head()"),
            Some("frame.head()".to_string()),
            "otherwise the first non-empty line"
        );
        assert_eq!(cell_label("\n\n   \n"), None, "a blank cell has no label");
        assert_eq!(cell_label("#\n\nreal content"), Some("real content".to_string()),
            "an empty heading should not win");
    }

    #[test]
    fn test_cell_label_truncates() {
        let label = cell_label(&"x".repeat(200)).expect("should produce a label");
        assert!(
            label.chars().count() <= 60,
            "breadcrumbs should not be unbounded: {label}"
        );
        assert!(label.ends_with('\u{2026}'));
    }

    /// Breadcrumbs orient you inside a notebook, where "cell 7 of 40" is most of what
    /// you need and a plain editor cannot tell you.
    #[gpui::test]
    async fn test_breadcrumbs_show_position_within_the_notebook(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;

        let crumbs = cx.update(|_window, cx| {
            let (crumbs, _font) = editor.read(cx).breadcrumbs(cx).expect("should have crumbs");
            crumbs
                .into_iter()
                .map(|crumb| crumb.text.to_string())
                .collect::<Vec<_>>()
        });

        assert_eq!(
            crumbs,
            vec![
                "test.ipynb".to_string(),
                "Cell 1 of 2".to_string(),
                "import pandas as pd".to_string(),
            ]
        );

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.set_selected_index(1, false, window, cx)
            })
        });

        let crumbs = cx.update(|_window, cx| {
            let (crumbs, _font) = editor.read(cx).breadcrumbs(cx).expect("should have crumbs");
            crumbs
                .into_iter()
                .map(|crumb| crumb.text.to_string())
                .collect::<Vec<_>>()
        });
        assert_eq!(crumbs[1], "Cell 2 of 2");
        assert_eq!(crumbs[2], "pd.DataFrame()");
    }

    /// A notebook's outline is its cells, labelled the way a person would label them:
    /// markdown headings first, then definitions.
    #[gpui::test]
    async fn test_outline_lists_cells_with_useful_labels(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_HEADINGS, cx).await;

        let entries = cx.update(|_window, cx| editor.read(cx).outline_entries(cx));

        let labels: Vec<String> = entries.iter().map(|entry| entry.label.clone()).collect();
        assert_eq!(
            labels,
            vec![
                "Loading the data".to_string(),
                "def load(path)".to_string(),
                // An unlabelable cell still gets a row, or the outline would skip cells
                // and its numbering would stop matching the notebook.
                "Cell 3".to_string(),
                "Results".to_string(),
            ]
        );

        let indices: Vec<usize> = entries.iter().map(|entry| entry.index).collect();
        assert_eq!(indices, vec![0, 1, 2, 3], "indices must match cell positions");

        assert_eq!(entries[0].cell_type, CellType::Markdown);
        assert_eq!(entries[1].cell_type, CellType::Code);
    }

    /// Choosing an outline entry has to select and reveal that cell.
    #[gpui::test]
    async fn test_selecting_an_outline_entry_jumps_to_the_cell(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_HEADINGS, cx).await;

        let target = cx.update(|_window, cx| editor.read(cx).outline_entries(cx)[3].cell_id.clone());

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.select_cell_by_id_and_reveal(&target, window, cx)
            })
        });

        cx.update(|_window, cx| {
            assert_eq!(editor.read(cx).selected_cell_index, 3);
        });

        // A cell that no longer exists must not move the selection.
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                let gone = CellId::new("not-here").expect("valid id");
                editor.select_cell_by_id_and_reveal(&gone, window, cx);
            })
        });
        cx.update(|_window, cx| {
            assert_eq!(editor.read(cx).selected_cell_index, 3, "selection is unchanged");
        });
    }

    /// Both views are over the same buffer, so switching copies nothing and an edit
    /// made in one is already present in the other.
    #[gpui::test]
    async fn test_source_view_shows_the_notebook_as_a_script(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.toggle_source_view(&ToggleSourceView, window, cx)
            })
        });

        let text = cx.update(|_window, cx| {
            let notebook = editor.read(cx);
            assert_eq!(notebook.view_mode, NotebookViewMode::Source);
            notebook
                .source_editor
                .as_ref()
                .expect("source editor should exist")
                .read(cx)
                .text(cx)
        });

        assert_eq!(
            text,
            "# %%\nimport pandas as pd\n\n# %%\npd.DataFrame()\n",
            "the source view is the notebook in percent format"
        );
    }

    /// Editing a cell's text in the source view, without touching the markers, must not
    /// rebuild the cell list, so ids and outputs are untouched.
    #[gpui::test]
    async fn test_editing_source_without_structural_change_keeps_cells(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;

        let ids_before = cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.toggle_source_view(&ToggleSourceView, window, cx);
                editor
                    .cell_order
                    .iter()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>()
            })
        });

        // Change a cell's body, leaving the markers alone.
        cx.update(|_window, cx| {
            let buffer = editor.read(cx).shadow.buffer().clone();
            buffer.update(cx, |buffer, cx| {
                let text = buffer.text();
                let start = text.find("pd.DataFrame()").expect("cell two present");
                buffer.edit([(start..start + "pd.DataFrame()".len(), "pd.Series()")], None, cx);
            })
        });

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.toggle_source_view(&ToggleSourceView, window, cx)
            })
        });

        cx.update(|_window, cx| {
            let notebook = editor.read(cx);
            assert_eq!(notebook.view_mode, NotebookViewMode::Cells);
            let ids_after: Vec<String> =
                notebook.cell_order.iter().map(|id| id.to_string()).collect();
            assert_eq!(
                ids_after, ids_before,
                "cell ids must survive a non-structural edit, or outputs would be orphaned"
            );
        });

        let sources = cx.update(|_window, cx| {
            let notebook = editor.read(cx);
            notebook
                .cell_order
                .iter()
                .filter_map(|id| match notebook.cell_map.get(id) {
                    Some(Cell::Code(cell)) => Some(cell.read(cx).current_source(cx)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(
            sources,
            vec!["import pandas as pd".to_string(), "pd.Series()".to_string()]
        );
    }

    /// Adding a cell in the source view has to produce a real cell on the way back.
    #[gpui::test]
    async fn test_adding_a_cell_in_source_view_creates_one(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.toggle_source_view(&ToggleSourceView, window, cx)
            })
        });

        cx.update(|_window, cx| {
            let buffer = editor.read(cx).shadow.buffer().clone();
            buffer.update(cx, |buffer, cx| {
                let end = buffer.len();
                buffer.edit([(end..end, "\n# %%\nprint('new')\n")], None, cx);
            })
        });

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.toggle_source_view(&ToggleSourceView, window, cx)
            })
        });

        let sources = cx.update(|_window, cx| {
            let notebook = editor.read(cx);
            assert_eq!(notebook.cell_order.len(), 3, "a third cell should exist");
            notebook
                .cell_order
                .iter()
                .filter_map(|id| match notebook.cell_map.get(id) {
                    Some(Cell::Code(cell)) => Some(cell.read(cx).current_source(cx)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(
            sources,
            vec![
                "import pandas as pd".to_string(),
                "pd.DataFrame()".to_string(),
                "print('new')".to_string(),
            ]
        );
    }

    /// Renders the notebook through a real layout, prepaint and paint pass.
    ///
    /// These are the closest thing to opening the editor and looking at it. They will
    /// not tell you it looks right, but they fail on anything that panics while
    /// rendering, which is most of what "it is broken" means in a GPUI view, and they
    /// run without a person who knows Jupyter.
    /// Builds the notebook's element tree.
    ///
    /// This runs the body of `render` and every closure it calls to assemble the tree:
    /// breadcrumb and cell labels, the view-mode branch, per-cell rendering, the kernel
    /// status bar. It does **not** run layout or paint, because a rendered notebook
    /// never parks the deterministic test executor, so a full window draw hangs.
    ///
    /// So it catches panics and bad state in the render path, which is where this
    /// crate's logic lives, and not visual defects. Visual correctness still needs a
    /// person to look at it.
    fn build_notebook_elements(
        editor: &Entity<NotebookEditor>,
        cx: &mut gpui::VisualTestContext,
    ) {
        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                let _ = editor.render(window, cx).into_any_element();
            })
        });
    }

    /// Holds the notebook so it is drawn inside a real view context.
    struct NotebookTestRoot {
        notebook: Entity<NotebookEditor>,
    }

    impl Render for NotebookTestRoot {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.notebook.clone())
        }
    }

    /// Opens the notebook in a real window and drives full frames: layout, prepaint and
    /// paint, not just the render call.
    ///
    /// Ticks a bounded number of times rather than using `run_until_parked`, because a
    /// notebook schedules repeating work and parking never happens; `add_window_view`
    /// parks internally, so it cannot be used either. A bounded loop still runs every
    /// frame the window queues, which is what exercises layout and paint.
    /// Lays out and paints the notebook's content in a real window.
    ///
    /// This is a full frame: layout, prepaint and paint, not just the render call. It
    /// covers the cell list, the empty state and the cell controls.
    ///
    /// Paints the whole notebook, kernel status bar included.
    ///
    /// The status bar used to deadlock here. `KernelSelector` built a `Picker` entity on
    /// every frame, which spawned work to compute its matches and then dropped both when
    /// the frame ended; dropping a picker mid-flight blocks on the task it spawned. The
    /// picker is now built when the menu opens.
    fn paint_notebook(editor: &Entity<NotebookEditor>, cx: &mut TestAppContext) {
        let notebook = editor.clone();
        let window = cx.add_window(|_window, _cx| NotebookTestRoot { notebook });

        for _ in 0..64 {
            if !cx.executor().tick() {
                break;
            }
        }

        window
            .update(cx, |_root, _window, _cx| {})
            .expect("the window should survive painting the notebook");
    }


    #[gpui::test]
    async fn test_builds_elements_for_a_notebook(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;
        build_notebook_elements(&editor, cx);
        paint_notebook(&editor, &mut cx.cx);
    }

    /// Outputs are where rendering is most likely to break: an image, a media type the
    /// editor cannot render, a stream and markdown attachments in one notebook.
    #[gpui::test]
    async fn test_builds_elements_for_rich_outputs(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_RICH_OUTPUTS, cx).await;
        build_notebook_elements(&editor, cx);
        paint_notebook(&editor, &mut cx.cx);
    }

    #[gpui::test]
    async fn test_builds_elements_for_markdown_and_raw_cells(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_HEADINGS, cx).await;
        build_notebook_elements(&editor, cx);
        paint_notebook(&editor, &mut cx.cx);
    }

    /// The empty state, and the transition into it, which deletes every cell out from
    /// under the renderer.
    #[gpui::test]
    async fn test_builds_elements_for_while_cells_are_deleted(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;
        build_notebook_elements(&editor, cx);
        paint_notebook(&editor, &mut cx.cx);

        for _ in 0..2 {
            cx.update(|window, cx| {
                editor.update(cx, |editor, cx| editor.delete_cell(window, cx))
            });
            build_notebook_elements(&editor, cx);
        paint_notebook(&editor, &mut cx.cx);
        }

        cx.update(|_window, cx| {
            assert!(editor.read(cx).cell_order.is_empty(), "all cells deleted");
        });
        build_notebook_elements(&editor, cx);
        paint_notebook(&editor, &mut cx.cx);
    }

    #[gpui::test]
    async fn test_builds_elements_for_after_adding_a_cell(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.add_code_block(window, cx);
                editor.add_markdown_block(window, cx);
            })
        });
        build_notebook_elements(&editor, cx);
        paint_notebook(&editor, &mut cx.cx);
    }

    /// Reordering re-points an excerpt while the list is rendering, which is the change
    /// most likely to leave a stale range behind.
    #[gpui::test]
    async fn test_builds_elements_for_after_reordering(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_HEADINGS, cx).await;
        build_notebook_elements(&editor, cx);
        paint_notebook(&editor, &mut cx.cx);

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.set_selected_index(3, false, window, cx);
                editor.move_cell_up(window, cx);
                editor.move_cell_up(window, cx);
            })
        });
        build_notebook_elements(&editor, cx);
        paint_notebook(&editor, &mut cx.cx);
    }

    #[gpui::test]
    async fn test_builds_elements_for_the_source_view(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_HEADINGS, cx).await;

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.toggle_source_view(&ToggleSourceView, window, cx)
            })
        });
        build_notebook_elements(&editor, cx);
        paint_notebook(&editor, &mut cx.cx);

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.toggle_source_view(&ToggleSourceView, window, cx)
            })
        });
        build_notebook_elements(&editor, cx);
        paint_notebook(&editor, &mut cx.cx);
    }

    /// An empty notebook still has to render, including its add-cell affordance.
    #[gpui::test]
    async fn test_builds_elements_for_an_empty_notebook(cx: &mut TestAppContext) {
        const EMPTY: &str = r#"{
            "metadata": {"language_info": {"name": "python"}},
            "nbformat": 4, "nbformat_minor": 5, "cells": []
        }"#;

        let (editor, cx) = open_notebook(EMPTY, cx).await;
        build_notebook_elements(&editor, cx);
        paint_notebook(&editor, &mut cx.cx);
    }

    /// With the sidecar on, every cell is re-excerpted against a different buffer while
    /// the view is live.
    #[gpui::test]
    async fn test_builds_elements_for_after_the_sidecar_upgrade(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook_with(
            NOTEBOOK_WITH_TWO_CODE_CELLS,
            |settings| {
                settings
                    .editor
                    .jupyter
                    .get_or_insert_default()
                    .language_server_sidecar = Some(true);
            },
            cx,
        )
        .await;

        cx.run_until_parked();
        build_notebook_elements(&editor, cx);
        paint_notebook(&editor, &mut cx.cx);
    }








    /// The diff has to reach the cells: each cell editor's multibuffer must carry it,
    /// or hunks render nowhere.
    #[gpui::test]
    async fn test_notebook_diff_is_attached_to_cell_editors(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;

        // Stand in for the committed version: the same notebook with one cell changed.
        let committed = NOTEBOOK_WITH_TWO_CODE_CELLS.replace("pd.DataFrame()", "pd.Series()");
        let base = projection_of_notebook(&committed, "#").expect("should project");

        let diff = cx.update(|_window, cx| {
            editor.update(cx, |editor, cx| {
                let snapshot = editor.shadow.buffer().read(cx).text_snapshot();
                let diff = cx.new(|cx| BufferDiff::new(&snapshot, None, None, cx));
                editor.notebook_diff = Some(diff.clone());
                editor.attach_diff_to_cells(&diff, cx);
                diff
            })
        });

        cx.update(|_window, cx| {
            let notebook = editor.read(cx);
            for cell_id in &notebook.cell_order {
                let Some(Cell::Code(cell)) = notebook.cell_map.get(cell_id) else {
                    continue;
                };
                let multi_buffer = cell.read(cx).editor().read(cx).buffer().read(cx);
                assert!(
                    multi_buffer
                        .diff_for(notebook.shadow.buffer().read(cx).remote_id())
                        .is_some(),
                    "cell {cell_id:?} should carry the notebook diff"
                );
            }
        });

        // Computing against a different base must produce hunks.
        let buffer = cx.update(|_window, cx| editor.read(cx).shadow.buffer().clone());
        let task = cx.update(|_window, cx| {
            diff.update(cx, |diff, cx| {
                let snapshot = buffer.read(cx).text_snapshot();
                diff.set_base_text(Some(base.into()), snapshot, cx)
            })
        });
        task.await;

        cx.update(|_window, cx| {
            let snapshot = diff.read(cx).snapshot(cx);
            assert!(
                !snapshot.is_empty(),
                "a changed cell should produce at least one hunk"
            );
        });
    }

    /// The property the whole approach exists for, end to end: a notebook that only
    /// differs in its outputs produces no hunks at all.
    #[gpui::test]
    async fn test_output_only_changes_produce_no_hunks(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_RICH_OUTPUTS, cx).await;

        // Same notebook, different outputs and execution counts.
        let committed = NOTEBOOK_WITH_RICH_OUTPUTS
            .replace("\"execution_count\": 3", "\"execution_count\": 99")
            .replace("iVBORw0KGgo=", "ZZZZdifferentimagedataZZZZ=");
        let base = projection_of_notebook(&committed, "#").expect("should project");

        let buffer = cx.update(|_window, cx| editor.read(cx).shadow.buffer().clone());
        let diff = cx.update(|_window, cx| {
            let snapshot = buffer.read(cx).text_snapshot();
            cx.new(|cx| BufferDiff::new(&snapshot, None, None, cx))
        });

        let task = cx.update(|_window, cx| {
            diff.update(cx, |diff, cx| {
                let snapshot = buffer.read(cx).text_snapshot();
                diff.set_base_text(Some(base.into()), snapshot, cx)
            })
        });
        task.await;

        cx.update(|_window, cx| {
            let snapshot = diff.read(cx).snapshot(cx);
            assert!(
                snapshot.is_empty(),
                "re-running a notebook must not show up as a diff"
            );
        });
    }



    /// Picking an outline entry in the source view has to move the cursor there. It
    /// used to scroll the cell list, which is not rendered in that view, so nothing
    /// visible happened.
    #[gpui::test]
    async fn test_outline_selection_moves_the_source_cursor(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_HEADINGS, cx).await;

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.toggle_source_view(&ToggleSourceView, window, cx)
            })
        });

        // The second code cell, so a move is observable.
        let target = cx.update(|_window, cx| {
            let notebook = editor.read(cx);
            notebook
                .cell_order
                .iter()
                .filter(|id| matches!(notebook.cell_map.get(id), Some(Cell::Code(_))))
                .nth(1)
                .cloned()
        });
        let Some(target) = target else {
            panic!("fixture should have two code cells");
        };

        let before = cx.update(|_window, cx| {
            editor
                .read(cx)
                .source_editor
                .as_ref()
                .expect("source editor")
                .read(cx)
                .selections
                .newest_anchor()
                .head()
        });

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.select_cell_by_id_and_reveal(&target, window, cx)
            })
        });

        let after = cx.update(|_window, cx| {
            editor
                .read(cx)
                .source_editor
                .as_ref()
                .expect("source editor")
                .read(cx)
                .selections
                .newest_anchor()
                .head()
        });

        assert_ne!(
            before, after,
            "the cursor should have moved to the selected cell"
        );

        cx.update(|_window, cx| {
            let notebook = editor.read(cx);
            assert_eq!(
                notebook.cell_order[notebook.selected_cell_index], target,
                "and the selection should follow, so breadcrumbs track it"
            );
        });
    }

    /// Opening a notebook and saving it without edits must reproduce the file exactly.
    ///
    /// Not "equivalent JSON": the same bytes. Anything less means every notebook a
    /// person opens shows up in `git diff`, which makes the editor unusable in a repo.
    /// The earlier version of this test compared `serde_json::Value`s, which ignores key
    /// order, and so could not see the file being rewritten.
    #[gpui::test]
    async fn test_saving_an_unchanged_notebook_reproduces_the_file(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_RICH_OUTPUTS, cx).await;

        let template = cx.update(|_window, cx| {
            editor.read(cx).notebook_item.read(cx).original_json.clone()
        });
        let saved = cx
            .update(|_window, cx| {
                serialize_notebook(&editor.read(cx).to_notebook(cx), template.as_ref())
            })
            .expect("should serialize");

        assert_eq!(
            saved, NOTEBOOK_WITH_RICH_OUTPUTS,
            "saving an unchanged notebook must reproduce the file byte for byte"
        );
    }

    /// And it has to stay that way across repeated saves, since `HashMap` iteration is
    /// reseeded per process.
    #[gpui::test]
    async fn test_saving_is_byte_stable(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_RICH_OUTPUTS, cx).await;

        let template = cx.update(|_window, cx| {
            editor.read(cx).notebook_item.read(cx).original_json.clone()
        });
        let first = cx
            .update(|_window, cx| {
                serialize_notebook(&editor.read(cx).to_notebook(cx), template.as_ref())
            })
            .expect("should serialize");

        let reparsed = match nbformat::parse_notebook(&first) {
            Ok(nbformat::Notebook::V4(notebook)) => notebook,
            other => panic!("should round-trip as v4: {other:?}"),
        };
        let second = serialize_notebook(&reparsed, template.as_ref()).expect("should serialize");

        assert_eq!(first, second, "a second save must produce identical bytes");
    }


    /// Cell reordering must do nothing in the source view.
    ///
    /// The source view shows the notebook as one text buffer, and the move operates on
    /// the selected cell index, which has no relationship to where the cursor is. Left
    /// enabled it shuffled cells out from under the text being edited, emptied one and
    /// duplicated another, then autosave wrote the result to disk.
    #[gpui::test]
    async fn test_cell_moves_are_inert_in_the_source_view(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_HEADINGS, cx).await;

        let before = cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.set_selected_index(2, false, window, cx);
                editor.toggle_source_view(&ToggleSourceView, window, cx);
                editor
                    .cell_order
                    .iter()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>()
            })
        });

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.move_cell_up(window, cx);
                editor.move_cell_down(window, cx);
                editor.move_cell_down(window, cx);
            })
        });

        let after = cx.update(|_window, cx| {
            editor
                .read(cx)
                .cell_order
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
        });

        assert_eq!(
            before, after,
            "cell order must be untouched while the source view is showing"
        );
    }

    /// And they still work in the cell view, so the guard did not disable the feature.
    #[gpui::test]
    async fn test_cell_moves_still_work_in_the_cell_view(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_HEADINGS, cx).await;

        let (was_first, was_second) = cx.update(|_window, cx| {
            let order = &editor.read(cx).cell_order;
            (order[1].clone(), order[2].clone())
        });

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.set_selected_index(2, false, window, cx);
                editor.move_cell_up(window, cx);
            })
        });

        cx.update(|_window, cx| {
            let notebook = editor.read(cx);
            assert_eq!(
                notebook.cell_order[1], was_second,
                "the moved cell should have taken the earlier slot"
            );
            assert_eq!(
                notebook.cell_order[2], was_first,
                "and the one it passed should have taken its place"
            );
            assert_eq!(
                notebook.selected_cell_index, 1,
                "selection follows the cell that moved"
            );
        });
    }

    /// Saving while the source view is showing must not lose what was typed there.
    ///
    /// Cells are excerpts over the shared buffer, and saving reads the cells. Text typed
    /// between two markers belongs to no excerpt until the cell list is rebuilt, so
    /// without reconciling first it is simply not saved.
    #[gpui::test]
    async fn test_saving_from_the_source_view_keeps_new_text(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.toggle_source_view(&ToggleSourceView, window, cx)
            })
        });

        // Add a cell the way someone would in the source view: type a marker and code.
        cx.update(|_window, cx| {
            let buffer = editor.read(cx).shadow.buffer().clone();
            buffer.update(cx, |buffer, cx| {
                let end = buffer.len();
                buffer.edit([(end..end, "\n# %%\nprint('typed in source view')\n")], None, cx);
            })
        });

        // Save without toggling back to the cell view.
        let saved = cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.sync_from_source_view(window, cx);
                editor.to_notebook(cx)
            })
        });
        let sources: Vec<String> = saved
            .cells
            .iter()
            .map(|cell| match cell {
                nbformat::v4::Cell::Code { source, .. } => source.concat(),
                nbformat::v4::Cell::Markdown { source, .. } => source.concat(),
                nbformat::v4::Cell::Raw { source, .. } => source.concat(),
            })
            .collect();

        assert!(
            sources.iter().any(|s| s.contains("typed in source view")),
            "text typed in the source view must survive a save from that view: {sources:?}"
        );
    }

    /// Cell-level mutations must be inert in the source view, for the same reason moves
    /// are: they act on the selected cell index, which is unrelated to the cursor.
    ///
    /// `backspace` is the alarming one. It is bound to `DeleteCell` in command mode, and
    /// entering the source view did not change the mode, so a backspace while typing
    /// deleted a cell.
    #[gpui::test]
    async fn test_cell_mutations_are_inert_in_the_source_view(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_HEADINGS, cx).await;

        let before = cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.toggle_source_view(&ToggleSourceView, window, cx);
                editor.cell_order.len()
            })
        });

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.delete_cell(window, cx);
                editor.add_code_block(window, cx);
                editor.add_markdown_block(window, cx);
            })
        });

        cx.update(|_window, cx| {
            assert_eq!(
                editor.read(cx).cell_order.len(),
                before,
                "no cell should be added or removed while the source view is showing"
            );
        });
    }

    /// Entering the source view leaves command mode, so its bindings stop applying.
    #[gpui::test]
    async fn test_source_view_is_not_in_command_mode(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_HEADINGS, cx).await;

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.toggle_source_view(&ToggleSourceView, window, cx);
                assert_eq!(
                    editor.notebook_mode,
                    NotebookMode::Edit,
                    "the source view is a text buffer, so command mode must not apply"
                );
            })
        });
    }

    /// The diff has to reach the source editor as well as the cells. The source view is
    /// a full editor with a gutter, so it is the one place hunks render directly.
    #[gpui::test]
    async fn test_diff_reaches_the_source_editor(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;

        let diff = cx.update(|_window, cx| {
            editor.update(cx, |editor, cx| {
                let snapshot = editor.shadow.buffer().read(cx).text_snapshot();
                let diff = cx.new(|cx| BufferDiff::new(&snapshot, None, None, cx));
                editor.notebook_diff = Some(diff.clone());
                diff
            })
        });

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.toggle_source_view(&ToggleSourceView, window, cx)
            })
        });

        cx.update(|_window, cx| {
            let notebook = editor.read(cx);
            let buffer_id = notebook.shadow.buffer().read(cx).remote_id();
            let source_editor = notebook
                .source_editor
                .as_ref()
                .expect("source editor should exist");
            assert!(
                source_editor
                    .read(cx)
                    .buffer()
                    .read(cx)
                    .diff_for(buffer_id)
                    .is_some(),
                "the source view must carry the notebook diff, or its gutter stays empty"
            );
            let _ = diff;
        });
    }

    /// Editing a cell has to produce a hunk and mark that cell.
    ///
    /// The diff used to be computed once, when the notebook opened and nothing had
    /// changed yet, and never recomputed. So it always held zero hunks and no marker
    /// ever appeared, in the cell margin or the source view gutter.
    #[gpui::test]
    async fn test_editing_a_cell_produces_a_diff_marker(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;

        // Stand in for the committed version: the notebook as it is now.
        let base = projection_of_notebook(NOTEBOOK_WITH_TWO_CODE_CELLS, "#")
            .expect("should project");

        let (diff, buffer) = cx.update(|_window, cx| {
            editor.update(cx, |editor, cx| {
                let buffer = editor.shadow.buffer().clone();
                let snapshot = buffer.read(cx).text_snapshot();
                let diff = cx.new(|cx| BufferDiff::new(&snapshot, None, None, cx));
                editor.notebook_diff = Some(diff.clone());
                editor.notebook_diff_base = Some(base.clone());
                editor.attach_diff_to_cells(&diff, cx);
                (diff, buffer)
            })
        });

        // Unchanged: no hunks, no marked cells.
        let task = cx.update(|_window, cx| {
            diff.update(cx, |diff, cx| {
                let snapshot = buffer.read(cx).text_snapshot();
                diff.set_base_text(Some(base.clone().into()), snapshot, cx)
            })
        });
        task.await;
        cx.update(|_window, cx| {
            editor.update(cx, |editor, cx| editor.refresh_cell_change_markers(cx))
        });
        cx.update(|_window, cx| {
            let notebook = editor.read(cx);
            let marked = notebook
                .cell_order
                .iter()
                .filter(|id| match notebook.cell_map.get(id) {
                    Some(Cell::Code(cell)) => cell.read(cx).has_changes(),
                    _ => false,
                })
                .count();
            assert_eq!(marked, 0, "an unchanged notebook marks no cells");
        });

        // Now edit the second cell.
        let range = cx.update(|_window, cx| {
            let notebook = editor.read(cx);
            let cell_id = notebook.cell_order[1].clone();
            notebook.shadow.source_range(&cell_id, cx)
        });
        let Some(range) = range else {
            panic!("second cell should have a range");
        };
        cx.update(|_window, cx| {
            buffer.update(cx, |buffer, cx| {
                buffer.edit([(range, "pd.Series()  # edited")], None, cx);
            })
        });

        let task = cx.update(|_window, cx| {
            diff.update(cx, |diff, cx| {
                let snapshot = buffer.read(cx).text_snapshot();
                diff.set_base_text(Some(base.into()), snapshot, cx)
            })
        });
        task.await;
        cx.update(|_window, cx| {
            editor.update(cx, |editor, cx| editor.refresh_cell_change_markers(cx))
        });

        cx.update(|_window, cx| {
            let notebook = editor.read(cx);
            let marked: Vec<String> = notebook
                .cell_order
                .iter()
                .filter(|id| match notebook.cell_map.get(id) {
                    Some(Cell::Code(cell)) => cell.read(cx).has_changes(),
                    _ => false,
                })
                .map(|id| id.to_string())
                .collect();
            assert_eq!(
                marked,
                vec!["cell-two".to_string()],
                "only the edited cell should be marked"
            );
        });
    }

    /// Running a cell before the kernel is ready must queue, not fail.
    ///
    /// Opening a notebook and pressing run is the first thing anyone does, and the
    /// kernel takes seconds to start, so refusing produced "the kernel is still
    /// starting" on almost every first attempt.
    #[gpui::test]
    async fn test_running_before_the_kernel_is_ready_queues(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;

        let cell_id = cx.update(|_window, cx| editor.read(cx).cell_order[0].clone());

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                // The kernel has not launched in this test, so it sits in Shutdown.
                // Put it in the starting state the way a real launch does.
                editor.kernel = Kernel::Restarting;
                editor.execute_cell(cell_id.clone(), window, cx);
            })
        });

        cx.update(|_window, cx| {
            let notebook = editor.read(cx);
            assert_eq!(
                notebook.pending_executions,
                vec![cell_id.clone()],
                "the run should be queued while the kernel comes up"
            );

            let Some(Cell::Code(cell)) = notebook.cell_map.get(&cell_id) else {
                panic!("expected a code cell");
            };
            assert!(
                !cell.read(cx).has_outputs(),
                "queueing must not push an error output onto the cell"
            );
        });
    }

    /// Asking twice while it starts should not queue the same cell twice.
    #[gpui::test]
    async fn test_queued_executions_are_not_duplicated(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;
        let cell_id = cx.update(|_window, cx| editor.read(cx).cell_order[0].clone());

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.kernel = Kernel::Restarting;
                editor.execute_cell(cell_id.clone(), window, cx);
                editor.execute_cell(cell_id.clone(), window, cx);
            })
        });

        cx.update(|_window, cx| {
            assert_eq!(
                editor.read(cx).pending_executions.len(),
                1,
                "a cell should only be queued once"
            );
        });
    }

    /// A queued run must surface a launch failure rather than spinning forever.
    ///
    /// Queueing is only an improvement over refusing if the failure still reaches the
    /// cell. Without this the cell sat on "Running..." indefinitely whenever the kernel
    /// failed to start, which is strictly worse than the error it replaced.
    #[gpui::test]
    async fn test_a_failed_launch_reaches_queued_cells(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;
        let cell_id = cx.update(|_window, cx| editor.read(cx).cell_order[0].clone());

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.kernel = Kernel::Restarting;
                editor.execute_cell(cell_id.clone(), window, cx);
                assert_eq!(editor.pending_executions.len(), 1, "queued");

                editor.fail_pending_executions("the kernel failed to launch", window, cx);
            })
        });

        cx.update(|_window, cx| {
            let notebook = editor.read(cx);
            assert!(
                notebook.pending_executions.is_empty(),
                "the queue should be drained"
            );

            let Some(Cell::Code(cell)) = notebook.cell_map.get(&cell_id) else {
                panic!("expected a code cell");
            };
            let cell = cell.read(cx);
            assert!(
                cell.has_outputs(),
                "the cell should show the failure instead of spinning"
            );
            assert!(
                !cell.is_executing(),
                "and should no longer report itself as running"
            );
        });
    }

    /// The stop control has to cancel a queued run.
    ///
    /// Interrupting the kernel cannot stop a cell that is not on the kernel yet, so a
    /// cell waiting for startup sat on "Running..." with a stop button that did nothing.
    #[gpui::test]
    async fn test_stop_cancels_a_queued_run(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;
        let cell_id = cx.update(|_window, cx| editor.read(cx).cell_order[0].clone());

        cx.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.kernel = Kernel::Restarting;
                editor.execute_cell(cell_id.clone(), window, cx);
                editor.interrupt_kernel(&InterruptKernel, window, cx);
            })
        });

        cx.update(|_window, cx| {
            let notebook = editor.read(cx);
            assert!(
                notebook.pending_executions.is_empty(),
                "stopping should clear the queue"
            );
            let Some(Cell::Code(cell)) = notebook.cell_map.get(&cell_id) else {
                panic!("expected a code cell");
            };
            assert!(
                !cell.read(cx).is_executing(),
                "and the cell should stop reporting itself as running"
            );
        });
    }

    /// A notebook must not be splittable until two views can share state.
    ///
    /// Each `NotebookEditor` owns its shared buffer and its cell entities, built from
    /// the notebook as parsed from disk, so a split produced two panes that diverged on
    /// the first edit and then raced to save over each other.
    #[gpui::test]
    async fn test_notebooks_cannot_be_split(cx: &mut TestAppContext) {
        let (editor, cx) = open_notebook(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;

        cx.update(|_window, cx| {
            assert!(
                !Item::can_split(editor.read(cx)),
                "splitting would silently diverge the two panes"
            );
        });

        let clone = cx
            .update(|window, cx| {
                editor.update(cx, |editor, cx| Item::clone_on_split(editor, None, window, cx))
            })
            .await;

        assert!(clone.is_none(), "and no second view should be produced");
    }
}
