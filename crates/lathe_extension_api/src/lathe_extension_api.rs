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

/// Tells Lathe which version of this API the extension was built against.
#[cfg(target_arch = "wasm32")]
#[unsafe(link_section = "lathe:api-version")]
#[doc(hidden)]
pub static LATHE_API_VERSION: [u8; 6] = *include_bytes!(concat!(env!("OUT_DIR"), "/version_bytes"));

thread_local! {
    static COMMAND_HANDLERS: RefCell<HashMap<String, CommandHandler>> = RefCell::new(HashMap::new());
    static EVENT_HANDLER: RefCell<Option<EventHandler>> = const { RefCell::new(None) };
}

pub use wit::lathe::extension::events::BufferEvent;
pub use wit::lathe::extension::tasks::{TaskReveal, TaskSpec};

/// Runs a command as a Lathe task in a terminal tab and returns as soon as it
/// starts. Needs the `tasks:spawn` capability.
pub fn spawn_task(task: &TaskSpec) -> Result<()> {
    wit::lathe::extension::tasks::spawn_task(task)
}

/// This extension's section of `"extension_settings": { "<extension id>": ... }`,
/// as JSON. Resolved for the file the running command or event is about, so a
/// trusted project's settings override the user's for its files. No
/// capability needed: an extension can always read its own settings.
pub fn extension_settings_json() -> Result<Option<String>> {
    wit::lathe::extension::extension_settings::get()
}
pub use wit::lathe::extension::ui::{NotificationLevel, PickItem};

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
    wit::lathe::extension::ui::set_status_text(text, tooltip)
}

/// Shows a notification. Needs the `ui:status` capability.
pub fn show_notification(level: NotificationLevel, message: &str) -> Result<()> {
    wit::lathe::extension::ui::show_notification(level, message)
}

/// Asks the user for a line of text. Returns `None` if they dismiss it.
/// Needs the `ui:prompt` capability; only works while a command runs.
pub fn prompt_input(label: &str, placeholder: Option<&str>) -> Result<Option<String>> {
    wit::lathe::extension::ui::prompt_input(label, placeholder)
}

/// Lets the user choose one item and returns its `id`, or `None` if they
/// dismiss the list. Needs `ui:prompt`; only works while a command runs.
pub fn quick_pick(label: &str, items: &[PickItem]) -> Result<Option<String>> {
    wit::lathe::extension::ui::quick_pick(label, items)
}

/// Registers a command that Lathe shows in the command palette as
/// `<Extension Name>: <title>` and that users can bind to a key with the
/// `extension::RunCommand` action. Call it from `zed::Extension::new`.
pub fn register_command(
    id: &str,
    title: &str,
    handler: impl Fn() -> Result<()> + 'static,
) -> Result<()> {
    wit::lathe::extension::commands::register_command(
        &wit::lathe::extension::commands::CommandSpec {
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
    let snapshot = wit::lathe::extension::buffers::active_buffer()?;
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
    pub fn edit(
        &self,
        edits: impl IntoIterator<Item = (std::ops::Range<usize>, String)>,
    ) -> Result<()> {
        let edits = edits
            .into_iter()
            .map(|(range, text)| wit::lathe::extension::buffers::BufferEdit {
                range: byte_range(range),
                text,
            })
            .collect::<Vec<_>>();
        wit::lathe::extension::buffers::edit_active_buffer(self.version, &edits)
    }

    /// Replaces the editor's selections. Needs the `buffer:write` capability.
    pub fn select(&self, ranges: impl IntoIterator<Item = std::ops::Range<usize>>) -> Result<()> {
        let ranges = ranges.into_iter().map(byte_range).collect::<Vec<_>>();
        wit::lathe::extension::buffers::select_in_active_buffer(self.version, &ranges)
    }
}

fn byte_range(range: std::ops::Range<usize>) -> wit::lathe::extension::buffers::ByteRange {
    wit::lathe::extension::buffers::ByteRange {
        start: range.start as u64,
        end: range.end as u64,
    }
}

mod wit {
    wit_bindgen::generate!({
        path: "./wit/since_v0.1.0",
        world: "lathe:extension/extension@0.1.0",
    });
}

// Exports only exist in the wasm build. Gating them lets extensions that use
// this crate compile natively, so their own unit tests can run with `cargo test`.
#[cfg(target_arch = "wasm32")]
wit::export!(Component with_types_in wit);

#[cfg(target_arch = "wasm32")]
struct Component;

#[cfg(target_arch = "wasm32")]
impl wit::exports::lathe::extension::event_handler::Guest for Component {
    fn on_buffer_event(event: BufferEvent) -> Result<()> {
        match EVENT_HANDLER.with(|slot| slot.borrow().clone()) {
            Some(handler) => handler(event),
            None => Ok(()),
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl wit::exports::lathe::extension::command_handler::Guest for Component {
    fn run_command(command_id: String) -> Result<()> {
        let handler = COMMAND_HANDLERS
            .with(|handlers| handlers.borrow().get(&command_id).cloned())
            .ok_or_else(|| format!("no handler registered for command {command_id:?}"))?;
        handler()
    }
}
