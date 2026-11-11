//! Lathe-only APIs for extensions.
//!
//! Use this crate together with `zed_extension_api`. An extension that uses it
//! only loads in Lathe, so set `requires = ["lathe"]` in its `extension.toml`.
//!
//! ```ignore
//! use zed_extension_api as zed;
//!
//! struct MyExtension;
//!
//! impl zed::Extension for MyExtension {
//!     fn new() -> Self {
//!         lathe_extension_api::register_command("say-hello", "Say Hello", || Ok(()))
//!             .expect("failed to register command");
//!         MyExtension
//!     }
//! }
//!
//! zed::register_extension!(MyExtension);
//! ```

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// The result type returned by command handlers.
pub type Result<T, E = String> = core::result::Result<T, E>;

type CommandHandler = Rc<dyn Fn() -> Result<()>>;
type EventHandler = Rc<dyn Fn(BufferEvent) -> Result<()>>;
type ViewEventHandler = Rc<dyn Fn(&str, &str, &ViewEvent) -> Result<()>>;
type SurfaceEventHandler = Rc<dyn Fn(&SurfaceEvent) -> Result<()>>;
type FileEventHandler = Rc<dyn Fn(FileEvent, &FileRef) -> Result<()>>;
type RowsHandler = Rc<dyn Fn(&str, &str, u32, u32) -> Result<Vec<ListItemNode>>>;

/// Tells Lathe which version of this API the extension was built against.
#[cfg(target_arch = "wasm32")]
#[unsafe(link_section = "lathe:api-version")]
#[doc(hidden)]
pub static LATHE_API_VERSION: [u8; 6] = *include_bytes!(concat!(env!("OUT_DIR"), "/version_bytes"));

thread_local! {
    static COMMAND_HANDLERS: RefCell<HashMap<String, CommandHandler>> = RefCell::new(HashMap::new());
    static EVENT_HANDLER: RefCell<Option<EventHandler>> = const { RefCell::new(None) };
    static VIEW_EVENT_HANDLER: RefCell<Option<ViewEventHandler>> = const { RefCell::new(None) };
    static SURFACE_EVENT_HANDLER: RefCell<Option<SurfaceEventHandler>> = const { RefCell::new(None) };
    static ROWS_HANDLER: RefCell<Option<RowsHandler>> = const { RefCell::new(None) };
    static FILE_EVENT_HANDLER: RefCell<Option<FileEventHandler>> = const { RefCell::new(None) };
}

pub use wit::exports::lathe::extension0_2_0::file_handler::{FileEvent, FileRef};
pub use wit::lathe::extension0_1_0::events::BufferEvent;
pub use wit::lathe::extension0_2_0::decorations::{Decoration, DecorationStyle};

/// Sets the handler for files coming and going: `TabOpened` and `TabClosed`
/// follow the editor tabs the user has open; `BufferClosed` is the other end
/// of [`BufferEvent::Opened`], which follows files Lathe has loaded, tab or
/// not. During `TabOpened`, [`active_buffer`] returns the file. Needs
/// `events:open`. A later call replaces it.
pub fn on_file_event(handler: impl Fn(FileEvent, &FileRef) -> Result<()> + 'static) {
    FILE_EVENT_HANDLER.with(|slot| *slot.borrow_mut() = Some(Rc::new(handler)));
}
pub use wit::lathe::extension0_2_0::surfaces::{
    DockSide, PanelSpec, PreviewFile, PreviewSpec, StatusItem, SurfaceEvent,
};
pub use wit::lathe::extension0_2_0::views::{
    BadgeNode, ButtonNode, CheckboxNode, Disclosure, ListItemNode, Location, MenuEntry, Node,
    NodeKind, ProgressNode, SelectNode, SelectOption, TextInputNode, TextNode, TextStyle, Tone,
    View, ViewEvent, VirtualListNode,
};

/// Sets the handler that supplies rows of virtual lists. It receives the view
/// id, the list id, the first row and how many rows to return (at most 100),
/// and returns list items; fewer only at the end of the list.
pub fn on_provide_rows(
    handler: impl Fn(&str, &str, u32, u32) -> Result<Vec<ListItemNode>> + 'static,
) {
    ROWS_HANDLER.with(|slot| *slot.borrow_mut() = Some(Rc::new(handler)));
}

/// A decoration over byte offsets `range` of [`ActiveBuffer::text`], for
/// [`ActiveBuffer::set_decorations`].
pub fn decoration(
    range: std::ops::Range<usize>,
    style: DecorationStyle,
    tone: Tone,
    message: Option<&str>,
) -> Decoration {
    Decoration {
        range: byte_range(range),
        style,
        tone,
        message: message.map(str::to_string),
    }
}

/// A plain list item row for [`on_provide_rows`].
pub fn list_row(id: &str, label: &str, detail: Option<&str>) -> ListItemNode {
    ListItemNode {
        id: id.to_string(),
        label: label.to_string(),
        detail: detail.map(str::to_string),
        icon: None,
        tooltip: None,
        selected: false,
        disabled: false,
        indent: 0,
        disclosure: Disclosure::None,
        menu: Vec::new(),
    }
}

/// Opens a file and, if `line` is set, moves the cursor there (1-based line
/// and byte column). Only works while handling something the user did: a
/// command, a view event, or a status item click.
pub fn open_location(path: &str, line: Option<u32>, column: Option<u32>) -> Result<()> {
    wit::lathe::extension0_2_0::views::open_location(&Location {
        path: path.to_string(),
        line,
        column,
    })
}

/// Adds a dockable panel showing the view with the same id. Call from
/// `zed::Extension::new`. The panel appears while the user allows `ui:views`.
pub fn register_panel(id: &str, title: &str, icon: Option<&str>, dock: DockSide) -> Result<()> {
    wit::lathe::extension0_2_0::surfaces::register_panel(&PanelSpec {
        id: id.to_string(),
        title: title.to_string(),
        icon: icon.map(str::to_string),
        dock,
    })
}

/// Adds a preview for files whose names end with one of `file_suffixes`. Users
/// open it from the command palette as "<Extension Name>: <title>", and it
/// shows in a tab beside the file's editor. Each opened preview arrives as
/// [`SurfaceEvent::PreviewOpened`]; during that event [`active_buffer`]
/// returns the file. Call from `zed::Extension::new`. Takes effect while the
/// user allows `ui:views`.
pub fn register_preview(id: &str, title: &str, file_suffixes: &[&str]) -> Result<()> {
    wit::lathe::extension0_2_0::surfaces::register_preview(&PreviewSpec {
        id: id.to_string(),
        title: title.to_string(),
        file_suffixes: file_suffixes
            .iter()
            .map(|suffix| suffix.to_string())
            .collect(),
    })
}

/// Shows a clickable item in the status bar, or hides it with `None`. Clicks
/// arrive as [`SurfaceEvent::StatusItemClicked`]. Needs `ui:status`.
pub fn set_status_item(item: Option<&StatusItem>) -> Result<()> {
    wit::lathe::extension0_2_0::surfaces::set_status_item(item)
}

/// Sets the handler for changes to where this extension's UI appears: views
/// shown and hidden, status item clicks, previews opened and closed. A later
/// call replaces it.
pub fn on_surface_event(handler: impl Fn(&SurfaceEvent) -> Result<()> + 'static) {
    SURFACE_EVENT_HANDLER.with(|slot| *slot.borrow_mut() = Some(Rc::new(handler)));
}

/// Replaces the view with this id. Needs the `ui:views` capability.
///
/// Build `view` with [`ViewBuilder`] rather than by hand: it keeps node 0 as
/// the root and fills in child indices.
pub fn update_view(view_id: &str, view: &View) -> Result<()> {
    wit::lathe::extension0_2_0::views::update_view(view_id, view)
}

/// Forgets the view with this id. Removing one that doesn't exist is fine.
pub fn remove_view(view_id: &str) -> Result<()> {
    wit::lathe::extension0_2_0::views::remove_view(view_id)
}

/// Sets the handler for what users do in this extension's views. It receives
/// the view id, the element's id, and what happened. A later call replaces it.
pub fn on_view_event(handler: impl Fn(&str, &str, &ViewEvent) -> Result<()> + 'static) {
    VIEW_EVENT_HANDLER.with(|slot| *slot.borrow_mut() = Some(Rc::new(handler)));
}

/// Builds a [`View`]'s flat node list from nested calls.
///
/// ```ignore
/// let mut view = ViewBuilder::column();
/// view.text("Recent files", TextStyle::Heading);
/// view.list_item("open:README.md", "README.md", None);
/// lathe_extension_api::update_view("recent", &view.build())?;
/// ```
pub struct ViewBuilder {
    nodes: Vec<Node>,
    /// Index of the container new children are added to.
    current: u32,
}

impl ViewBuilder {
    fn new(root: NodeKind) -> Self {
        Self {
            nodes: vec![Node {
                kind: root,
                children: Vec::new(),
            }],
            current: 0,
        }
    }

    /// A view whose root lays children out top to bottom.
    pub fn column() -> Self {
        Self::new(NodeKind::Column)
    }

    /// A view whose root lays children out left to right.
    pub fn row() -> Self {
        Self::new(NodeKind::Row)
    }

    /// A view whose root is a column that scrolls when it overflows.
    pub fn scroll() -> Self {
        Self::new(NodeKind::Scroll)
    }

    /// Adds any node. The other methods are shorthands for this.
    pub fn push(&mut self, kind: NodeKind) -> u32 {
        let index = self.nodes.len() as u32;
        self.nodes.push(Node {
            kind,
            children: Vec::new(),
        });
        let parent = self.current as usize;
        self.nodes[parent].children.push(index);
        index
    }

    pub fn text(&mut self, text: &str, style: TextStyle) -> &mut Self {
        self.push(NodeKind::Text(TextNode {
            text: text.to_string(),
            style,
            tone: Tone::Neutral,
        }));
        self
    }

    /// Text in a theme color, e.g. an error message.
    pub fn toned_text(&mut self, text: &str, style: TextStyle, tone: Tone) -> &mut Self {
        self.push(NodeKind::Text(TextNode {
            text: text.to_string(),
            style,
            tone,
        }));
        self
    }

    pub fn button(&mut self, id: &str, label: &str, icon: Option<&str>) -> &mut Self {
        self.push(NodeKind::Button(ButtonNode {
            id: id.to_string(),
            label: label.to_string(),
            icon: icon.map(str::to_string),
            tooltip: None,
            disabled: false,
        }));
        self
    }

    pub fn list_item(&mut self, id: &str, label: &str, detail: Option<&str>) -> &mut Self {
        self.push(NodeKind::ListItem(ListItemNode {
            id: id.to_string(),
            label: label.to_string(),
            detail: detail.map(str::to_string),
            icon: None,
            tooltip: None,
            selected: false,
            disabled: false,
            indent: 0,
            disclosure: Disclosure::None,
            menu: Vec::new(),
        }));
        self
    }

    /// A list item for tree views: `indent` is its nesting level and
    /// `disclosure` whether it shows an expand arrow.
    pub fn tree_item(
        &mut self,
        id: &str,
        label: &str,
        indent: u8,
        disclosure: Disclosure,
        selected: bool,
    ) -> &mut Self {
        self.push(NodeKind::ListItem(ListItemNode {
            id: id.to_string(),
            label: label.to_string(),
            detail: None,
            icon: None,
            tooltip: None,
            selected,
            disabled: false,
            indent,
            disclosure,
            menu: Vec::new(),
        }));
        self
    }

    pub fn text_input(&mut self, id: &str, value: &str, placeholder: Option<&str>) -> &mut Self {
        self.push(NodeKind::TextInput(TextInputNode {
            id: id.to_string(),
            value: value.to_string(),
            placeholder: placeholder.map(str::to_string),
            disabled: false,
        }));
        self
    }

    pub fn checkbox(&mut self, id: &str, label: &str, checked: bool) -> &mut Self {
        self.push(NodeKind::Checkbox(CheckboxNode {
            id: id.to_string(),
            label: label.to_string(),
            checked,
            disabled: false,
        }));
        self
    }

    pub fn badge(&mut self, text: &str, tone: Tone) -> &mut Self {
        self.push(NodeKind::Badge(BadgeNode {
            text: text.to_string(),
            tone,
        }));
        self
    }

    pub fn icon(&mut self, name: &str) -> &mut Self {
        self.push(NodeKind::Icon(name.to_string()));
        self
    }

    pub fn divider(&mut self) -> &mut Self {
        self.push(NodeKind::Divider);
        self
    }

    pub fn spacer(&mut self) -> &mut Self {
        self.push(NodeKind::Spacer);
        self
    }

    /// Gives the most recently added list item a right-click menu of
    /// `(id, label)` entries. Does nothing if the last node isn't a list item.
    pub fn menu(&mut self, entries: &[(&str, &str)]) -> &mut Self {
        if let Some(Node {
            kind: NodeKind::ListItem(item),
            ..
        }) = self.nodes.last_mut()
        {
            item.menu = entries
                .iter()
                .map(|(id, label)| MenuEntry {
                    id: id.to_string(),
                    label: label.to_string(),
                })
                .collect();
        }
        self
    }

    /// A dropdown of `(value, label)` options.
    pub fn select(
        &mut self,
        id: &str,
        options: &[(&str, &str)],
        selected: Option<&str>,
    ) -> &mut Self {
        self.push(NodeKind::Select(SelectNode {
            id: id.to_string(),
            options: options
                .iter()
                .map(|(value, label)| SelectOption {
                    value: value.to_string(),
                    label: label.to_string(),
                })
                .collect(),
            selected: selected.map(str::to_string),
            placeholder: None,
            disabled: false,
        }));
        self
    }

    /// A list of `row_count` rows that Lathe asks for as they scroll into view
    /// (see [`on_provide_rows`]). Change `revision` to make it ask again.
    pub fn virtual_list(&mut self, id: &str, row_count: u32, revision: u32) -> &mut Self {
        self.push(NodeKind::VirtualList(VirtualListNode {
            id: id.to_string(),
            row_count,
            revision,
        }));
        self
    }

    /// A progress bar from 0.0 to 1.0, or a spinner when `fraction` is `None`.
    pub fn progress(&mut self, fraction: Option<f32>, label: Option<&str>) -> &mut Self {
        self.push(NodeKind::Progress(ProgressNode {
            fraction,
            label: label.map(str::to_string),
        }));
        self
    }

    pub fn markdown(&mut self, markdown: &str) -> &mut Self {
        self.push(NodeKind::Markdown(markdown.to_string()));
        self
    }

    /// Adds a nested row or column and fills it with `contents`.
    pub fn nested(&mut self, row: bool, contents: impl FnOnce(&mut Self)) -> &mut Self {
        self.container(if row { NodeKind::Row } else { NodeKind::Column }, contents)
    }

    /// Adds a nested scrolling column and fills it with `contents`.
    pub fn scrolling(&mut self, contents: impl FnOnce(&mut Self)) -> &mut Self {
        self.container(NodeKind::Scroll, contents)
    }

    fn container(&mut self, kind: NodeKind, contents: impl FnOnce(&mut Self)) -> &mut Self {
        let index = self.push(kind);
        let parent = std::mem::replace(&mut self.current, index);
        contents(self);
        self.current = parent;
        self
    }

    pub fn build(self) -> View {
        View { nodes: self.nodes }
    }
}
pub use wit::lathe::extension0_1_0::tasks::{TaskReveal, TaskSpec};

/// Runs a command as a Lathe task in a terminal tab and returns as soon as it
/// starts. Needs the `tasks:spawn` capability.
pub fn spawn_task(task: &TaskSpec) -> Result<()> {
    wit::lathe::extension0_1_0::tasks::spawn_task(task)
}

/// This extension's section of `"extension_settings": { "<extension id>": ... }`,
/// as JSON. Resolved for the file the running command or event is about, so a
/// trusted project's settings override the user's for its files. No
/// capability needed: an extension can always read its own settings.
pub fn extension_settings_json() -> Result<Option<String>> {
    wit::lathe::extension0_1_0::extension_settings::get()
}
pub use wit::lathe::extension0_1_0::ui::{NotificationLevel, PickItem};

/// Sets the handler for the buffer events this extension declared with the
/// `events:open`, `events:save` and `events:change` capabilities. While it
/// runs, [`active_buffer`] returns the buffer the event is about. Call it from
/// `zed::Extension::new`; a later call replaces the earlier handler.
///
/// Lathe pauses an extension's events after its handler repeatedly takes
/// longer than two seconds, so keep handlers quick.
pub fn on_buffer_event(handler: impl Fn(BufferEvent) -> Result<()> + 'static) {
    EVENT_HANDLER.with(|slot| *slot.borrow_mut() = Some(Rc::new(handler)));
}

/// Sets this extension's text in the status bar, or hides it with `None`.
/// Needs the `ui:status` capability.
pub fn set_status_text(text: Option<&str>, tooltip: Option<&str>) -> Result<()> {
    wit::lathe::extension0_1_0::ui::set_status_text(text, tooltip)
}

/// Shows a notification. Needs the `ui:status` capability.
pub fn show_notification(level: NotificationLevel, message: &str) -> Result<()> {
    wit::lathe::extension0_1_0::ui::show_notification(level, message)
}

/// Asks the user for a line of text. Returns `None` if they dismiss it.
/// Needs the `ui:prompt` capability; only works while a command runs.
pub fn prompt_input(label: &str, placeholder: Option<&str>) -> Result<Option<String>> {
    wit::lathe::extension0_1_0::ui::prompt_input(label, placeholder)
}

/// Lets the user choose one item and returns its `id`, or `None` if they
/// dismiss the list. Needs `ui:prompt`; only works while a command runs.
pub fn quick_pick(label: &str, items: &[PickItem]) -> Result<Option<String>> {
    wit::lathe::extension0_1_0::ui::quick_pick(label, items)
}

/// Registers a command that Lathe shows in the command palette as
/// `<Extension Name>: <title>` and that users can bind to a key with the
/// `extension::RunCommand` action. Call it from `zed::Extension::new`.
pub fn register_command(
    id: &str,
    title: &str,
    handler: impl Fn() -> Result<()> + 'static,
) -> Result<()> {
    wit::lathe::extension0_1_0::commands::register_command(
        &wit::lathe::extension0_1_0::commands::CommandSpec {
            id: id.to_string(),
            title: title.to_string(),
        },
    )?;
    COMMAND_HANDLERS.with(|handlers| {
        handlers
            .borrow_mut()
            .insert(id.to_string(), Rc::new(handler));
    });
    Ok(())
}

/// A snapshot of the buffer in the editor a command was run from.
///
/// Offsets are UTF-8 byte offsets into [`ActiveBuffer::text`]. Once the buffer
/// changes, including through [`ActiveBuffer::edit`], this snapshot is stale:
/// call [`active_buffer`] again before making another change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveBuffer {
    version: u64,
    /// The file's path within its project, if the buffer has a file.
    pub path: Option<String>,
    /// The file's absolute path, for files on this machine.
    pub abs_path: Option<String>,
    /// The absolute path of the project folder that contains the file.
    pub worktree_root: Option<String>,
    pub language: Option<String>,
    pub text: String,
    /// Every selection in the editor; an empty range is a cursor.
    pub selections: Vec<std::ops::Range<usize>>,
}

/// Returns the buffer in the editor the current command was run from, or
/// `None` if it was not run from a single-file editor.
///
/// Needs `lathe_capabilities = ["buffer:read"]` in `extension.toml`.
pub fn active_buffer() -> Result<Option<ActiveBuffer>> {
    let snapshot = wit::lathe::extension0_1_0::buffers::active_buffer()?;
    Ok(snapshot.map(|snapshot| ActiveBuffer {
        version: snapshot.version,
        path: snapshot.path,
        abs_path: snapshot.abs_path,
        worktree_root: snapshot.worktree_root,
        language: snapshot.language,
        text: snapshot.text,
        selections: snapshot
            .selections
            .into_iter()
            .map(|range| range.start as usize..range.end as usize)
            .collect(),
    }))
}

impl ActiveBuffer {
    /// Replaces each range with its text, as one undoable change. Ranges must
    /// not overlap. Needs the `buffer:write` capability.
    /// Replaces this extension's decorations in `layer` for this buffer.
    /// Ranges are byte offsets into [`Self::text`]. An empty list clears the
    /// layer. Needs the `ui:decorations` capability.
    pub fn set_decorations(&self, layer: &str, decorations: &[Decoration]) -> Result<()> {
        wit::lathe::extension0_2_0::decorations::set_decorations(self.version, layer, decorations)
    }

    pub fn edit(
        &self,
        edits: impl IntoIterator<Item = (std::ops::Range<usize>, String)>,
    ) -> Result<()> {
        let edits = edits
            .into_iter()
            .map(
                |(range, text)| wit::lathe::extension0_1_0::buffers::BufferEdit {
                    range: byte_range(range),
                    text,
                },
            )
            .collect::<Vec<_>>();
        wit::lathe::extension0_1_0::buffers::edit_active_buffer(self.version, &edits)
    }

    /// Replaces the editor's selections. Needs the `buffer:write` capability.
    pub fn select(&self, ranges: impl IntoIterator<Item = std::ops::Range<usize>>) -> Result<()> {
        let ranges = ranges.into_iter().map(byte_range).collect::<Vec<_>>();
        wit::lathe::extension0_1_0::buffers::select_in_active_buffer(self.version, &ranges)
    }
}

fn byte_range(range: std::ops::Range<usize>) -> wit::lathe::extension0_1_0::buffers::ByteRange {
    wit::lathe::extension0_1_0::buffers::ByteRange {
        start: range.start as u64,
        end: range.end as u64,
    }
}

mod wit {
    wit_bindgen::generate!({
        path: "./wit/since_v0.2.0",
        world: "lathe:extension/extension@0.2.0",
        // The 0.2 world includes the frozen 0.1 world from `deps/`; generate
        // bindings for those interfaces too.
        generate_all,
    });
}

// Exports only exist in the wasm build. Gating them lets extensions that use
// this crate compile natively, so their own unit tests can run with `cargo test`.
#[cfg(target_arch = "wasm32")]
wit::export!(Component with_types_in wit);

#[cfg(target_arch = "wasm32")]
struct Component;

#[cfg(target_arch = "wasm32")]
impl wit::exports::lathe::extension0_2_0::view_handler::Guest for Component {
    fn on_view_event(view_id: String, element_id: String, event: ViewEvent) -> Result<()> {
        match VIEW_EVENT_HANDLER.with(|slot| slot.borrow().clone()) {
            Some(handler) => handler(&view_id, &element_id, &event),
            None => Ok(()),
        }
    }

    fn provide_rows(
        view_id: String,
        list_id: String,
        start: u32,
        count: u32,
    ) -> Result<Vec<ListItemNode>> {
        match ROWS_HANDLER.with(|slot| slot.borrow().clone()) {
            Some(handler) => handler(&view_id, &list_id, start, count),
            None => Err("this extension has no virtual lists".to_string()),
        }
    }

    fn on_surface_event(event: SurfaceEvent) -> Result<()> {
        match SURFACE_EVENT_HANDLER.with(|slot| slot.borrow().clone()) {
            Some(handler) => handler(&event),
            None => Ok(()),
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl wit::exports::lathe::extension0_2_0::file_handler::Guest for Component {
    fn on_file_event(event: FileEvent, file: FileRef) -> Result<()> {
        match FILE_EVENT_HANDLER.with(|slot| slot.borrow().clone()) {
            Some(handler) => handler(event, &file),
            None => Ok(()),
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl wit::exports::lathe::extension0_1_0::event_handler::Guest for Component {
    fn on_buffer_event(event: BufferEvent) -> Result<()> {
        match EVENT_HANDLER.with(|slot| slot.borrow().clone()) {
            Some(handler) => handler(event),
            None => Ok(()),
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl wit::exports::lathe::extension0_1_0::command_handler::Guest for Component {
    fn run_command(command_id: String) -> Result<()> {
        let handler = COMMAND_HANDLERS
            .with(|handlers| handlers.borrow().get(&command_id).cloned())
            .ok_or_else(|| format!("no handler registered for command {command_id:?}"))?;
        handler()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_view_builder_indices() {
        let mut builder = ViewBuilder::column();
        builder
            .text("Title", TextStyle::Heading)
            .nested(true, |row| {
                row.button("ok", "OK", None)
                    .spacer()
                    .button("cancel", "Cancel", None);
            })
            .divider();
        let view = builder.build();

        assert!(matches!(view.nodes[0].kind, NodeKind::Column));
        assert_eq!(view.nodes[0].children, vec![1, 2, 6]);
        assert!(matches!(view.nodes[2].kind, NodeKind::Row));
        assert_eq!(view.nodes[2].children, vec![3, 4, 5]);
        assert!(matches!(view.nodes[6].kind, NodeKind::Divider));
        assert_eq!(view.nodes.len(), 7);
    }

    #[test]
    fn test_view_builder_scroll_and_inputs() {
        let mut builder = ViewBuilder::column();
        builder
            .text_input("filter", "", Some("Filter"))
            .scrolling(|list| {
                list.tree_item("src", "src", 0, Disclosure::Expanded, false)
                    .tree_item("main", "main.rs", 1, Disclosure::None, true);
            })
            .checkbox("hidden", "Show hidden", false)
            .badge("3", Tone::Warning);
        let view = builder.build();
        assert_eq!(view.nodes[0].children, vec![1, 2, 5, 6]);
        assert!(matches!(view.nodes[2].kind, NodeKind::Scroll));
        assert_eq!(view.nodes[2].children, vec![3, 4]);
        let NodeKind::ListItem(item) = &view.nodes[4].kind else {
            panic!("expected a list item");
        };
        assert_eq!((item.indent, item.selected), (1, true));
    }

    #[test]
    fn test_view_builder_menu_select_progress_markdown() {
        let mut builder = ViewBuilder::column();
        builder
            .list_item("todo", "Fix it", None)
            .menu(&[("done", "Mark Done"), ("open", "Open")])
            .select("sort", &[("name", "Name"), ("size", "Size")], Some("size"))
            .progress(Some(0.5), Some("Indexing"))
            .markdown("# Hi")
            .menu(&[("ignored", "Ignored")]);
        let view = builder.build();
        let NodeKind::ListItem(item) = &view.nodes[1].kind else {
            panic!("expected a list item");
        };
        assert_eq!(item.menu.len(), 2);
        assert_eq!(item.menu[1].id, "open");
        let NodeKind::Select(select) = &view.nodes[2].kind else {
            panic!("expected a select");
        };
        assert_eq!(select.selected.as_deref(), Some("size"));
        assert!(matches!(&view.nodes[4].kind, NodeKind::Markdown(text) if text == "# Hi"));
    }
}
